//! Raising the secure attention sequence (Ctrl+Alt+Del) with `SendSAS`, which does nothing for a
//! service unless `SoftwareSASGeneration` lets services raise it.

pub(crate) const SAS_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";
/// The DWORD that says which software may raise the sequence: 0 none, 1 services, 2 Ease of
/// Access applications, 3 both. Windows treats a missing value as 0.
pub(crate) const SAS_VALUE: &str = "SoftwareSASGeneration";
/// The service's own key, which only administrators and the service write.
pub(crate) const MARKER_KEY: &str = r"SYSTEM\CurrentControlSet\Services\DariService\Parameters";
pub(crate) const SAS_SET_MARKER: &str = "SoftwareSASGenerationSet";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stored {
    Missing,
    Dword(u32),
    NotDword,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Generation {
    Allowed(u32),
    Unset,
    Disallowed(u32),
    NotDword,
}

impl Generation {
    pub(crate) fn from_stored(stored: Stored) -> Self {
        match stored {
            Stored::Missing => Self::Unset,
            Stored::Dword(value @ (1 | 3)) => Self::Allowed(value),
            Stored::Dword(value) => Self::Disallowed(value),
            Stored::NotDword => Self::NotDword,
        }
    }
}

impl std::fmt::Display for Generation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Allowed(value) | Self::Disallowed(value) => {
                write!(f, "{SAS_VALUE} is {value}")
            }
            Self::NotDword => write!(f, "{SAS_VALUE} isn't a DWORD"),
            Self::Unset => write!(f, "{SAS_VALUE} was set to 1 for the call"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Raised {
    pub(crate) generation: Generation,
    pub(crate) cleanup_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NotRaised {
    Disallowed(Generation),
    Failed(String),
}

#[cfg(windows)]
pub(crate) use registry::{delete_leftover, raise};

#[cfg(windows)]
mod registry {
    use std::io;
    use std::sync::{Mutex, PoisonError};

    use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows_registry::{Key, LOCAL_MACHINE, Type};

    use super::{
        Generation, MARKER_KEY, NotRaised, Raised, SAS_KEY, SAS_SET_MARKER, SAS_VALUE, Stored,
    };

    static RAISING: Mutex<()> = Mutex::new(());

    pub(crate) struct Keys<'a> {
        pub(crate) root: &'a Key,
        pub(crate) sas: &'a str,
        pub(crate) marker: &'a str,
    }

    const MACHINE: Keys<'static> = Keys {
        root: LOCAL_MACHINE,
        sas: SAS_KEY,
        marker: MARKER_KEY,
    };

    pub(crate) fn raise() -> Result<Raised, NotRaised> {
        raise_in(&MACHINE, crate::win32::send_sas)
    }

    pub(crate) fn delete_leftover() -> Result<bool, String> {
        let _raising = RAISING.lock().unwrap_or_else(PoisonError::into_inner);
        delete_leftover_in(&MACHINE)
    }

    pub(crate) fn raise_in(
        keys: &Keys<'_>,
        send: impl FnOnce() -> io::Result<()>,
    ) -> Result<Raised, NotRaised> {
        let _raising = RAISING.lock().unwrap_or_else(PoisonError::into_inner);
        delete_leftover_in(keys).map_err(NotRaised::Failed)?;
        let failed = |doing: &str, error: &dyn std::fmt::Display| {
            NotRaised::Failed(format!("cannot {doing} {SAS_VALUE}: {error}"))
        };
        let key = keys
            .root
            .create(keys.sas)
            .map_err(|error| failed("open the key of", &error))?;
        let generation =
            Generation::from_stored(stored(&key).map_err(|error| failed("read", &error))?);
        match generation {
            Generation::Disallowed(_) | Generation::NotDword => {
                Err(NotRaised::Disallowed(generation))
            }
            Generation::Allowed(_) => {
                send().map_err(|error| failed("call SendSAS with", &error))?;
                Ok(Raised {
                    generation,
                    cleanup_error: None,
                })
            }
            Generation::Unset => {
                let marker = keys
                    .root
                    .create(keys.marker)
                    .and_then(|marker| marker.set_u32(SAS_SET_MARKER, 1).map(|()| marker))
                    .map_err(|error| failed("note that the service sets", &error))?;
                if let Err(error) = key.set_u32(SAS_VALUE, 1) {
                    let _removed = marker.remove_value(SAS_SET_MARKER);
                    return Err(failed("set", &error));
                }
                let sent = send();
                let cleaned = delete_marked(&key, &marker);
                match (sent, cleaned) {
                    (Ok(()), cleaned) => Ok(Raised {
                        generation,
                        cleanup_error: cleaned.err(),
                    }),
                    (Err(error), Ok(())) => Err(failed("call SendSAS after setting", &error)),
                    (Err(error), Err(cleanup)) => Err(NotRaised::Failed(format!(
                        "cannot call SendSAS after setting {SAS_VALUE}: {error}; {cleanup}"
                    ))),
                }
            }
        }
    }

    pub(crate) fn delete_leftover_in(keys: &Keys<'_>) -> Result<bool, String> {
        let marker = match keys.root.options().read().write().open(keys.marker) {
            Ok(marker) => marker,
            Err(error) if not_found(&error) => return Ok(false),
            Err(error) => return Err(format!("cannot open the key of {SAS_SET_MARKER}: {error}")),
        };
        match marker.get_value(SAS_SET_MARKER) {
            Ok(_) => {}
            Err(error) if not_found(&error) => return Ok(false),
            Err(error) => return Err(format!("cannot read {SAS_SET_MARKER}: {error}")),
        }
        let key = keys
            .root
            .options()
            .read()
            .write()
            .open(keys.sas)
            .map_err(|error| format!("cannot open the key of {SAS_VALUE}: {error}"))?;
        delete_marked(&key, &marker)?;
        Ok(true)
    }

    fn delete_marked(key: &Key, marker: &Key) -> Result<(), String> {
        let value = stored(key).map_err(|error| {
            format!("cannot read {SAS_VALUE}, which may still be the service's 1: {error}")
        })?;
        if value == Stored::Dword(1) {
            key.remove_value(SAS_VALUE).map_err(|error| {
                format!(
                    "cannot delete {SAS_VALUE}, which stays 1 until the service's next start or call: {error}"
                )
            })?;
        }
        marker.remove_value(SAS_SET_MARKER).map_err(|error| {
            format!("deleted {SAS_VALUE} but cannot delete {SAS_SET_MARKER}: {error}")
        })
    }

    fn not_found(error: &windows::core::Error) -> bool {
        error.code() == ERROR_FILE_NOT_FOUND.to_hresult()
    }

    fn stored(key: &Key) -> windows_registry::Result<Stored> {
        match key.get_value(SAS_VALUE) {
            Ok(value) if value.ty() == Type::U32 => Ok(match u32::try_from(value) {
                Ok(value) => Stored::Dword(value),
                Err(_) => Stored::NotDword,
            }),
            Ok(_) => Ok(Stored::NotDword),
            Err(error) if not_found(&error) => Ok(Stored::Missing),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_1_or_3_lets_the_service_call_and_only_a_missing_value_is_written() {
        for (stored, generation) in [
            (Stored::Missing, Generation::Unset),
            (Stored::Dword(1), Generation::Allowed(1)),
            (Stored::Dword(3), Generation::Allowed(3)),
            (Stored::Dword(0), Generation::Disallowed(0)),
            (Stored::Dword(2), Generation::Disallowed(2)),
            (Stored::Dword(4), Generation::Disallowed(4)),
            (Stored::Dword(u32::MAX), Generation::Disallowed(u32::MAX)),
            (Stored::NotDword, Generation::NotDword),
        ] {
            assert_eq!(Generation::from_stored(stored), generation, "{stored:?}");
        }
    }

    #[test]
    fn the_log_says_what_the_value_was() {
        assert_eq!(
            Generation::Unset.to_string(),
            "SoftwareSASGeneration was set to 1 for the call"
        );
        assert_eq!(
            Generation::Allowed(3).to_string(),
            "SoftwareSASGeneration is 3"
        );
        assert_eq!(
            Generation::NotDword.to_string(),
            "SoftwareSASGeneration isn't a DWORD"
        );
    }

    #[cfg(windows)]
    mod registry {
        use std::cell::Cell;
        use std::io;

        use windows_registry::{CURRENT_USER, Type};

        use super::super::registry::{Keys, delete_leftover_in, raise_in};
        use super::super::*;

        struct Scratch {
            root: String,
            sas: String,
            marker: String,
        }

        impl Scratch {
            fn new(name: &str) -> Self {
                let root = format!(r"Software\dari-winsvc-sas-{}-{name}", std::process::id());
                let _cleared = CURRENT_USER.remove_tree(&root);
                Self {
                    sas: format!(r"{root}\System"),
                    marker: format!(r"{root}\Parameters"),
                    root,
                }
            }

            fn keys(&self) -> Keys<'_> {
                Keys {
                    root: CURRENT_USER,
                    sas: &self.sas,
                    marker: &self.marker,
                }
            }

            fn stored(&self) -> Option<(Type, Vec<u8>)> {
                read(&self.sas, SAS_VALUE)
            }

            fn marked(&self) -> bool {
                read(&self.marker, SAS_SET_MARKER).is_some()
            }

            fn set(&self, ty: Type, data: &[u8]) {
                CURRENT_USER
                    .create(&self.sas)
                    .unwrap()
                    .set_bytes(SAS_VALUE, ty, data)
                    .unwrap();
            }

            fn mark(&self) {
                CURRENT_USER
                    .create(&self.marker)
                    .unwrap()
                    .set_u32(SAS_SET_MARKER, 1)
                    .unwrap();
            }
        }

        impl Drop for Scratch {
            fn drop(&mut self) {
                let _removed = CURRENT_USER.remove_tree(&self.root);
            }
        }

        fn read(path: &str, name: &str) -> Option<(Type, Vec<u8>)> {
            let value = CURRENT_USER.open(path).ok()?.get_value(name).ok()?;
            Some((value.ty(), value.to_vec()))
        }

        fn dword(value: u32) -> (Type, Vec<u8>) {
            (Type::U32, value.to_le_bytes().to_vec())
        }

        #[test]
        fn a_missing_value_is_1_and_marked_during_the_call_and_gone_after_it() {
            let scratch = Scratch::new("missing");
            let during = Cell::new(None);
            let raised = raise_in(&scratch.keys(), || {
                during.set(Some((scratch.stored(), scratch.marked())));
                Ok(())
            });
            assert_eq!(
                raised,
                Ok(Raised {
                    generation: Generation::Unset,
                    cleanup_error: None,
                })
            );
            assert_eq!(during.take(), Some((Some(dword(1)), true)));
            assert_eq!((scratch.stored(), scratch.marked()), (None, false));
        }

        #[test]
        fn a_missing_value_is_deleted_even_when_the_call_fails() {
            let scratch = Scratch::new("failing");
            let raised = raise_in(&scratch.keys(), || Err(io::Error::other("no sas.dll")));
            assert_eq!(
                raised,
                Err(NotRaised::Failed(
                    "cannot call SendSAS after setting SoftwareSASGeneration: no sas.dll".into()
                ))
            );
            assert_eq!((scratch.stored(), scratch.marked()), (None, false));
        }

        #[test]
        fn a_value_that_allows_services_is_called_as_is() {
            for value in [1u32, 3] {
                let scratch = Scratch::new(&format!("allowed-{value}"));
                scratch.set(Type::U32, &value.to_le_bytes());
                let called = Cell::new(false);
                let raised = raise_in(&scratch.keys(), || {
                    called.set(true);
                    Ok(())
                });
                assert_eq!(
                    raised,
                    Ok(Raised {
                        generation: Generation::Allowed(value),
                        cleanup_error: None,
                    })
                );
                assert!(called.get());
                assert_eq!(scratch.stored(), Some(dword(value)));
            }
        }

        #[test]
        fn a_value_that_keeps_services_out_is_never_called_or_written() {
            let string: Vec<u8> = "1\0".encode_utf16().flat_map(u16::to_le_bytes).collect();
            for (ty, data, refused) in [
                (
                    Type::U32,
                    0u32.to_le_bytes().to_vec(),
                    Generation::Disallowed(0),
                ),
                (
                    Type::U32,
                    2u32.to_le_bytes().to_vec(),
                    Generation::Disallowed(2),
                ),
                (Type::String, string, Generation::NotDword),
            ] {
                let scratch = Scratch::new(&format!("disallowed-{ty:?}-{}", data[0]));
                scratch.set(ty, &data);
                let raised = raise_in(&scratch.keys(), || panic!("SendSAS must not be called"));
                assert_eq!(raised, Err(NotRaised::Disallowed(refused)));
                assert_eq!(scratch.stored(), Some((ty, data)));
                assert!(!scratch.marked());
            }
        }

        #[test]
        fn a_1_the_service_left_is_deleted_before_the_next_call_sets_it_again() {
            let scratch = Scratch::new("left");
            scratch.set(Type::U32, &1u32.to_le_bytes());
            scratch.mark();
            let during = Cell::new(None);
            let raised = raise_in(&scratch.keys(), || {
                during.set(Some(scratch.stored()));
                Ok(())
            });
            assert_eq!(
                raised,
                Ok(Raised {
                    generation: Generation::Unset,
                    cleanup_error: None,
                })
            );
            assert_eq!(during.take(), Some(Some(dword(1))));
            assert_eq!((scratch.stored(), scratch.marked()), (None, false));
        }

        #[test]
        fn a_value_an_administrator_wrote_during_the_call_is_kept() {
            let scratch = Scratch::new("rewritten");
            let raised = raise_in(&scratch.keys(), || {
                scratch.set(Type::U32, &0u32.to_le_bytes());
                Ok(())
            });
            assert_eq!(
                raised,
                Ok(Raised {
                    generation: Generation::Unset,
                    cleanup_error: None,
                })
            );
            assert_eq!(
                (scratch.stored(), scratch.marked()),
                (Some(dword(0)), false)
            );
        }

        #[test]
        fn deleting_the_leftover_deletes_only_a_1_the_service_marked() {
            let scratch = Scratch::new("repair");
            assert_eq!(delete_leftover_in(&scratch.keys()), Ok(false));

            scratch.set(Type::U32, &1u32.to_le_bytes());
            assert_eq!(delete_leftover_in(&scratch.keys()), Ok(false));
            assert_eq!(scratch.stored(), Some(dword(1)));

            scratch.mark();
            assert_eq!(delete_leftover_in(&scratch.keys()), Ok(true));
            assert_eq!((scratch.stored(), scratch.marked()), (None, false));

            scratch.set(Type::U32, &0u32.to_le_bytes());
            scratch.mark();
            assert_eq!(delete_leftover_in(&scratch.keys()), Ok(true));
            assert_eq!(
                (scratch.stored(), scratch.marked()),
                (Some(dword(0)), false)
            );
        }
    }
}
