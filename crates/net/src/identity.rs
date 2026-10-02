//! The device's long-lived TLS identity: a self-signed certificate and its private key.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};
use thiserror::Error;
use zeroize::Zeroizing;

/// Subject name in every Dari certificate; also the TLS server name viewers send.
pub(crate) const CERTIFICATE_SUBJECT: &str = "dari";

const CERTIFICATE_FILE: &str = "identity-cert.der";
const PRIVATE_KEY_FILE: &str = "identity-key.der";

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("failed to generate the device certificate: {0}")]
    Generate(#[from] rcgen::Error),
    #[error("failed to access {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
}

/// SHA-256 digest of a device certificate, used to recognize a device.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    pub fn of_certificate(certificate: &[u8]) -> Self {
        Self(Sha256::digest(certificate).into())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Display for Fingerprint {
    /// Upper-case hex in groups of four, e.g. `3F2A 9C01 ...`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, pair) in self.0.chunks(2).enumerate() {
            if index > 0 {
                f.write_str(" ")?;
            }
            for byte in pair {
                write!(f, "{byte:02X}")?;
            }
        }
        Ok(())
    }
}

impl std::fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

/// A device's certificate and private key.
pub struct DeviceIdentity {
    certificate: CertificateDer<'static>,
    private_key: Zeroizing<Vec<u8>>,
}

impl std::fmt::Debug for DeviceIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceIdentity")
            .field("fingerprint", &self.fingerprint())
            .finish_non_exhaustive()
    }
}

impl DeviceIdentity {
    /// Creates a fresh self-signed identity.
    pub fn generate() -> Result<Self, IdentityError> {
        let generated = rcgen::generate_simple_self_signed(vec![CERTIFICATE_SUBJECT.to_owned()])?;
        Ok(Self {
            certificate: generated.cert.der().clone(),
            private_key: Zeroizing::new(generated.signing_key.serialize_der()),
        })
    }

    /// Loads the identity stored in `directory`, creating and saving a new one if none exists.
    ///
    /// The private key file is only readable by the current user on Unix. On Windows the
    /// per-user application data directory already restricts access to the user.
    pub fn load_or_generate(directory: &Path) -> Result<Self, IdentityError> {
        let certificate_path = directory.join(CERTIFICATE_FILE);
        let key_path = directory.join(PRIVATE_KEY_FILE);
        if let (Some(certificate), Some(private_key)) =
            (read_file(&certificate_path)?, read_file(&key_path)?)
        {
            let identity = Self {
                certificate: CertificateDer::from(certificate),
                private_key: Zeroizing::new(private_key),
            };
            if identity.is_usable() {
                return Ok(identity);
            }
            tracing::warn!(
                ?directory,
                "stored device identity is unusable; creating a new one"
            );
        }
        let identity = Self::generate()?;
        fs::create_dir_all(directory).map_err(|source| IdentityError::Io {
            path: directory.to_owned(),
            source,
        })?;
        write_private_file(&key_path, &identity.private_key)?;
        write_private_file(&certificate_path, &identity.certificate)?;
        Ok(identity)
    }

    /// Whether the key parses and belongs to the certificate, so TLS setup will succeed.
    fn is_usable(&self) -> bool {
        let Ok(key) = rustls::crypto::ring::sign::any_supported_type(&self.private_key()) else {
            return false;
        };
        rustls::sign::CertifiedKey::new(vec![self.certificate()], key)
            .keys_match()
            .is_ok()
    }

    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::of_certificate(&self.certificate)
    }

    pub(crate) fn certificate(&self) -> CertificateDer<'static> {
        self.certificate.clone()
    }

    pub(crate) fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.private_key.to_vec()))
    }
}

fn read_file(path: &Path) -> Result<Option<Vec<u8>>, IdentityError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(IdentityError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

/// Writes `contents` to `path` via a temporary file so a crash never leaves a torn file.
fn write_private_file(path: &Path, contents: &[u8]) -> Result<(), IdentityError> {
    let io_error = |source| IdentityError::Io {
        path: path.to_owned(),
        source,
    };
    let temporary = path.with_extension("tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(io_error)?;
    file.write_all(contents).map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    drop(file);
    fs::rename(&temporary, path).map_err(io_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_persists_across_loads() {
        let directory = tempfile::tempdir().unwrap();
        let first = DeviceIdentity::load_or_generate(directory.path()).unwrap();
        let second = DeviceIdentity::load_or_generate(directory.path()).unwrap();
        assert_eq!(first.fingerprint(), second.fingerprint());
    }

    #[test]
    fn corrupt_identity_is_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let first = DeviceIdentity::load_or_generate(directory.path()).unwrap();
        fs::write(directory.path().join(PRIVATE_KEY_FILE), b"not a key").unwrap();
        let second = DeviceIdentity::load_or_generate(directory.path()).unwrap();
        assert_ne!(first.fingerprint(), second.fingerprint());
        assert!(second.is_usable());
    }

    #[cfg(unix)]
    #[test]
    fn private_key_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        DeviceIdentity::load_or_generate(directory.path()).unwrap();
        let mode = fs::metadata(directory.path().join(PRIVATE_KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn fingerprint_formats_as_grouped_hex() {
        let fingerprint = Fingerprint([0xAB; 32]);
        let text = fingerprint.to_string();
        assert!(text.starts_with("ABAB ABAB "));
        assert_eq!(text.split(' ').count(), 16);
    }
}
