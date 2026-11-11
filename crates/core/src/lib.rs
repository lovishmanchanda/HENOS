//! henos-core: everything the desktop and Android apps share.
//!
//! Built so far (see `docs/PLAN.md` for the roadmap):
//!
//! - [`identity`]: the device's long-lived key and where it is stored
//! - [`device`]: the name and platform a device shows to others
//! - [`trust`]: the saved list of paired devices
//! - [`pairing`]: how two devices come to trust each other, by code or QR
//! - [`protocol`]: the messages two paired devices exchange
//! - [`node`]: discovery on the local network and encrypted connections
//! - [`clipboard`]: shared clipboard between paired devices
//!
//! Platform shells (Tauri, Android) stay thin: they supply the clipboard, the
//! keychain and the UI, and call into this crate for the rest.

pub mod clipboard;
pub mod device;
pub mod error;
mod fsutil;
pub mod identity;
pub mod node;
pub mod pairing;
pub mod protocol;
pub mod trust;

pub use clipboard::{
    ClipboardBackend, ClipboardContent, ClipboardEvent, ClipboardItem, ClipboardSync, LocalCopy,
};
pub use device::{DeviceInfo, Platform};
pub use error::{Error, Result};
pub use identity::{DeviceId, FileKeyStore, Identity, KeyStore};
pub use node::{NearbyDevice, Node, NodeConfig, NodeEvent, PairingOffer, Peer};
pub use pairing::PairingTicket;
pub use protocol::PROTOCOL_VERSION;
pub use trust::{PairedDevice, TrustStore};

/// Crate version, shown in the apps and the CLI.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
