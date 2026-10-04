//! H.264 encoding with VideoToolbox, the hardware encoder on Apple silicon and Intel Macs.
//!
//! The session is configured for screen streaming: Constrained Baseline (what OpenH264 decodes)
//! and no frame reordering. Real-time mode is off: with it, VideoToolbox lowered its clock after
//! a few seconds to just keep up with the expected frame rate, which stretched each frame from
//! about 4.5 ms to 8–11 ms (1920×1246 on an M5) and capped the stream near 120 fps.
//!
//! Frames are submitted without waiting for the previous one (no
//! `VTCompressionSessionCompleteFrames` per frame), so the hardware works on the next frame while
//! the last one finishes; at 2560×1662, where one frame takes about 8 ms, that is what reaches
//! 144 fps. Each frame's output callback hands it straight to the delivery submitted with it, so
//! an isolated frame (a keystroke on a still screen) leaves as soon as it is encoded rather than
//! when a next frame arrives. VideoToolbox's low-latency rate control is not used: it made each
//! frame 20–70% slower, so 2560×1662 no longer kept up with 144 fps, and on screen content it spent
//! a fifth of the target bitrate. The output arrives in AVCC form with the parameter sets in the
//! format description, and is rewritten as Annex-B for the wire.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Mutex, MutexGuard, PoisonError};
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
use crate::codec::{
    CodecError, EncodedFrame, EncoderSettings, FrameDelivery, avcc_to_annex_b, rgba_to_i420,
};
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

/// A frame the hardware encoder could not take.
pub(crate) struct SubmitError {
    pub(crate) error: CodecError,
    /// The frame's delivery, if it was not consumed, so another encoder can take the frame.
    pub(crate) deliver: Option<FrameDelivery>,
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

    /// Whether a frame already submitted failed to encode. Frames after it are discarded, so the
    /// encoder must be replaced.
    pub(crate) fn has_failed(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| session.outputs.lock().failed)
    }

    /// Starts encoding `frame`; `deliver` receives it from a VideoToolbox thread once it is
    /// encoded.
    pub(crate) fn submit(
        &mut self,
        frame: &CapturedFrame,
        deliver: FrameDelivery,
    ) -> Result<(), SubmitError> {
        let converted;
        let frame = match frame {
            CapturedFrame::Native(frame) => frame,
            CapturedFrame::Rgba(frame) => match NativeFrame::from_i420(&rgba_to_i420(frame)) {
                Ok(frame) => {
                    converted = frame;
                    &converted
                }
                Err(error) => {
                    return Err(SubmitError {
                        error,
                        deliver: Some(deliver),
                    });
                }
            },
        };
        let size = (frame.width(), frame.height());
        let session = match &mut self.session {
            Some(session) if session.outputs.lock().size == size => session,
            slot => {
                // Dropping the old session delivers its last frames before the new size's first.
                *slot = None;
                match Session::new(size, self.settings) {
                    Ok(session) => slot.insert(session),
                    Err(error) => {
                        return Err(SubmitError {
                            error,
                            deliver: Some(deliver),
                        });
                    }
                }
            }
        };
        let force_keyframe = std::mem::take(&mut self.keyframe_requested);
        session.submit(frame, force_keyframe, deliver)
    }

    /// Waits until every submitted frame has been delivered or dropped.
    pub(crate) fn flush(&mut self) {
        if let Some(session) = &mut self.session {
            session.flush();
        }
    }
}

/// The timescale of presentation times.
const MICROSECONDS: i32 = 1_000_000;

/// State shared with the output callback. Boxed so its address, the callback's refcon, never
/// moves.
struct Outputs {
    state: Mutex<OutputState>,
}

struct OutputState {
    size: (u32, u32),
    /// Deliveries of submitted frames, oldest first, each with the id passed to VideoToolbox as
    /// the frame's refcon.
    pending: VecDeque<(usize, FrameDelivery)>,
    /// A frame failed to encode; later frames are discarded rather than sent after the gap.
    failed: bool,
}

