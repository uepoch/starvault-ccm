//! Validated identities used at every filesystem and IPC boundary.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{user_err, Result};

/// Filesystem-safe package identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackageId(String);

impl PackageId {
    pub const MAX_LEN: usize = 64;

    pub fn parse(value: impl AsRef<str>) -> Result<Self> {
        let value = value.as_ref();
        if value.is_empty() || value.len() > Self::MAX_LEN {
            return Err(invalid_id("package id must contain 1 to 64 characters"));
        }
        if !value.is_ascii()
            || value
                .bytes()
                .any(|byte| !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'))
        {
            return Err(invalid_id(
                "package id may contain only lowercase ASCII letters, digits, and single dashes",
            ));
        }
        if value.starts_with('-') || value.ends_with('-') || value.contains("--") {
            return Err(invalid_id(
                "package id must use non-empty segments separated by single dashes",
            ));
        }
        if value == "plain" {
            return Err(invalid_id("`plain` is reserved"));
        }
        if value.split('-').any(is_windows_device_name) {
            return Err(invalid_id(
                "package id contains a reserved Windows device name",
            ));
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

fn invalid_id(message: impl Into<String>) -> crate::Error {
    user_err("invalid_package_id", message)
}

fn is_windows_device_name(segment: &str) -> bool {
    matches!(segment, "con" | "prn" | "aux" | "nul")
        || segment.strip_prefix("com").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
        || segment.strip_prefix("lpt").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
}

impl AsRef<str> for PackageId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for PackageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for PackageId {
    type Err = crate::Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

impl Serialize for PackageId {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PackageId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

/// Opaque, stable save-profile identity. Discovery hashes the local account
/// and profile names; callers must still resolve the token against a fresh
/// discovery pass before using it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProfileId(String);

impl ProfileId {
    pub(crate) fn discovered(seed: impl AsRef<[u8]>) -> Self {
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();
        hasher.update(b"starvault-profile-id\0");
        hasher.update(seed.as_ref());
        Self(hex::encode(hasher.finalize()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn parse(value: impl AsRef<str>) -> Result<Self> {
        let value = value.as_ref();
        if value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Ok(Self(value.to_string()))
        } else {
            Err(user_err(
                "invalid_profile_id",
                "profile id must be a 64-character lowercase hexadecimal token",
            ))
        }
    }
}

impl FromStr for ProfileId {
    type Err = crate::Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

impl fmt::Display for ProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for ProfileId {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ProfileId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_canonical_ids() {
        for id in ["a", "campaign-2", "raynor-rogue", "a1-b2"] {
            assert_eq!(PackageId::parse(id).unwrap().as_str(), id);
        }
        assert!(PackageId::parse("a".repeat(64)).is_ok());
    }

    #[test]
    fn rejects_unsafe_or_ambiguous_ids() {
        for id in [
            "",
            "A",
            "a_b",
            "a/b",
            "a\\b",
            ".",
            "..",
            "-a",
            "a-",
            "a--b",
            "plain",
            "con",
            "thing-lpt9",
            "com1-tool",
            "trail.",
            "trail ",
        ] {
            assert!(PackageId::parse(id).is_err(), "accepted {id:?}");
        }
        assert!(PackageId::parse("a".repeat(65)).is_err());
    }

    #[test]
    fn serde_cannot_bypass_validation() {
        assert!(serde_json::from_str::<PackageId>("\"../escape\"").is_err());
    }

    #[test]
    fn profile_ids_are_opaque_stable_tokens() {
        let first = ProfileId::discovered(b"account\0profile");
        let again = ProfileId::discovered(b"account\0profile");
        let other = ProfileId::discovered(b"account\0other");

        assert_eq!(first, again);
        assert_ne!(first, other);
        assert_eq!(first.as_str().len(), 64);
        assert!(!first.as_str().contains("account"));
        assert_eq!(
            serde_json::from_str::<ProfileId>(&serde_json::to_string(&first).unwrap()).unwrap(),
            first
        );
        assert!(serde_json::from_str::<ProfileId>("\"account/profile\"").is_err());
        assert!(serde_json::from_str::<ProfileId>(
            "\"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\""
        )
        .is_err());
    }
}
