//! Operating-system permission to record the screen and the system's sound.

/// Whether this process may capture the screen or the system's sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionState {
    Granted,
    Denied,
    /// The user hasn't been asked yet, or the OS can't tell; using it may prompt.
    NotDetermined,
    /// The platform has no such permission (Windows).
    NotRequired,
}

/// Checks system-audio-recording permission without prompting.
///
/// On macOS this is "System Audio Recording" under Screen & System Audio Recording. An app the
/// user refused still opens its process tap and records silence, so callers must check this to
/// tell a refusal from a quiet machine.
pub fn system_audio_access() -> PermissionState {
    #[cfg(target_os = "macos")]
    {
        audio_access_from_preflight(crate::apple::system_audio_preflight())
    }
    #[cfg(not(target_os = "macos"))]
    {
        PermissionState::NotRequired
    }
}

#[cfg(any(target_os = "macos", test))]
fn audio_access_from_preflight(answer: Option<i32>) -> PermissionState {
    match answer {
        Some(0) => PermissionState::Granted,
        Some(1) => PermissionState::Denied,
        _ => PermissionState::NotDetermined,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_refusal_counts_as_denied() {
        assert_eq!(
            audio_access_from_preflight(Some(0)),
            PermissionState::Granted
        );
        assert_eq!(
            audio_access_from_preflight(Some(1)),
            PermissionState::Denied
        );
        assert_eq!(
            audio_access_from_preflight(Some(2)),
            PermissionState::NotDetermined
        );
        assert_eq!(
            audio_access_from_preflight(None),
            PermissionState::NotDetermined
        );
    }
}
