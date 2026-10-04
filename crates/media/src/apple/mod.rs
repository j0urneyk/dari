//! macOS capture and encoding through ScreenCaptureKit and VideoToolbox, and the privacy check
//! for system audio.
//!
//! These are the only OS bindings in the crate that need `unsafe`: ScreenCaptureKit, CoreVideo,
//! CoreMedia, VideoToolbox, and TCC are C and Objective-C APIs. Every unsafe block states what it
//! relies on.

mod capture;
mod encoder;
mod frame;
mod tcc;

pub(crate) use capture::ScreenCaptureKitCapturer;
pub(crate) use encoder::HardwareEncoder;
pub(crate) use frame::NATIVE_PIXEL_FORMAT;
pub use frame::NativeFrame;
pub(crate) use tcc::system_audio_preflight;
