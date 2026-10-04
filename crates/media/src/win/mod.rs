//! Windows capture and encoding through Windows.Graphics.Capture, Direct3D 11, and Media
//! Foundation.
//!
//! These are the only OS bindings in the crate that need `unsafe`: Direct3D 11, Media
//! Foundation, and the capture interop are COM APIs, and the `windows` crate marks every COM
//! call unsafe. Every unsafe block states what it relies on.

mod capture;
mod convert;
mod device;
mod encoder;
mod frame;

pub(crate) use capture::GraphicsCaptureCapturer;
pub(crate) use encoder::HardwareEncoder;
pub use frame::NativeFrame;
