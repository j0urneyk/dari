use dari_input::{EnigoBackend, InjectError, InputBackend};

use crate::clipboard::{ClipboardFactory, SystemClipboard};
use dari_media::{
    AudioCapturer, AudioError, CaptureError, DisplayCapturer, DisplayInfo, ScreenCapturer,
    StreamSettings, SystemAudioCapturer, list_displays,
};

/// The host machine's screen and input devices.
///
/// Abstracted so sessions can be tested end to end without a display or real input.
pub trait HostPlatform: Send + Sync + 'static {
    /// Connected displays, primary first.
    fn displays(&self) -> Result<Vec<DisplayInfo>, CaptureError>;
    /// Opens a capturer for `display` for a stream with `settings`. Called on the capture thread.
    fn open_capturer(
        &self,
        display: u32,
        settings: StreamSettings,
    ) -> Result<Box<dyn ScreenCapturer>, CaptureError>;
    /// Opens the input backend. Called on the input thread.
    fn open_input(&self) -> Result<Box<dyn InputBackend>, InjectError>;
    /// The clipboard to synchronize, if any.
    fn clipboard(&self) -> Option<ClipboardFactory> {
        None
    }
    /// Opens a capturer for what the system is playing. Called on the audio thread.
    fn open_audio(&self) -> Result<Box<dyn AudioCapturer>, AudioError> {
        Err(AudioError::Unsupported)
    }
}

/// The real screen (ScreenCaptureKit on macOS, Windows.Graphics.Capture on Windows), input
/// (enigo), and system audio (cpal loopback).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemPlatform;

impl HostPlatform for SystemPlatform {
    fn displays(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
        list_displays()
    }

    fn open_capturer(
        &self,
        display: u32,
        settings: StreamSettings,
    ) -> Result<Box<dyn ScreenCapturer>, CaptureError> {
        Ok(Box::new(DisplayCapturer::open(Some(display), &settings)?))
    }

    fn open_input(&self) -> Result<Box<dyn InputBackend>, InjectError> {
        Ok(Box::new(EnigoBackend::new()?))
    }

    fn clipboard(&self) -> Option<ClipboardFactory> {
        Some(SystemClipboard::factory())
    }

    fn open_audio(&self) -> Result<Box<dyn AudioCapturer>, AudioError> {
        Ok(Box::new(SystemAudioCapturer::open()?))
    }
}
