//! H.264 encoding with a Media Foundation hardware encoder (Intel Quick Sync, NVIDIA NVENC, AMD
//! AMF, Qualcomm, or whatever the GPU's driver registers).
//!
//! The encoder is found by what it does rather than by vendor: Media Foundation lists the
//! hardware transforms that turn NV12 into H.264 on the adapter the frames come from. It is set
//! up for real-time screen streaming: low-latency mode, Constrained Baseline (what OpenH264
//! decodes), no B-frames, constant bitrate, and BT.601 limited-range color. Frames reach it as
//! the capture's Direct3D 11 textures, without a copy.
//!
//! Hardware transforms are asynchronous: they ask for input and announce output through events.
//! Each frame is still encoded one at a time (submit it, then wait for its output), which keeps
//! the capture loop's one-frame-at-a-time backpressure intact.

use std::mem::ManuallyDrop;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tracing::{debug, warn};
use windows::Win32::Foundation::{E_NOTIMPL, VARIANT_TRUE};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMF2DBuffer,
    IMFActivate, IMFAsyncCallback, IMFAttributes, IMFDXGIDeviceManager, IMFMediaBuffer,
    IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFShutdown, IMFTransform,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_AVG_BITRATE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_YUV_MATRIX, MF_SA_D3D11_AWARE, MF_TRANSFORM_ASYNC,
    MF_TRANSFORM_ASYNC_UNLOCK, MFCreateAttributes, MFCreateDXGIDeviceManager,
    MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Video, MFNominalRange_16_235, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_ADAPTER_LUID,
    MFT_ENUM_FLAG, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFTEnum2, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFVideoTransferMatrix_BT601,
    eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile, eAVEncH264VProfile_Base,
    eAVEncH264VProfile_ConstrainedBase,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::{
    VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4,
};
use windows::core::{GUID, HRESULT, IUnknown, Interface};

use super::NativeFrame;
use super::device::{create_device, start_media_foundation};
use super::frame::windows_error;
use crate::codec::{
    CodecError, EncodedFrame, EncoderSettings, avcc_to_annex_b, cap_annex_b_levels, rgba_to_i420,
};
use crate::frame::CapturedFrame;

/// How long a hardware encoder may take to ask for a frame or to finish one before it is
/// considered stuck, and the stream falls back to OpenH264.
const EVENT_TIMEOUT: Duration = Duration::from_secs(1);
/// Media Foundation's time unit: 100 ns.
const TICKS_PER_SECOND: i64 = 10_000_000;

/// Which encoder transform a session uses.
#[derive(Debug, Clone, Copy)]
enum Transform {
    /// The first hardware transform on the frames' adapter.
    Hardware,
    /// Media Foundation's own software encoder, which lets the tests run the Media Foundation
    /// path on machines without a hardware encoder.
    #[cfg(test)]
    MicrosoftSoftware,
}

/// Encodes frames with a Media Foundation transform. The transform is created for the first
/// frame and recreated when the frame size or the frames' device changes.
pub(crate) struct HardwareEncoder {
    settings: EncoderSettings,
    transform: Transform,
    session: Option<Session>,
    keyframe_requested: bool,
    /// The device frames that arrive in memory are uploaded to.
    upload_device: Option<ID3D11Device>,
}

impl std::fmt::Debug for HardwareEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HardwareEncoder")
            .field("settings", &self.settings)
            .field("transform", &self.transform)
            .finish_non_exhaustive()
    }
}

impl HardwareEncoder {
    /// Returns `None` when this machine has no hardware H.264 encoder (a VM, a server without a
    /// GPU, a missing driver) or no Media Foundation (Windows N editions).
    pub(crate) fn new(settings: EncoderSettings) -> Option<Self> {
        if let Err(code) = start_media_foundation() {
            debug!(code = code.0, "Media Foundation is not available");
            return None;
        }
        match enumerate(MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER, None) {
            Ok(transforms) if !transforms.is_empty() => {}
            Ok(_) => {
                debug!("no hardware H.264 encoder");
                return None;
            }
            Err(error) => {
                debug!(%error, "could not list hardware H.264 encoders");
                return None;
            }
        }
        Some(Self::with_transform(settings, Transform::Hardware))
    }

    #[cfg(test)]
    pub(crate) fn microsoft_software(settings: EncoderSettings) -> Self {
        start_media_foundation().unwrap();
        Self::with_transform(settings, Transform::MicrosoftSoftware)
    }

