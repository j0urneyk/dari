//! macOS capture and encoding through ScreenCaptureKit and VideoToolbox.
//!
//! These are the only OS bindings in the crate that need `unsafe`: ScreenCaptureKit, CoreVideo,
//! CoreMedia, and VideoToolbox are C and Objective-C APIs. Every unsafe block states what it
//! relies on.

mod capture;
mod encoder;
mod frame;

pub(crate) use capture::ScreenCaptureKitCapturer;
pub(crate) use encoder::HardwareEncoder;
pub(crate) use frame::NATIVE_PIXEL_FORMAT;
pub use frame::NativeFrame;
