//! Error type shared by the whole core.

use crate::identity::DeviceId;

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong in the core.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Reading or writing a file or a network stream failed.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// The stored identity key exists but cannot be used.
    #[error("identity key is corrupt: {0}")]
    CorruptIdentity(String),

    /// The network endpoint could not be started.
    #[error("could not start networking: {0}")]
    Bind(String),

    /// Dialing the other device failed.
    #[error("could not connect: {0}")]
    Connect(String),

    /// We refuse to talk to a device that has not been paired.
    #[error("device {} is not trusted", .0.fmt_short())]
    NotTrusted(DeviceId),

    /// The other device refused us because it has not paired with us.
    #[error("the other device does not trust this device")]
    Rejected,

    /// Both devices run versions that cannot talk to each other.
    #[error("incompatible protocol version (ours {ours}, theirs {theirs})")]
    IncompatibleVersion { ours: u16, theirs: u16 },

    /// The other device sent something that breaks the protocol.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// A message is larger than the limit for its stream.
    #[error("message of {size} bytes exceeds the limit of {limit} bytes")]
    FrameTooLarge { size: usize, limit: usize },

    /// Something took longer than allowed.
    #[error("timed out during {0}")]
    Timeout(&'static str),

    /// There is no live connection to that device.
    #[error("not connected to device {}", .0.fmt_short())]
    NotConnected(DeviceId),

    /// The connection ended while we were waiting for a reply.
    #[error("connection closed")]
    ConnectionClosed,

    /// A second connection to the same device lost the tie-break. Internal:
    /// callers of the public API get the surviving connection instead.
    #[error("duplicate connection")]
    Duplicate,

    /// The paired-device list exists but cannot be used.
    #[error("paired-device list is corrupt: {0}")]
    CorruptTrustStore(String),

    /// A pairing code must be six digits.
    #[error("a pairing code is 6 digits")]
    InvalidCode,

    /// The scanned or pasted pairing ticket is not usable.
    #[error("invalid pairing ticket: {0}")]
    InvalidTicket(String),

    /// The code did not match. The host has discarded it; a new one is needed.
    #[error("wrong pairing code")]
    WrongPairingCode,

    /// The other device is not showing a pairing code, or the code expired.
    #[error("the other device is not in pairing mode")]
    NotInPairingMode,

    /// Pairing failed for another reason.
    #[error("pairing failed: {0}")]
    Pairing(String),
}
