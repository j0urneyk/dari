//! Screen capture and video coding for Dari.
//!
//! The host side captures a display ([`DisplayCapturer`]), scales the frame to the stream's size
//! limit, and encodes it as H.264 ([`VideoEncoder`]). On macOS, ScreenCaptureKit captures and
//! scales on the GPU and VideoToolbox encodes in hardware. On Windows, Windows.Graphics.Capture
//! captures, Direct3D 11 scales on the GPU, and a Media Foundation hardware encoder encodes.
//! OpenH264 encodes wherever there is no hardware encoder, and the CPU scales frames that arrive
//! in memory ([`FrameScaler`]). [`spawn_capture_stream`] runs that pipeline on a dedicated
//! thread. The viewer side decodes packets back into BGRA frames ([`VideoDecoder`])
//! ready for GPU upload.
//!
//! System audio follows the same shape: [`spawn_audio_stream`] captures and Opus-encodes it on
//! the host, and an [`AudioPlayer`] decodes and plays it on the viewer.

#[cfg(target_os = "macos")]
#[allow(
    unsafe_code,
    reason = "ScreenCaptureKit, CoreVideo, VideoToolbox, and TCC are C APIs"
)]
mod apple;
mod audio;
mod codec;
mod display;
mod frame;
mod permission;
mod scale;
mod stream;
mod synthetic;
#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "Direct3D 11, Media Foundation, and the capture interop are COM APIs"
)]
mod win;

/// The platform's hardware capture and encoding module.
#[cfg(target_os = "macos")]
use apple as native;
#[cfg(windows)]
use win as native;

pub use audio::{
    AUDIO_CHANNELS, AUDIO_FRAME_SAMPLES, AUDIO_SAMPLE_RATE, AudioCapturer, AudioChunk,
    AudioDecoder, AudioEncoder, AudioError, AudioOutput, AudioOutputFactory, AudioPlayer,
    AudioStream, MAX_AUDIO_PACKET_BYTES, PlaybackBuffer, SyntheticAudioCapturer,
    SystemAudioCapturer, SystemAudioOutput, spawn_audio_stream,
};
pub use codec::{
    CodecError, DecodedFrame, EncodedFrame, EncoderSettings, FrameDelivery, VideoDecoder,
    VideoEncoder,
};
pub use display::{CaptureError, DisplayCapturer, DisplayInfo, list_displays};
pub use frame::{CapturedFrame, RgbaFrame};
#[cfg(any(target_os = "macos", windows))]
pub use native::NativeFrame;
pub use permission::{
    PermissionState, request_screen_capture_access, screen_capture_access, system_audio_access,
};
pub use scale::{FrameScaler, MAX_ENCODED_LONG_EDGE, fit_within};
pub use stream::{
    CaptureStream, FRAMES_IN_FLIGHT, ScreenCapturer, StillRefinement, StreamError, StreamSettings,
    StreamStats, spawn_capture_stream,
};
pub use synthetic::{SyntheticCapturer, render_text_page};
