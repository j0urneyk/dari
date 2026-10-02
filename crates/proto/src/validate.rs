use thiserror::Error;

/// Longest device name, in characters, a peer may announce.
pub const MAX_DEVICE_NAME_CHARS: usize = 64;

/// Semantic checks applied to every decoded message, after the frame-size limit.
///
/// Frame limits bound memory; `Validate` rejects values that are well-formed postcard but
/// outside what the protocol allows (over-long names, empty payloads, out-of-range values).
pub trait Validate {
    fn validate(&self) -> Result<(), ValidationError>;
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("{field} is longer than {max} characters")]
    TooLong { field: &'static str, max: usize },
    #[error("{field} contains control characters")]
    ControlCharacters { field: &'static str },
    #[error("{field} has an invalid value")]
    InvalidValue { field: &'static str },
}

pub(crate) fn validate_display_text(
    field: &'static str,
    value: &str,
    max_chars: usize,
) -> Result<(), ValidationError> {
    if value.chars().count() > max_chars {
        return Err(ValidationError::TooLong {
            field,
            max: max_chars,
        });
    }
    if value.chars().any(char::is_control) {
        return Err(ValidationError::ControlCharacters { field });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_text_rejects_long_and_control_values() {
        assert!(validate_display_text("name", "MacBook Pro", 64).is_ok());
        assert_eq!(
            validate_display_text("name", &"x".repeat(65), 64),
            Err(ValidationError::TooLong {
                field: "name",
                max: 64
            })
        );
        assert_eq!(
            validate_display_text("name", "evil\u{1b}[2J", 64),
            Err(ValidationError::ControlCharacters { field: "name" })
        );
    }
}