impl Outputs {
    fn lock(&self) -> MutexGuard<'_, OutputState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

struct Session {
    compression: CFRetained<VTCompressionSession>,
    outputs: Box<Outputs>,
    /// Presentation times are microseconds since this instant: frames only arrive when the
    /// screen changes, and real times let rate control spend the bits saved while it was still.
    started: Instant,
    last_time: i64,
    next_id: usize,
}

impl Session {
    fn new(size: (u32, u32), settings: EncoderSettings) -> Result<Self, CodecError> {
        let outputs = Box::new(Outputs {
            state: Mutex::new(OutputState {
                size,
                pending: VecDeque::new(),
                failed: false,
            }),
        });
        let compression = create_session(size, &outputs)?;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "frame rates are small positive numbers"
        )]
        let frame_rate = CFNumber::new_i32(settings.max_fps.round().max(1.0) as i32);
        let bitrate = CFNumber::new_i32(i32::try_from(settings.bitrate_bps).unwrap_or(i32::MAX));
        // SAFETY: Reading immutable static VideoToolbox and CoreVideo constants.
        let properties: [(&CFString, &CFType); 6] = unsafe {
            [
                // Real-time mode lowers the encoder's clock after a few seconds to just keep up with
                // the expected frame rate, so each frame took 8–11 ms instead of 4.5 ms. That saves
                // under 0.1 W; `MaximizePowerEfficiency` slows frames down the same way.
                (kVTCompressionPropertyKey_RealTime, &**CFBoolean::new(false)),
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
        let session = Self {
            compression,
            outputs,
            started: Instant::now(),
            last_time: -1,
            next_id: 0,
        };
        for (key, value) in properties {
            // SAFETY: Each value has the type its key documents.
            let status = unsafe { VTSessionSetProperty(&session.compression, key, Some(value)) };
            if status != 0 {
                return Err(CodecError::VideoToolbox {
                    operation: "session configuration",
                    status,
                });
            }
        }
        Ok(session)
    }

    /// Starts encoding one frame without waiting for it.
    fn submit(
        &mut self,
        frame: &NativeFrame,
        force_keyframe: bool,
        deliver: FrameDelivery,
    ) -> Result<(), SubmitError> {
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
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.outputs.lock().pending.push_back((id, deliver));
        let mut flags = VTEncodeInfoFlags::empty();
        // SAFETY: The pixel buffer and the options dictionary are valid for the call. The frame
        // refcon is an id, never dereferenced; the output callback only touches `self.outputs`,
        // which outlives the session.
        let status = unsafe {
            self.compression.encode_frame(
                frame.buffer(),
                time,
                kCMTimeInvalid,
                options.as_deref().map(CFDictionary::as_opaque),
                std::ptr::without_provenance_mut(id),
                &raw mut flags,
            )
        };
        if status != 0 {
            let mut outputs = self.outputs.lock();
            let deliver = outputs
                .pending
                .iter()
                .position(|(pending, _)| *pending == id)
                .and_then(|index| outputs.pending.remove(index))
                .map(|(_, deliver)| deliver);
            return Err(SubmitError {
                error: CodecError::VideoToolbox {
                    operation: "frame encoding",
                    status,
                },
                deliver,
            });
        }
        Ok(())
    }

    /// Waits for every submitted frame. Deliveries VideoToolbox never answered are dropped, so
    /// no caller waits on them forever.
    fn flush(&mut self) {
        // SAFETY: The session is valid; the callbacks this runs only touch `self.outputs`.
        let status = unsafe { self.compression.complete_frames(kCMTimeInvalid) };
        if status != 0 {
            tracing::debug!(status, "VideoToolbox frame completion failed");
        }
        let abandoned = std::mem::take(&mut self.outputs.lock().pending);
        drop(abandoned);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Frames still in flight are delivered first, in order.
        self.flush();
        // Invalidating stops callbacks before `outputs` is freed.
        // SAFETY: The session is valid and is not used afterwards.
        unsafe { self.compression.invalidate() };
    }
}

fn create_session(
    size: (u32, u32),
    outputs: &Outputs,
) -> Result<CFRetained<VTCompressionSession>, CodecError> {
    let width = i32::try_from(size.0).map_err(|_| unsupported(size))?;
    let height = i32::try_from(size.1).map_err(|_| unsupported(size))?;
    let mut raw = std::ptr::null_mut();
    // SAFETY: Every pointer is valid for the call. The refcon points at `outputs`, which the
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
            std::ptr::from_ref(outputs).cast_mut().cast(),
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

/// Receives each compressed frame, in submission order, on a VideoToolbox thread or within
/// `VTCompressionSessionCompleteFrames`, and hands it to the frame's delivery.
unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    source_frame: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: The refcon is the session's boxed outputs, alive until the session is invalidated.
    let outputs: &Outputs = unsafe { &*refcon.cast_const().cast() };
    let id = source_frame.addr();
    // Delivering under the lock keeps frames in order even if callbacks were to overlap.
    let mut state = outputs.lock();
    let Some(index) = state.pending.iter().position(|(pending, _)| *pending == id) else {
        return;
    };
    let Some((_, deliver)) = state.pending.remove(index) else {
        return;
    };
    if state.failed {
        return;
    }
    let encoded = if status != 0 {
        Err(CodecError::VideoToolbox {
            operation: "frame encoding",
            status,
        })
    } else if flags.contains(VTEncodeInfoFlags::FrameDropped) {
        // The encoder skipped the frame itself, so the next one does not reference it.
        return;
    } else {
        // SAFETY: A successful callback carries a valid sample buffer, or none for a dropped
        // frame.
        let Some(sample) = (unsafe { sample.as_ref() }) else {
            return;
        };
        annex_b(sample)
    };
    match encoded {
        Ok((data, keyframe)) => deliver(EncodedFrame {
            width: state.size.0,
            height: state.size.1,
            keyframe,
            data,
        }),
        Err(error) => {
            // A missing frame would break the reference chain of every frame after it.
            tracing::warn!(%error, "hardware encoding failed");
            state.failed = true;
        }
    }
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
