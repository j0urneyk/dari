use windows_registry::{Key, LOCAL_MACHINE, Type, Value};

use crate::local::{POLICY_KEY, POLICY_VALUE, PolicyReading, SecureDesktopControl};

/// `HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)`.
const NOT_FOUND: i32 = 0x8007_0002_u32.cast_signed();
/// `HRESULT_FROM_WIN32(ERROR_KEY_DELETED)`.
const KEY_DELETED: i32 = 0x8007_03fa_u32.cast_signed();

impl SecureDesktopControl {
    /// The machine's policy: the [`PolicyReading::control`] of [`PolicyReading::read`].
    pub fn read() -> Self {
        PolicyReading::read().control()
    }
}

impl PolicyReading {
    /// [`POLICY_VALUE`] under `HKLM\`[`POLICY_KEY`].
    pub fn read() -> Self {
        Self::read_from(LOCAL_MACHINE, POLICY_KEY)
    }

    /// [`POLICY_VALUE`] under `root\path`.
    pub fn read_from(root: &Key, path: &str) -> Self {
        Self::parse(root.open(path).and_then(|key| key.get_value(POLICY_VALUE)))
    }

    /// A key deleted while it was open reads as missing: Group Policy deletes and rewrites its
    /// keys on every refresh, and a policy it is rewriting hasn't changed.
    fn parse(value: windows_registry::Result<Value>) -> Self {
        match value {
            Ok(value) if value.ty() == Type::U32 => {
                u32::try_from(value).map_or(Self::WrongType, Self::Stored)
            }
            Ok(_) => Self::WrongType,
            Err(error) if matches!(error.code().0, NOT_FOUND | KEY_DELETED) => Self::Missing,
            Err(error) => Self::Unreadable(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use windows_registry::CURRENT_USER;

    use super::*;

    fn scratch(name: &str) -> String {
        let path = format!(r"Software\dari-proto-test-{name}-{}", std::process::id());
        let _cleared = CURRENT_USER.remove_tree(&path);
        path
    }

    #[test]
    fn the_reading_names_what_is_stored() {
        let path = scratch("policy");
        let read = || PolicyReading::read_from(CURRENT_USER, &path);
        assert_eq!(read(), PolicyReading::Missing);

        let key = CURRENT_USER.create(&path).unwrap();
        assert_eq!(read(), PolicyReading::Missing);
        key.set_string("", "the key's default value").unwrap();
        assert_eq!(read(), PolicyReading::Missing);

        let string_one: Vec<u8> = "1\0".encode_utf16().flat_map(u16::to_le_bytes).collect();
        for (ty, data, reading) in [
            (
                Type::U32,
                1u32.to_le_bytes().to_vec(),
                PolicyReading::Stored(1),
            ),
            (
                Type::U32,
                0u32.to_le_bytes().to_vec(),
                PolicyReading::Stored(0),
            ),
            (
                Type::U32,
                2u32.to_le_bytes().to_vec(),
                PolicyReading::Stored(2),
            ),
            (
                Type::U32,
                u32::MAX.to_le_bytes().to_vec(),
                PolicyReading::Stored(u32::MAX),
            ),
            (Type::U32, vec![1, 0], PolicyReading::WrongType),
            (
                Type::U64,
                1u64.to_le_bytes().to_vec(),
                PolicyReading::WrongType,
            ),
            (Type::String, string_one, PolicyReading::WrongType),
            (
                Type::Bytes,
                1u32.to_le_bytes().to_vec(),
                PolicyReading::WrongType,
            ),
        ] {
            key.set_bytes(POLICY_VALUE, ty, &data).unwrap();
            assert_eq!(read(), reading, "{ty:?} {data:?}");
        }
        CURRENT_USER.remove_tree(&path).unwrap();
    }

    #[test]
    fn a_key_deleted_while_open_reads_as_missing() {
        let path = scratch("deleted-policy");
        let key = CURRENT_USER.create(&path).unwrap();
        key.set_u32(POLICY_VALUE, 0).unwrap();
        CURRENT_USER.remove_tree(&path).unwrap();
        let value = key.get_value(POLICY_VALUE);
        assert_eq!(
            value.as_ref().err().map(|error| error.code().0),
            Some(KEY_DELETED)
        );
        assert_eq!(PolicyReading::parse(value), PolicyReading::Missing);
    }

    #[test]
    fn any_other_error_is_unreadable_and_names_itself() {
        let path = format!(r"Software\{}", "a".repeat(300));
        let reading = PolicyReading::read_from(CURRENT_USER, &path);
        assert!(
            matches!(&reading, PolicyReading::Unreadable(error) if error.contains("0x")),
            "{reading:?}"
        );
        assert_eq!(reading.control(), SecureDesktopControl::Off);
    }
}