    fn with_transform(settings: EncoderSettings, transform: Transform) -> Self {
        Self {
            settings,
            transform,
            session: None,
            keyframe_requested: false,
            upload_device: None,
        }
    }

    pub(crate) fn settings(&self) -> EncoderSettings {
        self.settings
    }

    pub(crate) fn request_keyframe(&mut self) {
        self.keyframe_requested = true;
    }

    pub(crate) fn encode(
        &mut self,
        frame: &CapturedFrame,
    ) -> Result<Option<EncodedFrame>, CodecError> {
        let converted;
        let frame = match frame {
            CapturedFrame::Native(frame) => frame,
            CapturedFrame::Rgba(frame) => {
                let device = match &self.upload_device {
                    Some(device) => device,
                    None => self.upload_device.insert(
                        create_device()
                            .map_err(|error| windows_error("device creation", &error))?,
                    ),
                };
                converted = NativeFrame::from_i420(device, &rgba_to_i420(frame))?;
                &converted
            }
        };
        let device = frame.device()?;
        let size = (frame.width(), frame.height());
        let session = match &mut self.session {
            Some(session) if session.size == size && session.device == device => session,
            slot => {
                *slot = None;
                slot.insert(Session::new(self.transform, device, size, self.settings)?)
            }
        };
        let force_keyframe = std::mem::take(&mut self.keyframe_requested);
        let encoded = session.encode(frame, force_keyframe)?;
        Ok(encoded.map(|(data, keyframe)| EncodedFrame {
            width: size.0,
            height: size.1,
            keyframe,
            data,
        }))
    }
}

/// One configured encoder transform.
struct Session {
    transform: IMFTransform,
    device: ID3D11Device,
    /// Keeps the device manager alive while the transform uses it.
    _manager: Option<IMFDXGIDeviceManager>,
    codec: Option<ICodecAPI>,
    /// The events of an asynchronous (hardware) transform.
    events: Option<EventQueue>,
    /// Frames the asynchronous transform asked for and has not been given yet.
    input_requests: u32,
    /// Whether the transform reads Direct3D 11 textures; otherwise it gets NV12 in memory.
    reads_textures: bool,
    input_stream: u32,
    output_stream: u32,
    /// Whether the transform allocates its own output samples.
    provides_samples: bool,
    output_buffer_size: u32,
    size: (u32, u32),
    frame_duration: i64,
    /// Sample times count from here: frames only arrive when the screen changes, and real
    /// times let rate control spend the bits saved while it was still.
    started: Instant,
    last_time: i64,
    /// Output that arrived after its frame had been returned, sent with the next frame.
    late_output: Vec<u8>,
}

