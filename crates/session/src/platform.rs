use open_desk_input::{EnigoBackend, InjectError, InputBackend};

use crate::clipboard::{ClipboardFactory, SystemClipboard};
use open_desk_media::{CaptureError, DisplayCapturer, DisplayInfo, ScreenCapturer, list_displays};

/// The host machine's screen and input devices.
///
/// Abstracted so sessions can be tested end to end without a display or real input.
pub trait HostPlatform: Send + Sync + 'static {
    /// Connected displays, primary first.
    fn displays(&self) -> Result<Vec<DisplayInfo>, CaptureError>;
    /// Opens a capturer for `display`. Called on the capture thread.
    fn open_capturer(&self, display: u32) -> Result<Box<dyn ScreenCapturer>, CaptureError>;
    /// Opens the input backend. Called on the input thread.
    fn open_input(&self) -> Result<Box<dyn InputBackend>, InjectError>;
    /// The clipboard to synchronize, if any.
    fn clipboard(&self) -> Option<ClipboardFactory> {
        None
    }
}

/// The real screen (xcap) and input (enigo).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemPlatform;

impl HostPlatform for SystemPlatform {
    fn displays(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
        list_displays()
    }

    fn open_capturer(&self, display: u32) -> Result<Box<dyn ScreenCapturer>, CaptureError> {
        Ok(Box::new(DisplayCapturer::open(Some(display))?))
    }

    fn open_input(&self) -> Result<Box<dyn InputBackend>, InjectError> {
        Ok(Box::new(EnigoBackend::new()?))
    }

    fn clipboard(&self) -> Option<ClipboardFactory> {
        Some(SystemClipboard::factory())
    }
}
