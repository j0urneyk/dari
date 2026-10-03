//! File transfer messages and file name rules.
//!
//! Either side offers a file or a folder on the control stream; the receiver accepts or declines
//! it, and an accepted offer's bytes travel on their own [`StreamKind::File`](crate::StreamKind)
//! stream: one file's bytes, or every file of a folder one after another in the offer's order.

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::validate::{ValidationError, is_invisible_format};

/// Longest file name a peer may offer, in UTF-8 bytes (the macOS and Windows component limit).
pub const MAX_FILE_NAME_BYTES: usize = 255;
/// Most files one folder offer may hold.
pub const MAX_FOLDER_FILES: usize = 10_000;
/// Deepest a file may sit inside an offered folder, in path components.
pub const MAX_FOLDER_DEPTH: usize = 32;
/// Most bytes all path components of one folder offer may add up to, keeping the offer well
/// inside the control channel's frame limit.
pub const MAX_FOLDER_PATH_BYTES: usize = 1024 * 1024;

/// Longest name [`sanitize_file_name`] produces, leaving room for a ` (n)` suffix and `.part`.
const SANITIZED_FILE_NAME_BYTES: usize = 200;

/// Identifies one transfer within a session. The offering side picks it: hosts use even ids
/// and viewers odd ones, so offers from both directions never collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TransferId(pub u64);

impl TransferId {
    /// The `index`-th id offered by the host (`by_host`) or the viewer.
    pub const fn new(index: u64, by_host: bool) -> TransferId {
        TransferId(index * 2 + if by_host { 0 } else { 1 })
    }

    pub const fn offered_by_host(self) -> bool {
        self.0.is_multiple_of(2)
    }
}

impl std::fmt::Display for TransferId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// A file or folder the sender would like to transfer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileOffer {
    pub id: TransferId,
    /// The file's or folder's name without any directory, NFC-normalized.
    pub name: String,
    /// Bytes to transfer: the file's size, or the sum of the folder's files.
    pub size: u64,
    /// `None` for a file; for a folder, its files in the order their bytes are sent. Empty
    /// subfolders are not part of it.
    pub contents: Option<Vec<FolderFile>>,
}

/// One file inside an offered folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderFile {
    /// Path below the folder, one NFC-normalized name per component.
    pub path: Vec<String>,
    pub size: u64,
}

/// Checks an offer's names and, for a folder, its shape: every path component is a plain name,
/// paths are 1..=[`MAX_FOLDER_DEPTH`] deep, and the sizes add up to `size`.
pub(crate) fn validate_offer(offer: &FileOffer) -> Result<(), ValidationError> {
    validate_file_name(&offer.name)?;
    let Some(contents) = &offer.contents else {
        return Ok(());
    };
    if contents.len() > MAX_FOLDER_FILES {
        return Err(ValidationError::InvalidValue { field: "folder" });
    }
    let mut total = 0u64;
    let mut path_bytes = 0usize;
    for file in contents {
        if file.path.is_empty() || file.path.len() > MAX_FOLDER_DEPTH {
            return Err(ValidationError::InvalidValue { field: "folder" });
        }
        for component in &file.path {
            validate_file_name(component)?;
            path_bytes += component.len();
        }
        total = total
            .checked_add(file.size)
            .ok_or(ValidationError::InvalidValue { field: "folder" })?;
    }
    if path_bytes > MAX_FOLDER_PATH_BYTES || total != offer.size {
        return Err(ValidationError::InvalidValue { field: "folder" });
    }
    Ok(())
}

/// Why a transfer stopped before completing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferEnd {
    /// The receiver (or its policy) refused the offer.
    Declined,
    /// A user on either side stopped it.
    Cancelled,
    /// Reading, sending, or saving the file failed.
    Failed,
}

/// Checks a peer-offered file name: one path component, with nothing that could render it as
/// a different name. Receivers still map it onto the local file system with
/// [`sanitize_file_name`].
pub(crate) fn validate_file_name(name: &str) -> Result<(), ValidationError> {
    const FIELD: &str = "file name";
    if name.len() > MAX_FILE_NAME_BYTES {
        return Err(ValidationError::TooLong {
            field: FIELD,
            max: MAX_FILE_NAME_BYTES,
        });
    }
    if name
        .chars()
        .any(|character| character.is_control() || is_invisible_format(character))
    {
        return Err(ValidationError::ControlCharacters { field: FIELD });
    }
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        return Err(ValidationError::InvalidValue { field: FIELD });
    }
    Ok(())
}

/// Turns any name into one that is safe to create on macOS and Windows alike: NFC-normalized
/// (macOS hands out decomposed names, which Windows shows as separated jamo), without path
/// separators, characters Windows forbids, trailing dots or spaces, or reserved device names,
/// and short enough to take a ` (n)` suffix.
pub fn sanitize_file_name(name: &str) -> String {
    let normalized: String = name
        .nfc()
        .filter(|character| !character.is_control() && !is_invisible_format(*character))
        .map(|character| {
            if matches!(
                character,
                '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*'
            ) {
                '_'
            } else {
                character
            }
        })
        .collect();
    let trimmed = normalized
        .trim_start_matches(' ')
        .trim_end_matches(['.', ' ']);
    let mut sanitized = truncate_keeping_extension(trimmed, SANITIZED_FILE_NAME_BYTES);
    if sanitized.is_empty() {
        sanitized = "file".into();
    }
    if is_reserved_device_name(&sanitized) {
        sanitized.insert(0, '_');
    }
    sanitized
}