impl Session {
    fn new(
        kind: Transform,
        device: ID3D11Device,
        size: (u32, u32),
        settings: EncoderSettings,
    ) -> Result<Self, CodecError> {
        let transform = create_transform(kind, &device)?;
        let fail = |operation| move |error: windows::core::Error| windows_error(operation, &error);

        // SAFETY: Reading and setting a transform's own attributes.
        let attributes = unsafe { transform.GetAttributes() }.ok();
        let flag = |key: &GUID| {
            attributes
                .as_ref()
                // SAFETY: `key` is a valid GUID for the duration of the call.
                .and_then(|attributes| unsafe { attributes.GetUINT32(key) }.ok())
                == Some(1)
        };
        let asynchronous = flag(&MF_TRANSFORM_ASYNC);
        let reads_textures = flag(&MF_SA_D3D11_AWARE);
        if asynchronous && let Some(attributes) = &attributes {
            // An asynchronous transform refuses every call until it is unlocked.
            // SAFETY: As above.
            unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
                .map_err(fail("encoder unlock"))?;
        }
        let manager = if reads_textures {
            Some(attach_device(&transform, &device)?)
        } else {
            None
        };

        let codec = transform.cast::<ICodecAPI>().ok();
        if let Some(codec) = &codec {
            configure_codec(codec, settings);
        }

        let mut inputs = [0];
        let mut outputs = [0];
        // Most transforms number their single streams 0 and don't implement this.
        // SAFETY: The arrays have room for the one stream each.
        if unsafe { transform.GetStreamIDs(&mut inputs, &mut outputs) }.is_err() {
            (inputs, outputs) = ([0], [0]);
        }
        let (input_stream, output_stream) = (inputs[0], outputs[0]);

        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "frame rates are small positive numbers"
        )]
        let fps = settings.max_fps.round().max(1.0) as u32;
        set_media_types(
            &transform,
            (input_stream, output_stream),
            size,
            fps,
            settings.bitrate_bps,
        )?;

        // SAFETY: Querying the output stream the transform just accepted a type for.
        let info = unsafe { transform.GetOutputStreamInfo(output_stream) }
            .map_err(fail("encoder output info"))?;
        let provides_samples = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                .cast_unsigned()
            != 0;
        let events = if asynchronous {
            let generator: IMFMediaEventGenerator =
                transform.cast().map_err(fail("encoder events"))?;
            Some(EventQueue::new(generator))
        } else {
            None
        };
        // SAFETY: Streaming messages with no parameter.
        com("encoder start", || unsafe {
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
        })?;
        debug!(
            ?kind,
            asynchronous,
            reads_textures,
            ?size,
            "Media Foundation encoder started"
        );
        Ok(Self {
            transform,
            device,
            _manager: manager,
            codec,
            events,
            input_requests: 0,
            reads_textures,
            input_stream,
            output_stream,
            provides_samples,
            output_buffer_size: info.cbSize.max(size.0 * size.1 * 3 / 2),
            size,
            frame_duration: TICKS_PER_SECOND / i64::from(fps),
            started: Instant::now(),
            last_time: -1,
            late_output: Vec::new(),
        })
    }

    /// Encodes one frame and waits for it. Returns the Annex-B data and whether it is a
    /// keyframe, or `None` if the encoder produced nothing for it.
    fn encode(
        &mut self,
        frame: &NativeFrame,
        force_keyframe: bool,
    ) -> Result<Option<(Vec<u8>, bool)>, CodecError> {
        let sample = self.input_sample(frame)?;
        if force_keyframe && let Some(codec) = &self.codec {
            let force = variant_u32(1);
            // SAFETY: The value is a VT_UI4, as this property documents.
            unsafe { codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &raw const force) }
                .map_err(|error| windows_error("keyframe request", &error))?;
        }
        let output = if self.events.is_some() {
            self.encode_async(&sample)?
        } else {
            self.encode_sync(&sample)?
        };
        match output {
            Some(data) => self.annex_b(data).map(Some),
            None => Ok(None),
        }
    }

    fn input_sample(&mut self, frame: &NativeFrame) -> Result<IMFSample, CodecError> {
        let fail = |operation| move |error: windows::core::Error| windows_error(operation, &error);
        let buffer: IMFMediaBuffer = if self.reads_textures {
            // SAFETY: The texture is a valid NV12 texture on the transform's device, which the
            // buffer keeps a reference to.
            let buffer = unsafe {
                MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, frame.texture(), 0, false)
            }
            .map_err(fail("texture buffer"))?;
            // Some encoders reject a buffer whose length was never set.
            if let Ok(planar) = buffer.cast::<IMF2DBuffer>() {
                // SAFETY: Plain queries on a valid buffer.
                com("texture buffer", || unsafe {
                    buffer.SetCurrentLength(planar.GetContiguousLength()?)
                })?;
            }
            buffer
        } else {
            let nv12 = frame.to_nv12()?;
            let length =
                u32::try_from(nv12.len()).map_err(|_| CodecError::UnsupportedDimensions {
                    width: self.size.0,
                    height: self.size.1,
                })?;
            // SAFETY: Creating a buffer of `length` bytes.
            let buffer = unsafe { MFCreateMemoryBuffer(length) }.map_err(fail("memory buffer"))?;
            // SAFETY: Locking gives `length` writable bytes until the matching unlock.
            com("memory buffer", || unsafe {
                let mut data = std::ptr::null_mut();
                buffer.Lock(&raw mut data, None, None)?;
                std::ptr::copy_nonoverlapping(nv12.as_ptr(), data, nv12.len());
                buffer.Unlock()?;
                buffer.SetCurrentLength(length)
            })?;
            buffer
        };
        let elapsed = i64::try_from(self.started.elapsed().as_micros()).unwrap_or(i64::MAX / 10);
        // Sample times must increase.
        self.last_time = (elapsed * 10).max(self.last_time + 1);
        // SAFETY: Building a sample from a valid buffer.
        com("input sample", || unsafe {
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(self.last_time)?;
            sample.SetSampleDuration(self.frame_duration)?;
            Ok(sample)
        })
    }

    /// A synchronous transform takes the frame and hands back its output right away.
    fn encode_sync(&mut self, sample: &IMFSample) -> Result<Option<Vec<u8>>, CodecError> {
        // SAFETY: A valid sample for the configured input stream.
        unsafe { self.transform.ProcessInput(self.input_stream, sample, 0) }
            .map_err(|error| windows_error("frame encoding", &error))?;
        let mut data = Vec::new();
        while let Some(output) = self.process_output()? {
            data.extend_from_slice(&output);
        }
        Ok((!data.is_empty()).then_some(data))
    }

    /// An asynchronous transform asks for the frame with one event and announces its output
    /// with another.
    fn encode_async(&mut self, sample: &IMFSample) -> Result<Option<Vec<u8>>, CodecError> {
        while self.input_requests == 0 {
            match self.next_event()? {
                Event::NeedInput => self.input_requests += 1,
                Event::HaveOutput => self.take_late_output()?,
                Event::Other => {}
            }
        }
        // SAFETY: A valid sample for the configured input stream, which the transform asked for.
        unsafe { self.transform.ProcessInput(self.input_stream, sample, 0) }
            .map_err(|error| windows_error("frame encoding", &error))?;
        self.input_requests -= 1;
        loop {
            match self.next_event()? {
                Event::NeedInput => self.input_requests += 1,
                // An announcement can also be a format change, which carries no data; the frame's
                // output then follows with the next one.
                Event::HaveOutput => {
                    if let Some(output) = self.process_output()? {
                        let mut data = std::mem::take(&mut self.late_output);
                        data.extend_from_slice(&output);
                        return Ok(Some(data));
                    }
                }
                Event::Other => {}
            }
        }
    }

    fn next_event(&mut self) -> Result<Event, CodecError> {
        self.events
            .as_mut()
            .ok_or(CodecError::MalformedOutput("encoder events"))?
            .next(EVENT_TIMEOUT)
    }

    /// Keeps output announced while no frame was waiting for it, for the next frame.
    fn take_late_output(&mut self) -> Result<(), CodecError> {
        if let Some(output) = self.process_output()? {
            warn!(bytes = output.len(), "encoder output arrived late");
            self.late_output.extend_from_slice(&output);
        }
        Ok(())
    }

    /// Collects one output sample, or `None` when the transform needs more input.
    fn process_output(&mut self) -> Result<Option<Vec<u8>>, CodecError> {
        // The transform may change its output type once (for example, to add the parameter
        // sets); it then wants that type confirmed before it hands out data.
        for _attempt in 0..2 {
            let sample = if self.provides_samples {
                None
            } else {
                // SAFETY: Building an empty sample with room for one encoded frame.
                Some(com("output sample", || unsafe {
                    let sample = MFCreateSample()?;
                    sample.AddBuffer(&MFCreateMemoryBuffer(self.output_buffer_size)?)?;
                    Ok(sample)
                })?)
            };
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.output_stream,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0;
            // SAFETY: One output buffer for the configured output stream; the sample in it is
            // ours or the transform's, and both are taken back out below.
            let result = unsafe {
                self.transform
                    .ProcessOutput(0, &mut buffers, &raw mut status)
            };
            let [buffer] = buffers;
            let sample = ManuallyDrop::into_inner(buffer.pSample);
            drop(ManuallyDrop::into_inner(buffer.pEvents));
            match result {
                Ok(()) => return sample.map(|sample| read_sample(&sample)).transpose(),
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // SAFETY: Re-applying a type the transform itself offers.
                    com("encoder output type change", || unsafe {
                        let output = self
                            .transform
                            .GetOutputAvailableType(self.output_stream, 0)?;
                        self.transform.SetOutputType(self.output_stream, &output, 0)
                    })?;
                }
                Err(error) => return Err(windows_error("encoder output", &error)),
            }
        }
        Err(CodecError::MalformedOutput(
            "encoder output type keeps changing",
        ))
    }

    /// Rewrites encoder output as Annex-B, with the parameter sets in front of a keyframe.
    fn annex_b(&self, data: Vec<u8>) -> Result<(Vec<u8>, bool), CodecError> {
        let mut data = if data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1]) {
            data
        } else {
            avcc_to_annex_b(&data, 4, &[])?
        };
        // Encoders pick the level from the macroblock rate, which can exceed what OpenH264
        // decodes at high frame rates.
        cap_annex_b_levels(&mut data);
        let units = nal_unit_types(&data);
        let keyframe = units.contains(&NAL_IDR);
        if !keyframe || units.contains(&NAL_SPS) {
            return Ok((data, keyframe));
        }
        let mut with_parameter_sets = self.sequence_header()?;
        cap_annex_b_levels(&mut with_parameter_sets);
        with_parameter_sets.extend_from_slice(&data);
        Ok((with_parameter_sets, keyframe))
    }

    /// The SPS and PPS the transform keeps on its output type, as Annex-B.
    fn sequence_header(&self) -> Result<Vec<u8>, CodecError> {
        // SAFETY: Reading a blob into a buffer of the size the transform reports.
        let header = com("parameter set lookup", || unsafe {
            let output = self.transform.GetOutputCurrentType(self.output_stream)?;
            let size = output.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER)?;
            let mut header = vec![0; size as usize];
            output.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut header, None)?;
            Ok(header)
        })?;
        if nal_unit_types(&header).contains(&NAL_SPS) {
            Ok(header)
        } else {
            Err(CodecError::MalformedOutput(
                "keyframe without parameter sets",
            ))
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: Streaming messages with no parameter, then shutting the transform down, which
        // also ends a pending event request; the transform is not used afterwards.
        unsafe {
            let _ended = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ended = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _shut_down = shutdown.Shutdown();
            }
        }
    }
}

