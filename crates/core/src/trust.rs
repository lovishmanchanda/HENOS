//! The list of paired devices: the only devices this one will talk to.
//!
//! A device gets on the list by completing pairing and leaves it by being
//! unpaired. The list is saved to a file readable only by the current user.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    device::{DeviceInfo, Platform},
    error::{Error, Result},
    identity::DeviceId,
};

const FILE_NAME: &str = "paired.json";
const FILE_VERSION: u32 = 1;

/// A device we have paired with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedDevice {
    pub id: DeviceId,
    /// Name and platform as the device described itself when pairing.
    pub device: DeviceInfo,
    /// When pairing completed, in seconds since the Unix epoch.
    pub paired_at: u64,
}

impl PairedDevice {
    /// A record for a device paired right now.
    pub fn new(id: DeviceId, device: DeviceInfo) -> Self {
        Self {
            id,
            device,
            paired_at: unix_now(),
        }
    }
}

/// Seconds since the Unix epoch.
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The paired-device list, optionally backed by a file.
#[derive(Debug)]
pub struct TrustStore {
    path: Option<PathBuf>,
    devices: Mutex<HashMap<DeviceId, PairedDevice>>,
}

#[derive(Serialize, Deserialize)]
struct StoredFile {
    version: u32,
    devices: Vec<StoredDevice>,
}

#[derive(Serialize, Deserialize)]
struct StoredDevice {
    id: String,
    name: String,
    platform: String,
    paired_at: u64,
}

impl TrustStore {
    /// An empty list that is never saved. For tests and throwaway nodes.
    pub fn in_memory() -> Self {
        Self {
            path: None,
            devices: Mutex::default(),
        }
    }

    /// An unsaved list that already trusts `ids`, as if paired long ago.
    pub fn in_memory_with(ids: impl IntoIterator<Item = DeviceId>) -> Self {
        let devices = ids
            .into_iter()
            .map(|id| {
                let device = DeviceInfo {
                    name: format!("Device {}", id.fmt_short()),
                    platform: Platform::Unknown,
                };
                (
                    id,
                    PairedDevice {
                        id,
                        device,
                        paired_at: 0,
                    },
                )
            })
            .collect();
        Self {
            path: None,
            devices: Mutex::new(devices),
        }
    }

    /// Opens the list stored in `data_dir`, starting empty on first run.
    ///
    /// A file that exists but cannot be understood is an error rather than an
    /// empty list, so that damage is noticed instead of silently unpairing
    /// everything.
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self> {
        let path = data_dir.as_ref().join(FILE_NAME);
        let devices = match fs::read(&path) {
            Ok(bytes) => parse(&bytes, &path)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            path: Some(path),
            devices: Mutex::new(devices),
        })
    }

    /// Whether `id` belongs to a paired device.
    pub fn is_trusted(&self, id: &DeviceId) -> bool {
        self.devices.lock().unwrap().contains_key(id)
    }

    /// The record for a paired device.
    pub fn get(&self, id: &DeviceId) -> Option<PairedDevice> {
        self.devices.lock().unwrap().get(id).cloned()
    }

    /// All paired devices, oldest pairing first.
    pub fn list(&self) -> Vec<PairedDevice> {
        let mut list: Vec<_> = self.devices.lock().unwrap().values().cloned().collect();
        list.sort_by(|a, b| (a.paired_at, a.id.as_bytes()).cmp(&(b.paired_at, b.id.as_bytes())));
        list
    }

    /// Adds a device, or replaces its record, and saves the list. If saving
    /// fails the device is not trusted.
    pub fn add(&self, device: PairedDevice) -> Result<()> {
        let mut devices = self.devices.lock().unwrap();
        let previous = devices.insert(device.id, device.clone());
        if let Err(e) = self.save(&devices) {
            match previous {
                Some(previous) => devices.insert(device.id, previous),
                None => devices.remove(&device.id),
            };
            return Err(e);
        }
        Ok(())
    }

    /// Removes a device and saves the list. Returns whether it was paired.
    /// If saving fails the device stays removed in memory: when in doubt, do
    /// not trust.
    pub fn remove(&self, id: &DeviceId) -> Result<bool> {
        let mut devices = self.devices.lock().unwrap();
        if devices.remove(id).is_none() {
            return Ok(false);
        }
        self.save(&devices)?;
        Ok(true)
    }

    fn save(&self, devices: &HashMap<DeviceId, PairedDevice>) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let mut stored: Vec<StoredDevice> = devices
            .values()
            .map(|d| StoredDevice {
                id: d.id.to_string(),
                name: d.device.name.clone(),
                platform: d.device.platform.as_str().to_owned(),
                paired_at: d.paired_at,
            })
            .collect();
        stored.sort_by(|a, b| (a.paired_at, &a.id).cmp(&(b.paired_at, &b.id)));
        let file = StoredFile {
            version: FILE_VERSION,
            devices: stored,
        };
        let json = serde_json::to_vec_pretty(&file)
            .map_err(|e| Error::CorruptTrustStore(e.to_string()))?;
        crate::fsutil::write_private(path, &json)?;
        Ok(())
    }
}

