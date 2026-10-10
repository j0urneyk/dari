use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetUserObjectInformationW, HDESK,
    OpenInputDesktop, SetThreadDesktop, UOI_NAME,
};

use super::from_wide;
use crate::tracker::{DesktopSource, Observation};

/// Naming, attaching to, and duplicating `Winlogon` or `Default` need no desktop-specific right,
/// as measured in the test VM. `SendInput` needs `DESKTOP_JOURNALPLAYBACK`, so the input thread
/// of #47 must open its own handle with that right rather than widen this one.
const NO_RIGHTS: DESKTOP_ACCESS_FLAGS = DESKTOP_ACCESS_FLAGS(0);

#[derive(Debug, Default)]
pub(crate) struct InputDesktopSource;

impl DesktopSource for InputDesktopSource {
    fn poll(&mut self) -> Observation {
        // SAFETY: `OpenInputDesktop` takes no pointers; the handle is closed before returning.
        let Ok(desktop) = (unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, NO_RIGHTS) })
        else {
            return Observation::Unreadable;
        };
        let name = name(desktop);
        // SAFETY: the handle came from `OpenInputDesktop` and no thread uses it.
        let _closed = unsafe { CloseDesktop(desktop) };
        name
    }
}

/// The input desktop the calling thread is attached to. Keep it while attached: a desktop a
/// thread uses can't be closed.
#[derive(Debug)]
pub(crate) struct AttachedDesktop(HDESK);

impl AttachedDesktop {
    /// Attaches the calling thread to the current input desktop and names it. Fails on a thread
    /// that has a window or a hook.
    pub(crate) fn attach() -> windows::core::Result<(Self, Observation)> {
        // SAFETY: `OpenInputDesktop` takes no pointers; the handle is owned by the result.
        let desktop =
            Self(unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, NO_RIGHTS)? });
        // SAFETY: `desktop` is an open desktop handle that outlives the call.
        unsafe { SetThreadDesktop(desktop.0)? };
        let name = name(desktop.0);
        Ok((desktop, name))
    }
}

impl Drop for AttachedDesktop {
    fn drop(&mut self) {
        // SAFETY: the handle came from `OpenInputDesktop`. Closing fails, harmlessly, while a
        // thread still uses it.
        let _closed = unsafe { CloseDesktop(self.0) };
    }
}

fn name(desktop: HDESK) -> Observation {
    let mut name = [0u16; 256];
    // SAFETY: `name` outlives the call, which writes at most its size into it.
    let read = unsafe {
        GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            u32::try_from(size_of_val(&name)).unwrap_or(0),
            None,
        )
    };
    match read {
        Ok(()) => Observation::Named(from_wide(&name)),
        Err(_) => Observation::Unreadable,
    }
}
