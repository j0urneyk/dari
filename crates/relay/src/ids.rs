//! Persistent assignment of public device IDs to certificate fingerprints.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use dari_net::Fingerprint;
use dari_proto::DeviceId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct IdFile {
    /// Hex certificate fingerprint → device ID.
    ids: BTreeMap<String, u32>,
}

/// Gives every device certificate a stable nine-digit ID, persisted across relay restarts.
#[derive(Debug)]
pub(crate) struct IdStore {
    path: PathBuf,
    file: IdFile,
    taken: HashSet<u32>,
}

fn hex(fingerprint: &Fingerprint) -> String {
    use std::fmt::Write as _;
    fingerprint
        .as_bytes()
        .iter()
        .fold(String::with_capacity(64), |mut text, byte| {
            let _written = write!(text, "{byte:02x}");
            text
        })
}

impl IdStore {
    pub(crate) fn load(path: &Path) -> anyhow::Result<Self> {
        let mut file: IdFile = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => IdFile::default(),
            Err(error) => return Err(error.into()),
        };
        file.ids.retain(|_, id| DeviceId::new(*id).is_some());
        let taken = file.ids.values().copied().collect();
        Ok(Self {
            path: path.to_owned(),
            file,
            taken,
        })
    }

    /// The ID for `fingerprint`, assigning and saving a new one the first time.
    pub(crate) fn get_or_assign(&mut self, fingerprint: &Fingerprint) -> anyhow::Result<DeviceId> {
        let key = hex(fingerprint);
        if let Some(id) = self.file.ids.get(&key).copied().and_then(DeviceId::new) {
            return Ok(id);
        }
        let id = loop {
            let mut random = [0u8; 4];
            getrandom::fill(&mut random)?;
            let span = DeviceId::MAX - DeviceId::MIN + 1;
            let candidate = DeviceId::MIN + u32::from_le_bytes(random) % span;
            if !self.taken.contains(&candidate) {
                break candidate;
            }
        };
        self.file.ids.insert(key, id);
        self.taken.insert(id);
        self.save()?;
        DeviceId::new(id).ok_or_else(|| anyhow::anyhow!("generated an out-of-range ID"))
    }

    fn save(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = self.path.with_extension("tmp");
        std::fs::write(&temporary, toml::to_string(&self.file)?)?;
        std::fs::rename(&temporary, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use dari_net::DeviceIdentity;

    use super::*;

    #[test]
    fn ids_are_stable_and_survive_restarts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ids.toml");
        let first = DeviceIdentity::generate().unwrap().fingerprint();
        let second = DeviceIdentity::generate().unwrap().fingerprint();

        let mut store = IdStore::load(&path).unwrap();
        let first_id = store.get_or_assign(&first).unwrap();
        let second_id = store.get_or_assign(&second).unwrap();
        assert_ne!(first_id, second_id);
        assert_eq!(store.get_or_assign(&first).unwrap(), first_id);

        let mut reloaded = IdStore::load(&path).unwrap();
        assert_eq!(reloaded.get_or_assign(&first).unwrap(), first_id);
        assert_eq!(reloaded.get_or_assign(&second).unwrap(), second_id);
    }
}