fn parse(bytes: &[u8], path: &Path) -> Result<HashMap<DeviceId, PairedDevice>> {
    let corrupt = |why: String| Error::CorruptTrustStore(format!("{}: {why}", path.display()));
    let file: StoredFile = serde_json::from_slice(bytes).map_err(|e| corrupt(e.to_string()))?;
    if file.version != FILE_VERSION {
        return Err(corrupt(format!("unsupported version {}", file.version)));
    }
    let mut devices = HashMap::new();
    for stored in file.devices {
        let id: DeviceId = stored
            .id
            .parse()
            .map_err(|_| corrupt(format!("invalid device id {:?}", stored.id)))?;
        let device = DeviceInfo {
            name: stored.name,
            platform: Platform::parse(&stored.platform),
        }
        .sanitized();
        devices.insert(
            id,
            PairedDevice {
                id,
                device,
                paired_at: stored.paired_at,
            },
        );
    }
    Ok(devices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;

    fn device(name: &str) -> PairedDevice {
        PairedDevice::new(
            Identity::generate().id(),
            DeviceInfo {
                name: name.to_owned(),
                platform: Platform::Android,
            },
        )
    }

    #[test]
    fn pairings_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let phone = device("Phone");
        let laptop = device("Laptop");
        {
            let store = TrustStore::open(dir.path()).unwrap();
            assert!(store.list().is_empty());
            store.add(phone.clone()).unwrap();
            store.add(laptop.clone()).unwrap();
        }
        let store = TrustStore::open(dir.path()).unwrap();
        assert!(store.is_trusted(&phone.id));
        assert_eq!(store.get(&laptop.id), Some(laptop.clone()));
        assert_eq!(store.list().len(), 2);

        assert!(store.remove(&phone.id).unwrap());
        assert!(!store.remove(&phone.id).unwrap());
        let store = TrustStore::open(dir.path()).unwrap();
        assert!(!store.is_trusted(&phone.id));
        assert!(store.is_trusted(&laptop.id));
    }

    #[test]
    fn unknown_devices_are_not_trusted() {
        let store = TrustStore::in_memory();
        assert!(!store.is_trusted(&Identity::generate().id()));
    }

    #[test]
    fn damaged_file_is_an_error_not_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(FILE_NAME), b"{ not json").unwrap();
        assert!(matches!(
            TrustStore::open(dir.path()),
            Err(Error::CorruptTrustStore(_))
        ));

        fs::write(
            dir.path().join(FILE_NAME),
            br#"{"version":1,"devices":[{"id":"nope","name":"x","platform":"macos","paired_at":1}]}"#,
        )
        .unwrap();
        assert!(matches!(
            TrustStore::open(dir.path()),
            Err(Error::CorruptTrustStore(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn file_is_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = TrustStore::open(dir.path()).unwrap();
        store.add(device("Phone")).unwrap();
        let mode = fs::metadata(dir.path().join(FILE_NAME))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn failed_save_does_not_grant_trust() {
        let dir = tempfile::tempdir().unwrap();
        let store = TrustStore::open(dir.path().join("gone")).unwrap();
        // Make the target folder impossible to create: a file sits in its place.
        fs::write(dir.path().join("gone"), b"").unwrap();
        let phone = device("Phone");
        assert!(store.add(phone.clone()).is_err());
        assert!(!store.is_trusted(&phone.id));
    }
}