/// Creates the encoder transform for `device`'s frames.
fn create_transform(kind: Transform, device: &ID3D11Device) -> Result<IMFTransform, CodecError> {
    // Encoders run on the capture thread, which a test may have started without COM.
    start_media_foundation().map_err(|code| CodecError::Windows {
        operation: "Media Foundation startup",
        code: code.0,
    })?;
    match kind {
        Transform::Hardware => activate_hardware(device),
        #[cfg(test)]
        Transform::MicrosoftSoftware => {
            // SAFETY: Creating an in-process COM object by its class ID.
            unsafe {
                windows::Win32::System::Com::CoCreateInstance(
                    &windows::Win32::Media::MediaFoundation::CLSID_MSH264EncoderMFT,
                    None,
                    windows::Win32::System::Com::CLSCTX_INPROC_SERVER,
                )
            }
            .map_err(|error| windows_error("encoder creation", &error))
        }
    }
}

/// Hands the transform `device`, so it reads the capture's textures directly.
fn attach_device(
    transform: &IMFTransform,
    device: &ID3D11Device,
) -> Result<IMFDXGIDeviceManager, CodecError> {
    let mut token = 0;
    let mut manager = None;
    // SAFETY: Valid out pointers.
    com("device manager creation", || unsafe {
        MFCreateDXGIDeviceManager(&raw mut token, &raw mut manager)
    })?;
    let manager = manager.ok_or(CodecError::MalformedOutput("device manager"))?;
    // SAFETY: `token` is the manager's own reset token. The message takes the manager's
    // interface pointer, which the transform keeps its own reference to; the session keeps the
    // manager and the device alive as well.
    com("encoder device setup", || unsafe {
        manager.ResetDevice(device, token)?;
        transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
    })?;
    Ok(manager)
}

