//! H.264 encoding with VideoToolbox, the hardware encoder on Apple silicon and Intel Macs.
//!
//! The session is configured for real-time screen streaming: Constrained Baseline (what OpenH264
//! decodes) and no frame reordering. Each frame is encoded synchronously
//! (`VTCompressionSessionCompleteFrames`), which keeps the capture loop's one-frame-at-a-time
//! backpressure intact. VideoToolbox's low-latency rate control is deliberately not used: on Apple
//! silicon it made each synchronous encode 15–20% slower (about 9.5 ms instead of 8 ms for a
//! 1920×1246 screen), which is the difference between reaching 120 fps and not. The output arrives in AVCC form with the
//! parameter sets in the format description, and is rewritten as Annex-B for the wire.

use std::borrow::Cow;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Mutex;
use std::time::Instant;

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMFormatDescription, CMSampleBuffer, CMTime,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex, kCMSampleAttachmentKey_NotSync,
    kCMTimeInvalid, kCMVideoCodecType_H264,
};
use objc2_core_video::kCVImageBufferYCbCrMatrix_ITU_R_601_4;
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTCompressionPropertyKey_YCbCrMatrix,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel,
};

use super::NativeFrame;
use crate::codec::{CodecError, EncodedFrame, EncoderSettings, avcc_to_annex_b, rgba_to_i420};
use crate::frame::CapturedFrame;

/// Encodes frames with VideoToolbox. The compression session is created for the first frame and
/// recreated when the frame size changes.
pub(crate) struct HardwareEncoder {
    settings: EncoderSettings,
    session: Option<Session>,
    keyframe_requested: bool,
}

impl std::fmt::Debug for HardwareEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HardwareEncoder")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl HardwareEncoder {
    pub(crate) fn new(settings: EncoderSettings) -> Self {
        Self {
            settings,
            session: None,
            keyframe_requested: false,
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
                converted = NativeFrame::from_i420(&rgba_to_i420(frame))?;
                &converted
            }
        };
        let size = (frame.width(), frame.height());
        let session = match &mut self.session {
            Some(session) if session.size == size => session,
            slot => {
                *slot = None;
                slot.insert(Session::new(size, self.settings)?)
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

/// The timescale of presentation times.
const MICROSECONDS: i32 = 1_000_000;

/// What the output callback produced for the frame being encoded.
type CallbackOutput = Option<Result<(Vec<u8>, bool), CodecError>>;

struct Session {
    compression: CFRetained<VTCompressionSession>,
    /// Written by the output callback. Boxed so its address, the callback's refcon, never moves.
    output: Box<Mutex<CallbackOutput>>,
    size: (u32, u32),
    /// Presentation times are microseconds since this instant: frames only arrive when the
    /// screen changes, and real times let rate control spend the bits saved while it was still.
    started: Instant,
    last_time: i64,
}

impl Session {
    fn new(size: (u32, u32), settings: EncoderSettings) -> Result<Self, CodecError> {
        let output = Box::new(Mutex::new(None));
        let compression = create_session(size, &output)?;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "frame rates are small positive numbers"
        )]
        let frame_rate = CFNumber::new_i32(settings.max_fps.round().max(1.0) as i32);
        let bitrate = CFNumber::new_i32(i32::try_from(settings.bitrate_bps).unwrap_or(i32::MAX));
        // SAFETY: Reading immutable static VideoToolbox and CoreVideo constants.
        let properties: [(&CFString, &CFType); 6] = unsafe {
            [
                (kVTCompressionPropertyKey_RealTime, &**CFBoolean::new(true)),
                (
                    kVTCompressionPropertyKey_ProfileLevel,
                    &**kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel,
                ),
                (
                    kVTCompressionPropertyKey_AllowFrameReordering,
                    &**CFBoolean::new(false),
                ),
                (kVTCompressionPropertyKey_AverageBitRate, &**bitrate),
                (kVTCompressionPropertyKey_ExpectedFrameRate, &**frame_rate),
                (
                    kVTCompressionPropertyKey_YCbCrMatrix,
                    &**kCVImageBufferYCbCrMatrix_ITU_R_601_4,
                ),
            ]
        };
        for (key, value) in properties {
            // SAFETY: Each value has the type its key documents.
            let status = unsafe { VTSessionSetProperty(&compression, key, Some(value)) };
            if status != 0 {
                return Err(CodecError::VideoToolbox {
                    operation: "session configuration",
                    status,
                });
            }
        }
        Ok(Self {
            compression,
            output,
            size,
            started: Instant::now(),
            last_time: -1,
        })
    }

    /// Encodes one frame and waits for it. Returns the Annex-B data and whether it is a
    /// keyframe, or `None` if the encoder dropped the frame.
    fn encode(
        &mut self,
        frame: &NativeFrame,
        force_keyframe: bool,
    ) -> Result<Option<(Vec<u8>, bool)>, CodecError> {
        let elapsed = i64::try_from(self.started.elapsed().as_micros()).unwrap_or(i64::MAX);
        // Presentation times must increase.
        self.last_time = elapsed.max(self.last_time + 1);
        // SAFETY: `CMTimeMake` only builds a value.
        let time = unsafe { CMTime::new(self.last_time, MICROSECONDS) };
        // SAFETY: Reading immutable static VideoToolbox constants.
        let options = force_keyframe.then(|| unsafe {
            CFDictionary::<CFString, CFType>::from_slices(
                &[kVTEncodeFrameOptionKey_ForceKeyFrame],
                &[CFBoolean::new(true)],
            )
        });
        let mut flags = VTEncodeInfoFlags::empty();
        // SAFETY: The pixel buffer and the options dictionary are valid for the call; the output
        // callback only touches `self.output`, which outlives the session.
        let status = unsafe {
            self.compression.encode_frame(
                frame.buffer(),
                time,
                kCMTimeInvalid,
                options.as_deref().map(CFDictionary::as_opaque),
                std::ptr::null_mut(),
                &raw mut flags,
            )
        };
        if status != 0 {
            return Err(CodecError::VideoToolbox {
                operation: "frame encoding",
                status,
            });
        }
        // SAFETY: Waits for the frame just submitted; the callback runs before this returns.
        let status = unsafe { self.compression.complete_frames(time) };
        if status != 0 {
            return Err(CodecError::VideoToolbox {
                operation: "frame completion",
                status,
            });
        }
        let output = self
            .output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        output.transpose()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Invalidating stops callbacks before `output` is freed.
        // SAFETY: The session is valid and is not used afterwards.
        unsafe { self.compression.invalidate() };
    }
}

