//! One-time access passwords shown on the host and typed on the viewer.

use subtle::ConstantTimeEq;
use thiserror::Error;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// Number of characters in an access password. 10 characters from a 32-symbol alphabet give
/// 50 bits; brute force is impossible because SPAKE2 allows one guess per online attempt and
/// the host rate-limits attempts.
pub const PASSWORD_LEN: usize = 10;

/// Upper-case letters and digits without the look-alikes `I`, `O`, `0`, and `1`.
const ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PasswordError {
    #[error("the password must be {PASSWORD_LEN} characters")]
    WrongLength,
    #[error("the password contains a character that is never used in passwords")]
    InvalidCharacter,
    #[error("the system random number generator failed")]
    Random,
}

/// A one-time access password. Its memory is wiped when dropped.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct AccessPassword(Vec<u8>);

impl AccessPassword {
    pub fn generate() -> Result<Self, PasswordError> {
        let mut random = Zeroizing::new([0u8; PASSWORD_LEN]);
        getrandom::fill(random.as_mut()).map_err(|_| PasswordError::Random)?;
        // 256 is a multiple of 32, so masking the low five bits is unbiased.
        Ok(Self(
            random
                .iter()
                .map(|byte| ALPHABET[usize::from(byte & 0x1f)])
                .collect(),
        ))
    }

    /// Parses what a person typed: case-insensitive, ignoring spaces and dashes.
    pub fn parse(input: &str) -> Result<Self, PasswordError> {
        let normalized: Zeroizing<Vec<u8>> = Zeroizing::new(
            input
                .bytes()
                .filter(|byte| !byte.is_ascii_whitespace() && *byte != b'-')
                .map(|byte| byte.to_ascii_uppercase())
                .collect(),
        );
        if normalized.len() != PASSWORD_LEN {
            return Err(PasswordError::WrongLength);
        }
        if !normalized.iter().all(|byte| ALPHABET.contains(byte)) {
            return Err(PasswordError::InvalidCharacter);
        }
        Ok(Self(normalized.to_vec()))
    }

    /// The password split into two dash-separated groups for display, e.g. `K7MXQ-3PTWA`.
    pub fn display_text(&self) -> Zeroizing<String> {
        let (head, tail) = self.0.split_at(PASSWORD_LEN / 2);
        let mut text = Zeroizing::new(String::with_capacity(PASSWORD_LEN + 1));
        text.extend(head.iter().map(|byte| char::from(*byte)));
        text.push('-');
        text.extend(tail.iter().map(|byte| char::from(*byte)));
        text
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl PartialEq for AccessPassword {
    fn eq(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0).into()
    }
}

impl Eq for AccessPassword {}

impl std::fmt::Debug for AccessPassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AccessPassword(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_passwords_use_the_alphabet() {
        for _ in 0..200 {
            let password = AccessPassword::generate().unwrap();
            assert_eq!(password.as_bytes().len(), PASSWORD_LEN);
            assert!(
                password
                    .as_bytes()
                    .iter()
                    .all(|byte| ALPHABET.contains(byte))
            );
        }
    }

    #[test]
    fn generated_passwords_differ() {
        let first = AccessPassword::generate().unwrap();
        let second = AccessPassword::generate().unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn parsing_is_forgiving_about_formatting() {
        let password = AccessPassword::generate().unwrap();
        let shown = password.display_text();
        assert_eq!(AccessPassword::parse(&shown).unwrap(), password);
        let typed = format!("  {} ", shown.to_lowercase().replace('-', " "));
        assert_eq!(AccessPassword::parse(&typed).unwrap(), password);
    }

    #[test]
    fn parsing_rejects_malformed_input() {
        assert_eq!(
            AccessPassword::parse("ABCDE"),
            Err(PasswordError::WrongLength)
        );
        assert_eq!(
            AccessPassword::parse("ABCDE-FGHI0"),
            Err(PasswordError::InvalidCharacter)
        );
    }

    #[test]
    fn debug_output_is_redacted() {
        let password = AccessPassword::parse("ABCDE-FGHJK").unwrap();
        assert!(!format!("{password:?}").contains("ABCDE"));
    }
}
