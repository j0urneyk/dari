//! Screen capture and video coding for Dari.
//!
//! The host side captures a display ([`DisplayCapturer`]), scales the frame to the stream's size
//! limit, and encodes it as H.264 ([`VideoEncoder`]). On macOS, ScreenCaptureKit captures and
//! scales on the GPU and VideoToolbox encodes in hardware; elsewhere xcap captures, the CPU
//! scales ([`FrameScaler`]), and OpenH264 encodes. [`spawn_capture_stream`] runs that pipeline on
//! a dedicated thread. The viewer side decodes packets back into BGRA frames ([`VideoDecoder`])
//! ready for GPU upload.

#[cfg(target_os = "macos")]
#[allow(
    unsafe_code,
    reason = "ScreenCaptureKit, CoreVideo, and VideoToolbox are C APIs"
)]
mod apple;
mod codec;
mod display;
mod frame;
mod permission;
mod scale;
mod stream;
mod synthetic;

#[cfg(target_os = "macos")]
pub use apple::NativeFrame;
pub use codec::{
    CodecError, DecodedFrame, EncodedFrame, EncoderSettings, VideoDecoder, VideoEncoder,
};
pub use display::{CaptureError, DisplayCapturer, DisplayInfo, list_displays};
pub use frame::{CapturedFrame, RgbaFrame};
pub use permission::{PermissionState, request_screen_capture_access, screen_capture_access};
pub use scale::{FrameScaler, MAX_ENCODED_LONG_EDGE, fit_within};
pub use stream::{
    CaptureStream, ScreenCapturer, StreamError, StreamSettings, StreamStats, spawn_capture_stream,
};
pub use synthetic::SyntheticCapturer;