/// Sets the output type (H.264, Constrained Baseline if the encoder offers it) and then the
/// input type to match, the order encoders require.
fn set_media_types(
    transform: &IMFTransform,
    (input_stream, output_stream): (u32, u32),
    size: (u32, u32),
    fps: u32,
    bitrate: u32,
) -> Result<(), CodecError> {
    let mut output_set = Err(CodecError::MalformedOutput("output type"));
    // Without FMO, ASO, and redundant slices, which encoders don't use, Baseline is Constrained
    // Baseline; some encoders only know it by the older name.
    for profile in [eAVEncH264VProfile_ConstrainedBase, eAVEncH264VProfile_Base] {
        output_set = video_type(&MFVideoFormat_H264, size, fps, Some((profile, bitrate))).and_then(
            |output| {
                // SAFETY: A complete H.264 media type for the transform's output stream.
                com("encoder output type", || unsafe {
                    transform.SetOutputType(output_stream, &output, 0)
                })
            },
        );
        if output_set.is_ok() {
            break;
        }
    }
    output_set?;
    let input = video_type(&MFVideoFormat_NV12, size, fps, None)?;
    // SAFETY: A complete NV12 media type for the transform's input stream.
    com("encoder input type", || unsafe {
        transform.SetInputType(input_stream, &input, 0)
    })
}

/// Runs a sequence of COM calls, reporting the first failure as `operation`'s.
fn com<T>(
    operation: &'static str,
    calls: impl FnOnce() -> windows::core::Result<T>,
) -> Result<T, CodecError> {
    calls().map_err(|error| windows_error(operation, &error))
}

