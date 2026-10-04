//! Physical displays and capturing them: ScreenCaptureKit on macOS, Windows.Graphics.Capture on
//! Windows, and xcap screenshots elsewhere. xcap lists the displays everywhere.

use std::time::Duration;

use thiserror::Error;

use crate::frame::CapturedFrame;
#[cfg(not(any(target_os = "macos", windows)))]
use crate::frame::RgbaFrame;
use crate::permission::{PermissionState, screen_capture_access};
use crate::stream::{ScreenCapturer, StreamSettings};

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("screen recording permission has not been granted")]
    PermissionDenied,
    #[error("display {0} is not connected")]
    DisplayNotFound(u32),
    #[error("no display is connected")]
    NoDisplay,
    #[error("screen capture failed: {0}")]
    Backend(String),
}

impl From<xcap::XCapError> for CaptureError {
    fn from(error: xcap::XCapError) -> Self {
        CaptureError::Backend(error.to_string())
    }
}

/// A display as the operating system lays it out.
///
/// `x`, `y`, `width`, and `height` are in the coordinate space the OS uses for pointer input:
/// points on macOS and physical pixels on Windows (the app is per-monitor DPI aware).
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayInfo {
    pub id: u32,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
    pub is_primary: bool,
    /// Refresh rate in hertz, or 0 if the OS does not report one.
    pub refresh_rate: u32,
}

impl DisplayInfo {
    fn from_monitor(monitor: &xcap::Monitor) -> Result<Self, CaptureError> {
        Ok(Self {
            id: monitor.id()?,
            name: monitor.friendly_name().or_else(|_| monitor.name())?,
            x: monitor.x()?,
            y: monitor.y()?,
            width: monitor.width()?,
            height: monitor.height()?,
            scale_factor: monitor.scale_factor()?,
            is_primary: monitor.is_primary()?,
            refresh_rate: monitor.frequency().map_or(0, round_hertz),
        })
    }
}

/// Rounds a reported refresh rate such as 59.94 to whole hertz.
fn round_hertz(frequency: f32) -> u32 {
    if frequency.is_finite() && frequency > 0.0 {
        // In range: refresh rates are a few hundred hertz at most.
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let hertz = frequency.round().min(1000.0) as u32;
        hertz
    } else {
        0
    }
}

/// Lists connected displays, primary first.
pub fn list_displays() -> Result<Vec<DisplayInfo>, CaptureError> {
    let mut displays = xcap::Monitor::all()?
        .iter()
        .map(DisplayInfo::from_monitor)
        .collect::<Result<Vec<_>, _>>()?;
    displays.sort_by_key(|display| !display.is_primary);
    Ok(displays)
}

/// Captures one display. Create it on the thread that will capture: platform handles are not
/// guaranteed to be `Send`.
pub struct DisplayCapturer {
    info: DisplayInfo,
    #[cfg(target_os = "macos")]
    source: crate::apple::ScreenCaptureKitCapturer,
    #[cfg(windows)]
    source: crate::win::GraphicsCaptureCapturer,
    #[cfg(not(any(target_os = "macos", windows)))]
    source: xcap::Monitor,
}

impl std::fmt::Debug for DisplayCapturer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DisplayCapturer")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

impl DisplayCapturer {
    /// Opens display `id`, or the primary display when `id` is `None`, for a stream with
    /// `settings`. On macOS and Windows, the capture itself scales to `settings.max_long_edge`
    /// and limits the rate to `settings.max_fps`.
    pub fn open(id: Option<u32>, settings: &StreamSettings) -> Result<Self, CaptureError> {
        if screen_capture_access() == PermissionState::Denied {
            return Err(CaptureError::PermissionDenied);
        }
        let monitors = xcap::Monitor::all()?;
        let monitor = if let Some(id) = id {
            monitors
                .into_iter()
                .find(|monitor| monitor.id().is_ok_and(|candidate| candidate == id))
                .ok_or(CaptureError::DisplayNotFound(id))?
        } else {
            let primary = monitors
                .iter()
                .position(|monitor| monitor.is_primary().unwrap_or(false))
                .unwrap_or(0);
            monitors
                .into_iter()
                .nth(primary)
                .ok_or(CaptureError::NoDisplay)?
        };
        let info = DisplayInfo::from_monitor(&monitor)?;
        #[cfg(target_os = "macos")]
        let source = crate::apple::ScreenCaptureKitCapturer::open(
            info.id,
            settings.max_long_edge,
            settings.max_fps,
        )?;
        #[cfg(windows)]
        let source = crate::win::GraphicsCaptureCapturer::open(
            info.id,
            settings.max_long_edge,
            settings.max_fps,
        )?;
        #[cfg(not(any(target_os = "macos", windows)))]
        let source = {
            // xcap captures whole frames on demand; the stream paces and scales them.
            let _ = settings;
            monitor
        };
        Ok(Self { info, source })
    }

    pub fn info(&self) -> &DisplayInfo {
        &self.info
    }
}

impl ScreenCapturer for DisplayCapturer {
    #[cfg(any(target_os = "macos", windows))]
    fn capture(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
        self.source.capture(timeout)
    }

    #[cfg(any(target_os = "macos", windows))]
    fn paces_itself(&self) -> bool {
        self.source.paces_itself()
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    fn capture(&mut self, _timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
        let image = self.source.capture_image()?;
        let (width, height) = image.dimensions();
        RgbaFrame::new(width, height, image.into_raw())
            .map(|frame| Some(frame.into()))
            .ok_or_else(|| CaptureError::Backend("captured image has an invalid size".into()))
    }
}
