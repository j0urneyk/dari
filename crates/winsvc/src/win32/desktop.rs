use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, GetUserObjectInformationW,
    OpenInputDesktop, UOI_NAME,
};

use super::from_wide;
use crate::tracker::{DesktopSource, Observation};

#[derive(Debug, Default)]
pub(crate) struct InputDesktopSource;

impl DesktopSource for InputDesktopSource {
    fn poll(&mut self) -> Observation {
        // SAFETY: the desktop handle is closed before returning, and `name` outlives the call
        // that writes at most its size into it.
        unsafe {
            let Ok(desktop) =
                OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS)
            else {
                return Observation::Unreadable;
            };
            let mut name = [0u16; 256];
            let read = GetUserObjectInformationW(
                HANDLE(desktop.0),
                UOI_NAME,
                Some(name.as_mut_ptr().cast()),
                u32::try_from(size_of_val(&name)).unwrap_or(0),
                None,
            );
            let _closed = CloseDesktop(desktop);
            match read {
                Ok(()) => Observation::Named(from_wide(&name)),
                Err(_) => Observation::Unreadable,
            }
        }
    }
}
