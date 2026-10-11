use windows_registry::{Key, LOCAL_MACHINE, Type};

use crate::local::{POLICY_KEY, POLICY_VALUE, SecureDesktopControl};

/// `HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)`.
const NOT_FOUND: i32 = 0x8007_0002_u32.cast_signed();

impl SecureDesktopControl {
    /// The machine's policy: [`POLICY_VALUE`] under `HKLM\`[`POLICY_KEY`].
    pub fn read() -> Self {
        Self::read_from(LOCAL_MACHINE, POLICY_KEY)
    }

    /// [`POLICY_VALUE`] under `root\path`, through [`SecureDesktopControl::from_stored`]. A value
    /// that isn't a DWORD, or a key that can't be read, is off.
    pub fn read_from(root: &Key, path: &str) -> Self {
        Self::from_stored(
            match root.open(path).and_then(|key| key.get_value(POLICY_VALUE)) {
                Ok(value) if value.ty() == Type::U32 => Some(u32::try_from(value).unwrap_or(0)),
                Err(error) if error.code().0 == NOT_FOUND => None,
                Ok(_) | Err(_) => Some(0),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use windows_registry::CURRENT_USER;

    use super::*;
    use SecureDesktopControl::{Off, On};

    #[test]
    fn missing_or_a_dword_of_1_is_on_and_anything_else_is_off() {
        let path = format!(r"Software\dari-proto-test-policy-{}", std::process::id());
        let _cleared = CURRENT_USER.remove_tree(&path);
        assert_eq!(SecureDesktopControl::read_from(CURRENT_USER, &path), On);

        let key = CURRENT_USER.create(&path).unwrap();
        assert_eq!(SecureDesktopControl::read_from(CURRENT_USER, &path), On);
        key.set_string("", "the key's default value").unwrap();
        assert_eq!(SecureDesktopControl::read_from(CURRENT_USER, &path), On);

        let string_one: Vec<u8> = "1\0".encode_utf16().flat_map(u16::to_le_bytes).collect();
        for (ty, data, control) in [
            (Type::U32, 1u32.to_le_bytes().to_vec(), On),
            (Type::U32, 0u32.to_le_bytes().to_vec(), Off),
            (Type::U32, 2u32.to_le_bytes().to_vec(), Off),
            (Type::U32, u32::MAX.to_le_bytes().to_vec(), Off),
            (Type::U32, vec![1, 0], Off),
            (Type::U64, 1u64.to_le_bytes().to_vec(), Off),
            (Type::String, string_one, Off),
            (Type::Bytes, 1u32.to_le_bytes().to_vec(), Off),
        ] {
            key.set_bytes(POLICY_VALUE, ty, &data).unwrap();
            assert_eq!(
                SecureDesktopControl::read_from(CURRENT_USER, &path),
                control,
                "{ty:?} {data:?}"
            );
        }
        CURRENT_USER.remove_tree(&path).unwrap();
    }
}