fn create_session(
    size: (u32, u32),
    output: &Mutex<CallbackOutput>,
) -> Result<CFRetained<VTCompressionSession>, CodecError> {
    let width = i32::try_from(size.0).map_err(|_| unsupported(size))?;
    let height = i32::try_from(size.1).map_err(|_| unsupported(size))?;
    let mut raw = std::ptr::null_mut();
    // SAFETY: Every pointer is valid for the call. The refcon points at `output`, which the
    // caller keeps alive (boxed) until the session is invalidated.
    let status = unsafe {
        VTCompressionSession::create(
            None,
            width,
            height,
            kCMVideoCodecType_H264,
            None,
            None,
            None,
            Some(output_callback),
            std::ptr::from_ref(output).cast_mut().cast(),
            NonNull::from(&mut raw),
        )
    };
    let session = NonNull::new(raw)
        .filter(|_| status == 0)
        .ok_or(CodecError::VideoToolbox {
            operation: "session creation",
            status,
        })?;
    // SAFETY: `VTCompressionSessionCreate` returned a +1 reference.
    Ok(unsafe { CFRetained::from_raw(session) })
}

fn unsupported(size: (u32, u32)) -> CodecError {
    CodecError::UnsupportedDimensions {
        width: size.0,
        height: size.1,
    }
}

/// Receives each compressed frame, on a VideoToolbox thread or within
/// `VTCompressionSessionCompleteFrames`.
unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    _source_frame: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: The refcon is the session's boxed output slot, alive until the session is
    // invalidated.
    let output: &Mutex<CallbackOutput> = unsafe { &*refcon.cast_const().cast() };
    let result = if status != 0 {
        Some(Err(CodecError::VideoToolbox {
            operation: "frame encoding",
            status,
        }))
    } else if flags.contains(VTEncodeInfoFlags::FrameDropped) {
        None
    } else {
        // SAFETY: A successful callback carries a valid sample buffer, or none for a dropped
        // frame.
        unsafe { sample.as_ref() }.map(annex_b)
    };
    *output
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = result;
}

/// Extracts one access unit as Annex-B, with SPS and PPS in front of a keyframe.
fn annex_b(sample: &CMSampleBuffer) -> Result<(Vec<u8>, bool), CodecError> {
    let keyframe = is_keyframe(sample);
    // SAFETY: The sample buffer is valid for the duration of the callback.
    let (data, format) = unsafe { (sample.data_buffer(), sample.format_description()) };
    let data = data.ok_or(CodecError::MalformedOutput("no data buffer"))?;
    let format = format.ok_or(CodecError::MalformedOutput("no format description"))?;
    // SAFETY: The block buffer is valid.
    let length = unsafe { data.data_length() };
    let mut avcc = vec![0u8; length];
    if length > 0 {
        // SAFETY: `avcc` has room for `length` bytes.
        let status = unsafe {
            data.copy_data_bytes(0, length, NonNull::new_unchecked(avcc.as_mut_ptr().cast()))
        };
        if status != 0 {
            return Err(CodecError::VideoToolbox {
                operation: "output copy",
                status,
            });
        }
    }
    let (parameter_sets, length_size) = parameter_sets(&format)?;
    let parameter_sets: Vec<Cow<'_, [u8]>> = if keyframe {
        parameter_sets.into_iter().map(cap_level).collect()
    } else {
        Vec::new()
    };
    let parameter_sets: Vec<&[u8]> = parameter_sets.iter().map(AsRef::as_ref).collect();
    Ok((
        avcc_to_annex_b(&avcc, length_size, &parameter_sets)?,
        keyframe,
    ))
}

