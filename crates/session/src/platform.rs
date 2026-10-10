use dari_input::{EnigoBackend, InjectError, InputBackend};

use crate::clipboard::{ClipboardFactory, SystemClipboard};
use crate::secure_desktop::SecureDesktopLink;
use dari_media::{
    AudioCapturer, AudioError, CaptureError, DisplayCapturer, DisplayInfo, PermissionState,
    ScreenCapturer, StreamSettings, SystemAudioCapturer, list_displays, system_audio_access,
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
    /// Whether the OS lets this app record what the system plays, without asking.
    /// [`PermissionState::NotDetermined`] means opening the capturer will ask the user and wait.
    fn audio_access(&self) -> PermissionState {
        PermissionState::NotRequired
    }
    /// Starts the helper that sees the Windows secure desktop, for a session the host user
    /// approved. `input` is false for a view-only session. Called from the session's runtime;
    /// `None` means this platform has no such helper.
    fn open_secure_desktop(&self, _input: bool) -> Option<SecureDesktopLink> {
        None
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

    fn audio_access(&self) -> PermissionState {
        system_audio_access()
    }
}
