//! Screen capture and video coding for Dari.
//!
//! The host side captures a display ([`DisplayCapturer`]), scales the frame to the stream's size
//! limit ([`FrameScaler`]), and encodes it as H.264 ([`VideoEncoder`]). [`spawn_capture_stream`]
//! runs that pipeline on a dedicated thread. The viewer side decodes packets back into BGRA
//! frames ([`VideoDecoder`]) ready for GPU upload.

mod codec;
mod display;
mod frame;
mod permission;
mod scale;
mod stream;
mod synthetic;

pub use codec::{
    CodecError, DecodedFrame, EncodedFrame, EncoderSettings, VideoDecoder, VideoEncoder,
};
pub use display::{CaptureError, DisplayCapturer, DisplayInfo, list_displays};
pub use frame::RgbaFrame;
pub use permission::{PermissionState, request_screen_capture_access, screen_capture_access};
pub use scale::{FrameScaler, MAX_ENCODED_LONG_EDGE, fit_within};
pub use stream::{
    CaptureStream, ScreenCapturer, StreamError, StreamSettings, StreamStats, spawn_capture_stream,
};
pub use synthetic::SyntheticCapturer;
