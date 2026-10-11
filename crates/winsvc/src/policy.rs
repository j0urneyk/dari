use dari_proto::{POLICY_KEY, POLICY_VALUE, SecureDesktopControl};
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, WIN32_ERROR};
use windows::core::Error;
use windows_registry::{Key, LOCAL_MACHINE};

use crate::win32::{EVENT_SOURCE, TYPES_SUPPORTED};

const EVENT_LOG: &str = r"SYSTEM\CurrentControlSet\Services\EventLog\Application";
/// Its message table renders `%1` for every event ID from 1 to 1000, so the service needs no
/// message file of its own.
const MESSAGE_FILE: &str = r"%SystemRoot%\System32\EventCreate.exe";

pub(crate) fn write(control: SecureDesktopControl) -> Result<(), Error> {
    write_in(LOCAL_MACHINE, POLICY_KEY, control)
}

pub(crate) fn register_event_source() -> Result<(), Error> {
    register_in(LOCAL_MACHINE, &event_source_key())
}

pub(crate) fn unregister_event_source() -> Result<(), Error> {
    unregister_in(LOCAL_MACHINE, &event_source_key())
}

pub(crate) fn is_access_denied(error: &Error) -> bool {
    is(error, ERROR_ACCESS_DENIED)
}

fn event_source_key() -> String {
    format!(r"{EVENT_LOG}\{EVENT_SOURCE}")
}

fn write_in(root: &Key, path: &str, control: SecureDesktopControl) -> Result<(), Error> {
    root.create(path)?
        .set_u32(POLICY_VALUE, control.as_stored())
}

fn register_in(root: &Key, path: &str) -> Result<(), Error> {
    let key = root.create(path)?;
    key.set_expand_string("EventMessageFile", MESSAGE_FILE)?;
    key.set_u32("TypesSupported", u32::from(TYPES_SUPPORTED))
}

fn unregister_in(root: &Key, path: &str) -> Result<(), Error> {
    match root.remove_tree(path) {
        Err(error) if is(&error, ERROR_FILE_NOT_FOUND) => Ok(()),
        result => result,
    }
}

fn is(error: &Error, code: WIN32_ERROR) -> bool {
    error.code() == code.to_hresult()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use windows_registry::{CURRENT_USER, Type};

    use super::*;
    use SecureDesktopControl::{Off, On};

    struct Scratch(String);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            Self(format!(
                r"Software\dari-winsvc-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ))
        }

        fn path(&self, below: &str) -> String {
            format!(r"{}\{below}", self.0)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _removed = CURRENT_USER.remove_tree(&self.0);
        }
    }

    #[test]
    fn the_policy_round_trips() {
        let scratch = Scratch::new();
        let path = scratch.path(r"Policies\Dari");
        for control in [Off, On, Off] {
            write_in(CURRENT_USER, &path, control).unwrap();
            assert_eq!(
                SecureDesktopControl::read_from(CURRENT_USER, &path),
                control
            );
            let key = CURRENT_USER.open(&path).unwrap();
            assert_eq!(key.get_type(POLICY_VALUE).unwrap(), Type::U32);
            assert_eq!(key.get_u32(POLICY_VALUE).unwrap(), control.as_stored());
        }
    }

    #[test]
    fn the_event_source_is_registered_and_removed_idempotently() {
        let scratch = Scratch::new();
        let path = scratch.path(EVENT_SOURCE);
        for _ in 0..2 {
            register_in(CURRENT_USER, &path).unwrap();
            let key = CURRENT_USER.open(&path).unwrap();
            assert_eq!(
                key.get_type("EventMessageFile").unwrap(),
                Type::ExpandString
            );
            assert_eq!(key.get_string("EventMessageFile").unwrap(), MESSAGE_FILE);
            assert_eq!(key.get_type("TypesSupported").unwrap(), Type::U32);
            assert_eq!(key.get_u32("TypesSupported").unwrap(), 7);
        }
        for _ in 0..2 {
            unregister_in(CURRENT_USER, &path).unwrap();
            let gone = CURRENT_USER.open(&path).unwrap_err();
            assert!(is(&gone, ERROR_FILE_NOT_FOUND), "{gone}");
        }
        assert!(CURRENT_USER.open(&scratch.0).is_ok());
    }

    #[test]
    fn the_event_source_key_is_dariservices_application_log_entry() {
        assert_eq!(
            event_source_key(),
            r"SYSTEM\CurrentControlSet\Services\EventLog\Application\DariService"
        );
    }
}
