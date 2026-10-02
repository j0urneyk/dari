//! Persistent user settings.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::DEFAULT_PORT;

const SETTINGS_FILE: &str = "settings.toml";
/// Recent addresses remembered on the connect form.
const MAX_RECENT: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Whether this device accepts viewers when the app starts.
    pub hosting_enabled: bool,
    pub port: u16,
    /// Translate ⌘ and Ctrl when controlling a device with a different OS.
    pub map_shortcut_modifier: bool,
    pub recent_addresses: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hosting_enabled: true,
            port: DEFAULT_PORT,
            map_shortcut_modifier: true,
            recent_addresses: Vec::new(),
        }
    }
}

impl Settings {
    /// Loads settings, falling back to defaults when the file is missing or unreadable.
    pub(crate) fn load(directory: &Path) -> Self {
        let path = directory.join(SETTINGS_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|error| {
                tracing::warn!(%error, ?path, "ignoring unreadable settings");
                Self::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                tracing::warn!(%error, ?path, "cannot read settings");
                Self::default()
            }
        }
    }

    pub fn save(&self, directory: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(directory)?;
        let path: PathBuf = directory.join(SETTINGS_FILE);
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, toml::to_string_pretty(self)?)?;
        std::fs::rename(&temporary, &path)?;
        Ok(())
    }

    /// Moves `address` to the front of the recent list.
    pub(crate) fn remember_address(&mut self, address: &str) {
        let address = address.trim();
        if address.is_empty() {
            return;
        }
        self.recent_addresses.retain(|recent| recent != address);
        self.recent_addresses.insert(0, address.to_owned());
        self.recent_addresses.truncate(MAX_RECENT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_through_disk() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(Settings::load(directory.path()), Settings::default());
        let mut settings = Settings {
            hosting_enabled: false,
            ..Settings::default()
        };
        settings.remember_address("10.0.0.2");
        settings.save(directory.path()).unwrap();
        assert_eq!(Settings::load(directory.path()), settings);
    }

    #[test]
    fn unreadable_settings_fall_back_to_defaults() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join(SETTINGS_FILE), "port = \"oops\"").unwrap();
        assert_eq!(Settings::load(directory.path()), Settings::default());
    }

    #[test]
    fn recent_addresses_are_unique_and_bounded() {
        let mut settings = Settings::default();
        for address in ["a", "b", "c", "d", "e", "f", "b"] {
            settings.remember_address(address);
        }
        assert_eq!(settings.recent_addresses, ["b", "f", "e", "d", "c"]);
    }
}