/// H.264 NAL unit types.
const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;

/// The types of the NAL units in an Annex-B stream. Start codes can't occur inside a unit,
/// which emulation prevention guarantees.
fn nal_unit_types(annex_b: &[u8]) -> Vec<u8> {
    annex_b
        .windows(4)
        .filter(|window| window[..3] == [0, 0, 1])
        .map(|window| window[3] & 0x1F)
        .collect()
}

fn read_sample(sample: &IMFSample) -> Result<Vec<u8>, CodecError> {
    // SAFETY: Locking gives `length` readable bytes until the matching unlock.
    com("encoder output", || unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut data = std::ptr::null_mut();
        let mut length = 0;
        buffer.Lock(&raw mut data, None, Some(&raw mut length))?;
        let copy = if data.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(data, length as usize).to_vec()
        };
        buffer.Unlock()?;
        Ok(copy)
    })
}

/// Sets the encoder's real-time streaming properties. Encoders support different subsets, so
/// each one is best effort; the media types carry the essentials.
fn configure_codec(codec: &ICodecAPI, settings: EncoderSettings) {
    let properties: [(&GUID, &str, VARIANT); 5] = [
        (&CODECAPI_AVLowLatencyMode, "low latency", variant_true()),
        (
            &CODECAPI_AVEncCommonRateControlMode,
            "rate control",
            variant_u32(eAVEncCommonRateControlMode_CBR.0.cast_unsigned()),
        ),
        (
            &CODECAPI_AVEncCommonMeanBitRate,
            "bitrate",
            variant_u32(settings.bitrate_bps),
        ),
        (
            &CODECAPI_AVEncMPVDefaultBPictureCount,
            "B-frames",
            variant_u32(0),
        ),
        // The transport is reliable; keyframes are only needed on start, resize, or request.
        (&CODECAPI_AVEncMPVGOPSize, "GOP size", variant_u32(u32::MAX)),
    ];
    for (key, name, value) in properties {
        // SAFETY: Each value has the type its property documents.
        if let Err(error) = unsafe { codec.SetValue(key, &raw const value) } {
            debug!(%error, property = name, "encoder ignored a property");
        }
    }
}

fn variant(vt: VARENUM, value: VARIANT_0_0_0) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: value,
            }),
        },
    }
}

fn variant_u32(value: u32) -> VARIANT {
    variant(VT_UI4, VARIANT_0_0_0 { ulVal: value })
}

fn variant_true() -> VARIANT {
    variant(
        VT_BOOL,
        VARIANT_0_0_0 {
            boolVal: VARIANT_TRUE,
        },
    )
}

/// An uncompressed (`format` NV12) or H.264 video type of `size` at `fps`.
fn video_type(
    format: &GUID,
    size: (u32, u32),
    fps: u32,
    h264: Option<(eAVEncH264VProfile, u32)>,
) -> Result<IMFMediaType, CodecError> {
    let pack = |high: u32, low: u32| u64::from(high) << 32 | u64::from(low);
    // SAFETY: Creating a media type and setting plain attributes on it.
    com("media type", || unsafe {
        let media_type = MFCreateMediaType()?;
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, format)?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(size.0, size.1))?;
        media_type.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        media_type.SetUINT32(
            &MF_MT_INTERLACE_MODE,
            MFVideoInterlace_Progressive.0.cast_unsigned(),
        )?;
        media_type.SetUINT32(
            &MF_MT_VIDEO_NOMINAL_RANGE,
            MFNominalRange_16_235.0.cast_unsigned(),
        )?;
        media_type.SetUINT32(
            &MF_MT_YUV_MATRIX,
            MFVideoTransferMatrix_BT601.0.cast_unsigned(),
        )?;
        if let Some((profile, bitrate)) = h264 {
            media_type.SetUINT32(&MF_MT_MPEG2_PROFILE, profile.0.cast_unsigned())?;
            media_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
        }
        Ok(media_type)
    })
}

