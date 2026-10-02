//! The OS permissions hosting needs (macOS privacy settings).

use open_desk_media::PermissionState;

/// What this machine currently allows a remote viewer to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocalPermissions {
    pub(crate) screen: bool,
    pub(crate) input: bool,
}

impl LocalPermissions {
    pub(crate) fn check() -> Self {
        Self {
            screen: open_desk_media::screen_capture_access() != PermissionState::Denied,
            input: open_desk_input::input_access_granted(),
        }
    }

    pub(crate) fn all_granted(self) -> bool {
        self.screen && self.input
    }
}

/// Shows the OS prompts for whatever is missing.
pub(crate) fn request_missing(current: LocalPermissions) {
    if !current.screen {
        open_desk_media::request_screen_capture_access();
    }
    if !current.input {
        open_desk_input::request_input_access();
    }
}

/// System Settings pane for the first missing permission.
pub(crate) fn settings_url(current: LocalPermissions) -> &'static str {
    if current.screen {
        "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
    } else {
        "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
    }
}
