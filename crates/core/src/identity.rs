//! Device identity: one long-lived Ed25519 key per device.
//!
//! The public half is the device's id. Every connection is encrypted to and
//! authenticated by these keys, so "who am I talking to" is always answered
//! cryptographically, never by a name or an IP address.

use std::{
    fmt, fs,
    path::{Path, PathBuf},
};

use iroh::SecretKey;

use crate::error::{Error, Result};

/// A device's public key. Safe to show, share and store.
pub type DeviceId = iroh::EndpointId;

/// Where the secret key lives. Each platform plugs in its own secure storage
/// (OS keychain on desktop, Keystore on Android); [`FileKeyStore`] is the
/// plain-file fallback used by the CLI and tests.
pub trait KeyStore: Send + Sync {
    /// Returns the stored secret key, or `None` on first run.
    fn load(&self) -> Result<Option<[u8; 32]>>;
    /// Persists the secret key.
    fn store(&self, key: &[u8; 32]) -> Result<()>;
}

/// Stores the key in a file readable only by the current user.
#[derive(Debug, Clone)]
pub struct FileKeyStore {
    path: PathBuf,
}

impl FileKeyStore {
    /// Uses `identity.key` inside `data_dir`.
    pub fn in_dir(data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: data_dir.as_ref().join("identity.key"),
        }
    }

    /// The file the key is kept in.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl KeyStore for FileKeyStore {
    fn load(&self) -> Result<Option<[u8; 32]>> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let key: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            Error::CorruptIdentity(format!(
                "{} holds {} bytes, expected 32",
                self.path.display(),
                bytes.len()
            ))
        })?;
        Ok(Some(key))
    }

    fn store(&self, key: &[u8; 32]) -> Result<()> {
        // Never world-readable, never half-written.
        crate::fsutil::write_private(&self.path, key)?;
        Ok(())
    }
}

/// This device's secret key.
#[derive(Clone)]
pub struct Identity {
    secret: SecretKey,
}

impl Identity {
    /// Creates a brand new identity without storing it.
    pub fn generate() -> Self {
        Self {
            secret: SecretKey::generate(),
        }
    }

    /// Loads the identity from `store`, creating and saving one on first run.
    pub fn load_or_create(store: &dyn KeyStore) -> Result<Self> {
        if let Some(bytes) = store.load()? {
            return Ok(Self {
                secret: SecretKey::from_bytes(&bytes),
            });
        }
        let identity = Self::generate();
        store.store(&identity.secret.to_bytes())?;
        Ok(identity)
    }

    /// The public id other devices know this device by.
    pub fn id(&self) -> DeviceId {
        self.secret.public()
    }

    pub(crate) fn secret_key(&self) -> &SecretKey {
        &self.secret
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print the secret.
        f.debug_struct("Identity")
            .field("id", &self.id().fmt_short().to_string())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileKeyStore::in_dir(dir.path());
        let first = Identity::load_or_create(&store).unwrap();
        let second = Identity::load_or_create(&store).unwrap();
        assert_eq!(first.id(), second.id());
    }

    #[test]
    fn different_devices_get_different_ids() {
        assert_ne!(Identity::generate().id(), Identity::generate().id());
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = FileKeyStore::in_dir(dir.path().join("nested"));
        Identity::load_or_create(&store).unwrap();
        let mode = fs::metadata(store.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn corrupt_key_file_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileKeyStore::in_dir(dir.path());
        fs::write(store.path(), b"too short").unwrap();
        assert!(matches!(
            Identity::load_or_create(&store),
            Err(Error::CorruptIdentity(_))
        ));
    }

    #[test]
    fn debug_output_hides_the_secret() {
        let identity = Identity::generate();
        let secret_hex: String = identity
            .secret_key()
            .to_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(!format!("{identity:?}").contains(&secret_hex));
    }
}