/// Lists H.264 encoder transforms that take NV12, optionally only those of the adapter
/// `attributes` names.
fn enumerate(
    flags: MFT_ENUM_FLAG,
    attributes: Option<&IMFAttributes>,
) -> windows::core::Result<Vec<IMFActivate>> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: The type infos outlive the call, and the out pointers are valid.
    unsafe {
        MFTEnum2(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&raw const input),
            Some(&raw const output),
            attributes,
            &raw mut list,
            &raw mut count,
        )
    }?;
    if list.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: Media Foundation returned `count` activation objects in a CoTaskMem array, which
    // is taken over here and then freed.
    let activates = unsafe {
        let activates = std::slice::from_raw_parts_mut(list, count as usize)
            .iter_mut()
            .filter_map(Option::take)
            .collect();
        CoTaskMemFree(Some(list.cast_const().cast()));
        activates
    };
    Ok(activates)
}

/// Creates the first hardware encoder on `device`'s adapter that starts.
fn activate_hardware(device: &ID3D11Device) -> Result<IMFTransform, CodecError> {
    // SAFETY: Plain queries on a valid device, then creating an attribute store.
    let attributes = com("adapter lookup", || unsafe {
        let luid = device
            .cast::<IDXGIDevice>()?
            .GetAdapter()?
            .GetDesc()?
            .AdapterLuid;
        let mut attributes = None;
        MFCreateAttributes(&raw mut attributes, 1)?;
        let attributes = attributes.ok_or_else(|| windows::core::Error::from(E_NOTIMPL))?;
        let mut blob = luid.LowPart.to_le_bytes().to_vec();
        blob.extend_from_slice(&luid.HighPart.to_le_bytes());
        attributes.SetBlob(&MFT_ENUM_ADAPTER_LUID, &blob)?;
        Ok(attributes)
    })?;
    let candidates = com("encoder lookup", || {
        enumerate(
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&attributes),
        )
    })?;
    for candidate in candidates {
        // SAFETY: Activating an object Media Foundation listed.
        match unsafe { candidate.ActivateObject::<IMFTransform>() } {
            Ok(transform) => return Ok(transform),
            Err(error) => debug!(%error, "a hardware encoder would not start"),
        }
    }
    Err(CodecError::Windows {
        operation: "hardware encoder lookup",
        code: E_NOTIMPL.0,
    })
}

/// What an asynchronous transform announced.
#[derive(Debug)]
enum Event {
    NeedInput,
    HaveOutput,
    Other,
}

/// Waits for an asynchronous transform's events with a timeout, which its blocking `GetEvent`
/// can't do.
struct EventQueue {
    generator: IMFMediaEventGenerator,
    callback: IMFAsyncCallback,
    events: mpsc::Receiver<Result<Event, HRESULT>>,
    /// Whether an event request is outstanding.
    requested: bool,
}

impl EventQueue {
    fn new(generator: IMFMediaEventGenerator) -> Self {
        let (sender, events) = mpsc::channel();
        let callback = callback::EventCallback {
            generator: generator.clone(),
            sender,
        }
        .into();
        Self {
            generator,
            callback,
            events,
            requested: false,
        }
    }

    fn next(&mut self, timeout: Duration) -> Result<Event, CodecError> {
        if !self.requested {
            // SAFETY: The callback stays alive while the request is outstanding (Media
            // Foundation holds a reference to it).
            unsafe {
                self.generator
                    .BeginGetEvent(&self.callback, None::<&IUnknown>)
            }
            .map_err(|error| windows_error("encoder event request", &error))?;
            self.requested = true;
        }
        let event = self
            .events
            .recv_timeout(timeout)
            .map_err(|_| CodecError::Windows {
                operation: "waiting for the encoder",
                code: windows::Win32::Foundation::ERROR_TIMEOUT.to_hresult().0,
            })?;
        self.requested = false;
        event.map_err(|code| CodecError::Windows {
            operation: "frame encoding",
            code: code.0,
        })
    }
}

