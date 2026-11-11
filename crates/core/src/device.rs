//! What a device tells others about itself: a display name and its platform.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Longest device name we send or accept, in bytes.
pub const MAX_NAME_BYTES: usize = 64;

/// The operating system a device runs.
///
/// Sent on the wire as a short string so that a newer app can introduce a
/// platform without breaking older ones, which simply see `Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "String", from = "String")]
pub enum Platform {
    MacOs,
    Windows,
    Linux,
    Android,
    Ios,
    Unknown,
}

impl Platform {
    /// The platform this code was compiled for.
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "android") {
            Self::Android
        } else if cfg!(target_os = "ios") {
            Self::Ios
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Unknown
        }
    }

    /// The wire and display form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::MacOs => "macos",
            Self::Windows => "windows",
            Self::Linux => "linux",
            Self::Android => "android",
            Self::Ios => "ios",
            Self::Unknown => "unknown",
        }
    }

    /// Parses the wire form. Anything unrecognised becomes `Unknown`.
    pub fn parse(s: &str) -> Self {
        match s {
            "macos" => Self::MacOs,
            "windows" => Self::Windows,
            "linux" => Self::Linux,
            "android" => Self::Android,
            "ios" => Self::Ios,
            _ => Self::Unknown,
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<Platform> for String {
    fn from(p: Platform) -> Self {
        p.as_str().to_owned()
    }
}

impl From<String> for Platform {
    fn from(s: String) -> Self {
        Self::parse(&s)
    }
}

/// A device's self-description. Shown in device lists; never used for trust.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub name: String,
    pub platform: Platform,
}

impl DeviceInfo {
    /// Describes this device, using `name` or the computer's hostname.
    pub fn for_this_device(name: Option<String>) -> Self {
        let name = name.unwrap_or_else(default_device_name);
        Self {
            name: sanitize_name(&name),
            platform: Platform::current(),
        }
    }

    /// Returns a copy that is safe to display: bounded length, no control
    /// characters. Applied to everything received from other devices.
    pub fn sanitized(&self) -> Self {
        Self {
            name: sanitize_name(&self.name),
            platform: self.platform,
        }
    }
}

/// The computer's hostname without a trailing `.local`.
pub fn default_device_name() -> String {
    let host = gethostname::gethostname().to_string_lossy().into_owned();
    let host = host.strip_suffix(".local").unwrap_or(&host);
    if host.trim().is_empty() {
        "Unnamed device".to_owned()
    } else {
        host.to_owned()
    }
}

/// Strips control characters and trims to [`MAX_NAME_BYTES`] on a character
/// boundary. Names come from other devices, so they are treated as untrusted.
pub fn sanitize_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars().filter(|c| !c.is_control()) {
        if out.len() + c.len_utf8() > MAX_NAME_BYTES {
            break;
        }
        out.push(c);
    }
    if out.is_empty() {
        "Unnamed device".to_owned()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_round_trips_and_tolerates_new_values() {
        for p in [
            Platform::MacOs,
            Platform::Windows,
            Platform::Linux,
            Platform::Android,
            Platform::Ios,
        ] {
            assert_eq!(Platform::parse(p.as_str()), p);
        }
        assert_eq!(Platform::parse("visionos"), Platform::Unknown);
    }

    #[test]
    fn names_are_cleaned_and_bounded() {
        assert_eq!(sanitize_name("  My\u{0007} Mac\n"), "My Mac");
        assert_eq!(sanitize_name("\n\t"), "Unnamed device");
        let long = "é".repeat(100);
        let cleaned = sanitize_name(&long);
        assert!(cleaned.len() <= MAX_NAME_BYTES);
        assert!(cleaned.chars().all(|c| c == 'é'));
    }
}