/// Splits `name` into stem and extension (with its dot); dotfiles have no extension.
pub fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    }
}

fn truncate_keeping_extension(name: &str, max_bytes: usize) -> String {
    if name.len() <= max_bytes {
        return name.into();
    }
    let (stem, extension) = split_extension(name);
    // An absurdly long "extension" is just part of the name.
    let extension = if extension.len() <= 16 { extension } else { "" };
    let mut stem_bytes = max_bytes - extension.len();
    while !stem.is_char_boundary(stem_bytes.min(stem.len())) {
        stem_bytes -= 1;
    }
    let stem = stem[..stem_bytes.min(stem.len())].trim_end_matches(['.', ' ']);
    format!("{stem}{extension}")
}

/// Windows device names, which can't be used as a file name with any extension.
fn is_reserved_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end();
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.len() == 4
            && upper.as_bytes()[3].is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_split_by_who_offers_them() {
        assert!(TransferId::new(0, true).offered_by_host());
        assert!(TransferId::new(5, true).offered_by_host());
        assert!(!TransferId::new(0, false).offered_by_host());
        assert_ne!(TransferId::new(3, true), TransferId::new(3, false));
    }

    #[test]
    fn offered_names_must_be_a_single_plain_component() {
        assert!(validate_file_name("report final.pdf").is_ok());
        assert!(validate_file_name("보고서.hwp").is_ok());
        for bad in [
            "",
            ".",
            "..",
            "../etc/passwd",
            "a/b",
            "a\\b",
            "x\0y",
            "a\nb",
        ] {
            assert!(validate_file_name(bad).is_err(), "{bad:?}");
        }
        // Right-to-left override: "photo\u{202E}gnp.exe" renders as "photoexe.png".
        assert!(validate_file_name("photo\u{202E}gnp.exe").is_err());
        assert!(validate_file_name(&"a".repeat(MAX_FILE_NAME_BYTES + 1)).is_err());
    }

    fn folder(files: &[(&[&str], u64)]) -> FileOffer {
        FileOffer {
            id: TransferId(1),
            name: "사진".into(),
            size: files.iter().map(|(_, size)| size).sum(),
            contents: Some(
                files
                    .iter()
                    .map(|(path, size)| FolderFile {
                        path: path.iter().map(|component| (*component).into()).collect(),
                        size: *size,
                    })
                    .collect(),
            ),
        }
    }

    #[test]
    fn folder_offers_hold_only_plain_relative_paths() {
        assert!(validate_offer(&folder(&[(&["a.jpg"], 3), (&["2026", "b.jpg"], 4)])).is_ok());
        assert!(validate_offer(&folder(&[])).is_ok(), "an empty folder");
        for bad in [&[".."][..], &["a", "..", "b"], &["a/b"], &["C:\\x"], &[]] {
            assert!(validate_offer(&folder(&[(bad, 1)])).is_err(), "{bad:?}");
        }
        let deep = vec!["d"; MAX_FOLDER_DEPTH + 1];
        assert!(validate_offer(&folder(&[(&deep, 1)])).is_err());
    }

    #[test]
    fn folder_sizes_must_add_up() {
        let mut offer = folder(&[(&["a"], 3), (&["b"], 4)]);
        offer.size = 8;
        assert!(validate_offer(&offer).is_err());
        let overflowing = folder(&[(&["a"], u64::MAX)]);
        let mut overflowing = FileOffer {
            contents: overflowing.contents.map(|mut files| {
                files.push(FolderFile {
                    path: vec!["b".into()],
                    size: 1,
                });
                files
            }),
            ..overflowing
        };
        overflowing.size = 0;
        assert!(validate_offer(&overflowing).is_err());
    }

    #[test]
    fn decomposed_hangul_is_composed() {
        // "한글" as macOS file systems store it: conjoining jamo.
        let decomposed = "\u{1112}\u{1161}\u{11AB}\u{1100}\u{1173}\u{11AF}.txt";
        assert_eq!(sanitize_file_name(decomposed), "한글.txt");
    }

    #[test]
    fn names_are_safe_on_windows() {
        assert_eq!(sanitize_file_name("a<b>c:d\"e|f?g*h"), "a_b_c_d_e_f_g_h");
        assert_eq!(sanitize_file_name("notes. . "), "notes");
        assert_eq!(sanitize_file_name("  padded.txt"), "padded.txt");
        assert_eq!(sanitize_file_name("CON"), "_CON");
        assert_eq!(sanitize_file_name("nul.txt"), "_nul.txt");
        assert_eq!(sanitize_file_name("com1.log"), "_com1.log");
        assert_eq!(sanitize_file_name("console.txt"), "console.txt");
        assert_eq!(sanitize_file_name("..."), "file");
        assert_eq!(sanitize_file_name("bidi\u{202E}txt.exe"), "biditxt.exe");
        assert_eq!(sanitize_file_name("../../x"), ".._.._x");
    }

    #[test]
    fn long_names_keep_their_extension() {
        let long = format!("{}.pdf", "가".repeat(100));
        let sanitized = sanitize_file_name(&long);
        assert!(sanitized.len() <= SANITIZED_FILE_NAME_BYTES);
        assert!(sanitized.ends_with("가.pdf"));
        assert!(validate_file_name(&sanitized).is_ok());
    }

    #[test]
    fn extensions_split_at_the_last_dot() {
        assert_eq!(split_extension("a.tar.gz"), ("a.tar", ".gz"));
        assert_eq!(split_extension(".bashrc"), (".bashrc", ""));
        assert_eq!(split_extension("README"), ("README", ""));
    }
}
