//! What each DXGI result means for the screen thread. HRESULTs are plain `i32`s so the table runs
//! in tests on every platform; a Windows-only test pins each constant to the `windows` crate.

use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Hresult(pub(crate) i32);

impl Hresult {
    pub(crate) const E_ACCESSDENIED: Self = Self(0x8007_0005_u32.cast_signed());
    pub(crate) const INVALID_CALL: Self = Self(0x887A_0001_u32.cast_signed());
    pub(crate) const UNSUPPORTED: Self = Self(0x887A_0004_u32.cast_signed());
    pub(crate) const DEVICE_REMOVED: Self = Self(0x887A_0005_u32.cast_signed());
    pub(crate) const DEVICE_RESET: Self = Self(0x887A_0007_u32.cast_signed());
    pub(crate) const NOT_CURRENTLY_AVAILABLE: Self = Self(0x887A_0022_u32.cast_signed());
    pub(crate) const ACCESS_LOST: Self = Self(0x887A_0026_u32.cast_signed());
    pub(crate) const WAIT_TIMEOUT: Self = Self(0x887A_0027_u32.cast_signed());
    pub(crate) const SESSION_DISCONNECTED: Self = Self(0x887A_0028_u32.cast_signed());
}

impl fmt::Debug for Hresult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#010X}", self.0.cast_unsigned())
    }
}

impl fmt::Display for Hresult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// What a failed `AcquireNextFrame`, image or pointer-shape copy, or `ReleaseFrame` means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameFailure {
    /// `AcquireNextFrame` timed out: the screen is still.
    Still,
    /// Drop the duplication, check the input desktop at once, re-attach, and duplicate again.
    /// Phase 0 saw `ACCESS_LOST` going to `Winlogon` and `INVALID_CALL` coming back; for the
    /// device codes and anything unknown, rebuilding is still a safe recovery.
    Switched,
}

pub(crate) fn frame_failure(code: Hresult) -> FrameFailure {
    if code == Hresult::WAIT_TIMEOUT {
        FrameFailure::Still
    } else {
        FrameFailure::Switched
    }
}

/// What a failed `DuplicateOutput`, or a failed attach before it, means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DuplicateFailure {
    /// Retry after the next desktop check; report the screen unavailable once it has failed
    /// for 5 s. `E_ACCESSDENIED` is transient during a switch (Phase 0).
    Transient,
    /// Unavailable until the desktop or display changes.
    Unsupported,
}

pub(crate) fn duplicate_failure(code: Hresult) -> DuplicateFailure {
    if code == Hresult::UNSUPPORTED || code == Hresult::SESSION_DISCONNECTED {
        DuplicateFailure::Unsupported
    } else {
        DuplicateFailure::Transient
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_dxgi_result_maps_to_its_action() {
        let unknown = Hresult(0x8000_4005_u32.cast_signed());
        for (code, action) in [
            (Hresult::WAIT_TIMEOUT, FrameFailure::Still),
            (Hresult::ACCESS_LOST, FrameFailure::Switched),
            (Hresult::INVALID_CALL, FrameFailure::Switched),
            (Hresult::DEVICE_REMOVED, FrameFailure::Switched),
            (Hresult::DEVICE_RESET, FrameFailure::Switched),
            (Hresult::E_ACCESSDENIED, FrameFailure::Switched),
            (unknown, FrameFailure::Switched),
        ] {
            assert_eq!(frame_failure(code), action, "{code}");
        }
        for (code, action) in [
            (Hresult::E_ACCESSDENIED, DuplicateFailure::Transient),
            (
                Hresult::NOT_CURRENTLY_AVAILABLE,
                DuplicateFailure::Transient,
            ),
            (Hresult::ACCESS_LOST, DuplicateFailure::Transient),
            (Hresult::DEVICE_REMOVED, DuplicateFailure::Transient),
            (unknown, DuplicateFailure::Transient),
            (Hresult::UNSUPPORTED, DuplicateFailure::Unsupported),
            (Hresult::SESSION_DISCONNECTED, DuplicateFailure::Unsupported),
        ] {
            assert_eq!(duplicate_failure(code), action, "{code}");
        }
    }

    #[test]
    fn codes_print_as_hex() {
        assert_eq!(Hresult::ACCESS_LOST.to_string(), "0x887A0026");
    }

    #[cfg(windows)]
    #[test]
    fn the_hresult_constants_match_the_windows_crate() {
        use windows::Win32::Foundation::E_ACCESSDENIED;
        use windows::Win32::Graphics::Dxgi::{
            DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
            DXGI_ERROR_INVALID_CALL, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE,
            DXGI_ERROR_SESSION_DISCONNECTED, DXGI_ERROR_UNSUPPORTED, DXGI_ERROR_WAIT_TIMEOUT,
        };

        for (ours, theirs) in [
            (Hresult::E_ACCESSDENIED, E_ACCESSDENIED),
            (Hresult::INVALID_CALL, DXGI_ERROR_INVALID_CALL),
            (Hresult::UNSUPPORTED, DXGI_ERROR_UNSUPPORTED),
            (Hresult::DEVICE_REMOVED, DXGI_ERROR_DEVICE_REMOVED),
            (Hresult::DEVICE_RESET, DXGI_ERROR_DEVICE_RESET),
            (
                Hresult::NOT_CURRENTLY_AVAILABLE,
                DXGI_ERROR_NOT_CURRENTLY_AVAILABLE,
            ),
            (Hresult::ACCESS_LOST, DXGI_ERROR_ACCESS_LOST),
            (Hresult::WAIT_TIMEOUT, DXGI_ERROR_WAIT_TIMEOUT),
            (
                Hresult::SESSION_DISCONNECTED,
                DXGI_ERROR_SESSION_DISCONNECTED,
            ),
        ] {
            assert_eq!(ours.0, theirs.0, "{ours}");
        }
    }
}
