//! Operating-system permission to record the screen.

/// Whether this process may capture the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionState {
    Granted,
    Denied,
    /// The platform has no such permission (Windows).
    NotRequired,
}

/// Checks screen-recording permission without prompting.
///
/// On macOS this is the "Screen & System Audio Recording" privacy setting; without it,
/// captures only contain the desktop background.
pub fn screen_capture_access() -> PermissionState {
    #[cfg(target_os = "macos")]
    {
        if objc2_core_graphics::CGPreflightScreenCaptureAccess() {
            PermissionState::Granted
        } else {
            PermissionState::Denied
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        PermissionState::NotRequired
    }
}

/// Asks the operating system to prompt for screen-recording permission.
///
/// macOS shows its prompt at most once per app; afterwards the user must enable the app in
/// System Settings and restart it. Returns the state after the request.
pub fn request_screen_capture_access() -> PermissionState {
    #[cfg(target_os = "macos")]
    {
        if objc2_core_graphics::CGRequestScreenCaptureAccess() {
            PermissionState::Granted
        } else {
            PermissionState::Denied
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        PermissionState::NotRequired
    }
}
