//! Physical displays and capturing them with xcap.

use thiserror::Error;

use crate::frame::RgbaFrame;
use crate::permission::{PermissionState, screen_capture_access};
use crate::stream::ScreenCapturer;

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
        })
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
    monitor: xcap::Monitor,
    info: DisplayInfo,
}

impl std::fmt::Debug for DisplayCapturer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DisplayCapturer")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

impl DisplayCapturer {
    /// Opens display `id`, or the primary display when `id` is `None`.
    pub fn open(id: Option<u32>) -> Result<Self, CaptureError> {
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
        Ok(Self { monitor, info })
    }

    pub fn info(&self) -> &DisplayInfo {
        &self.info
    }
}

impl ScreenCapturer for DisplayCapturer {
    fn capture(&mut self) -> Result<RgbaFrame, CaptureError> {
        let image = self.monitor.capture_image()?;
        let (width, height) = image.dimensions();
        RgbaFrame::new(width, height, image.into_raw())
            .ok_or_else(|| CaptureError::Backend("captured image has an invalid size".into()))
    }
}