/// The highest H.264 level OpenH264 decodes (`level_idc` 52, level 5.2).
const MAX_DECODABLE_LEVEL: u8 = 52;

/// Lowers the level an SPS declares to [`MAX_DECODABLE_LEVEL`]. The automatic level follows the
/// macroblock rate, so 2560×1662 at 144 fps comes out as level 6.0, whose parameter sets OpenH264
/// rejects. The level only states a throughput the decoder must sustain; frame size and
/// reference frames, which it does use, are within 5.2's limits for any stream Dari sends.
fn cap_level(nal: &[u8]) -> Cow<'_, [u8]> {
    const SPS: u8 = 7;
    // NAL header, profile_idc, constraint flags, level_idc: fixed bytes before any emulation
    // prevention could shift them, since profile_idc is never zero.
    match nal {
        [header, _profile, _constraints, level, ..]
            if header & 0x1f == SPS && *level > MAX_DECODABLE_LEVEL =>
        {
            let mut capped = nal.to_vec();
            capped[3] = MAX_DECODABLE_LEVEL;
            Cow::Owned(capped)
        }
        _ => Cow::Borrowed(nal),
    }
}

/// A sample is a keyframe unless its attachments mark it as not a sync sample.
fn is_keyframe(sample: &CMSampleBuffer) -> bool {
    // SAFETY: The sample buffer is valid; not creating the array avoids mutating it.
    let Some(attachments) = (unsafe { sample.sample_attachments_array(false) }) else {
        return true;
    };
    // SAFETY: Sample attachment arrays hold CFDictionary values with CFString keys.
    let attachments: CFRetained<objc2_core_foundation::CFArray<CFDictionary<CFString, CFType>>> =
        unsafe { CFRetained::cast_unchecked(attachments) };
    let Some(first) = attachments.get(0) else {
        return true;
    };
    // SAFETY: Reading an immutable static CoreMedia constant.
    let not_sync = unsafe { kCMSampleAttachmentKey_NotSync };
    first
        .get(not_sync)
        .and_then(|value| value.downcast::<CFBoolean>().ok())
        .is_none_or(|value| !value.as_bool())
}

/// The SPS and PPS of an H.264 format description, and the AVCC NAL length size.
fn parameter_sets(format: &CMFormatDescription) -> Result<(Vec<&[u8]>, usize), CodecError> {
    let mut count = 0usize;
    let mut length_size = 0i32;
    // SAFETY: Only the out pointers for count and length size are passed.
    let status = unsafe {
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            format,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut count,
            &raw mut length_size,
        )
    };
    if status != 0 {
        return Err(CodecError::VideoToolbox {
            operation: "parameter set lookup",
            status,
        });
    }
    let mut sets = Vec::with_capacity(count);
    for index in 0..count {
        let mut pointer = std::ptr::null();
        let mut size = 0usize;
        // SAFETY: Valid out pointers; the returned memory belongs to `format`, which outlives
        // the returned slices.
        let status = unsafe {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                format,
                index,
                &raw mut pointer,
                &raw mut size,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if status != 0 || pointer.is_null() {
            return Err(CodecError::MalformedOutput("parameter set"));
        }
        // SAFETY: VideoToolbox returned `size` readable bytes at `pointer`.
        sets.push(unsafe { std::slice::from_raw_parts(pointer, size) });
    }
    let length_size =
        usize::try_from(length_size).map_err(|_| CodecError::MalformedOutput("NAL length size"))?;
    Ok((sets, length_size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_above_5_2_are_capped() {
        let level_6 = [0x27, 0x42, 0xc0, 0x3c, 0xab, 0x40];
        assert_eq!(*cap_level(&level_6), [0x27, 0x42, 0xc0, 0x34, 0xab, 0x40]);
        let level_5_1 = [0x27, 0x42, 0xc0, 0x33, 0xab, 0x40];
        assert!(matches!(cap_level(&level_5_1), Cow::Borrowed(_)));
        let pps = [0x28, 0xce, 0x3c, 0x80];
        assert!(matches!(cap_level(&pps), Cow::Borrowed(_)));
    }
}