mod callback {
    //! The callback object `windows::core::implement` generates, kept apart so that the lints
    //! its generated code trips stay here.
    #![allow(
        clippy::inline_always,
        clippy::ref_as_ptr,
        reason = "generated by windows::core::implement"
    )]

    use std::sync::mpsc;

    use windows::Win32::Foundation::{E_FAIL, E_NOTIMPL};
    use windows::Win32::Media::MediaFoundation::{
        IMFAsyncCallback, IMFAsyncCallback_Impl, IMFAsyncResult, IMFMediaEventGenerator, MEError,
        METransformHaveOutput, METransformNeedInput,
    };
    use windows::core::{HRESULT, Ref, implement};

    use super::Event;

    /// Receives one event per request on a Media Foundation thread and passes it on.
    #[implement(IMFAsyncCallback)]
    pub(super) struct EventCallback {
        pub(super) generator: IMFMediaEventGenerator,
        pub(super) sender: mpsc::Sender<Result<Event, HRESULT>>,
    }

    impl IMFAsyncCallback_Impl for EventCallback_Impl {
        fn GetParameters(&self, _flags: *mut u32, _queue: *mut u32) -> windows::core::Result<()> {
            // Use the default work queue.
            Err(E_NOTIMPL.into())
        }

        fn Invoke(&self, result: Ref<IMFAsyncResult>) -> windows::core::Result<()> {
            // SAFETY: `result` is the result of this callback's own request on `generator`.
            let event = unsafe { self.generator.EndGetEvent(result.ok()?) }.and_then(|event| {
                // SAFETY: Plain queries on a valid event.
                let (kind, status) = unsafe { (event.GetType()?, event.GetStatus()?) };
                Ok((kind, status))
            });
            let event = match event {
                Ok((_, status)) if status.is_err() => Err(status),
                Ok((kind, _)) if kind == MEError.0.cast_unsigned() => Err(E_FAIL),
                Ok((kind, _)) if kind == METransformNeedInput.0.cast_unsigned() => {
                    Ok(Event::NeedInput)
                }
                Ok((kind, _)) if kind == METransformHaveOutput.0.cast_unsigned() => {
                    Ok(Event::HaveOutput)
                }
                Ok(_) => Ok(Event::Other),
                Err(error) => Err(error.code()),
            };
            // The receiver is gone once the session ended; nothing waits for the event then.
            let _sent = self.sender.send(event);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{DecodedFrame, VideoDecoder};
    use crate::frame::RgbaFrame;
    use crate::stream::StillRefinement;
    use crate::synthetic::render_text_page;

    fn psnr_y(source: &RgbaFrame, decoded: &DecodedFrame) -> f64 {
        let luma = |r: u8, g: u8, b: u8| {
            0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b)
        };
        let error: f64 = source
            .pixels()
            .as_chunks::<4>()
            .0
            .iter()
            .zip(decoded.bgra.as_chunks::<4>().0)
            .map(|(rgba, bgra)| {
                (luma(rgba[0], rgba[1], rgba[2]) - luma(bgra[2], bgra[1], bgra[0])).powi(2)
            })
            .sum();
        #[expect(clippy::cast_precision_loss, reason = "pixel counts")]
        let mse = error / (source.pixels().len() / 4) as f64;
        10.0 * (255.0f64.powi(2) / mse).log10()
    }

    #[test]
    #[ignore = "a release-mode quality measurement"]
    fn still_screen_refinement_through_media_foundation() {
        let settings = EncoderSettings {
            bitrate_bps: 10_000_000,
            max_fps: 30.0,
            hardware: true,
        };
        let refinement = StillRefinement::default();
        let source = render_text_page(2560, 1440);
        let frame = CapturedFrame::Rgba(source.clone());
        let mut encoder = HardwareEncoder::microsoft_software(settings);
        let mut decoder = VideoDecoder::new().unwrap();
        println!(
            "{:>5} {:>7} {:>4} {:>8} {:>7}",
            "frame", "t_ms", "type", "bytes", "psnr_y"
        );
        let went_still = Instant::now();
        let mut shown = None;
        for sent in 0..=refinement.frames {
            if let Some(due) = sent
                .checked_sub(1)
                .and_then(|sent| refinement.due_after(sent))
            {
                std::thread::sleep((went_still + due).saturating_duration_since(Instant::now()));
            }
            let encoded = encoder.encode(&frame).unwrap();
            if let Some(encoded) = &encoded {
                shown = decoder.decode(&encoded.data).unwrap().or(shown);
            }
            let psnr = shown
                .as_ref()
                .map_or(f64::NAN, |shown| psnr_y(&source, shown));
            println!(
                "{sent:>5} {:>7} {:>4} {:>8} {psnr:>7.2}",
                went_still.elapsed().as_millis(),
                encoded.as_ref().map_or("skip", |encoded| {
                    if encoded.keyframe { "I" } else { "P" }
                }),
                encoded.as_ref().map_or(0, |encoded| encoded.data.len()),
            );
        }
    }

    #[test]
    fn nal_unit_types_are_found_after_either_start_code() {
        let stream = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4,
        ];
        assert_eq!(nal_unit_types(&stream), [NAL_SPS, 8, NAL_IDR]);
        assert_eq!(nal_unit_types(&[0, 0, 2, 0x65]), [0u8; 0]);
    }
}
