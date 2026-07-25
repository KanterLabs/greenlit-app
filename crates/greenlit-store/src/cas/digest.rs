//! Validated SHA-256 object identities.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

/// A lowercase `sha256:<64 hex>` immutable object identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectDigest(pub(super) String);

impl ObjectDigest {
    /// Parses and validates an object identity.
    pub fn parse(value: &str) -> Result<Self, InvalidDigest> {
        let Some(hex) = value.strip_prefix("sha256:") else {
            return Err(InvalidDigest);
        };
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(InvalidDigest);
        }
        Ok(Self(value.to_string()))
    }

    /// Returns the complete prefixed identity.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Computes the immutable identity of `bytes`.
    #[must_use]
    pub fn of_bytes(bytes: &[u8]) -> Self {
        let hash = Sha256::digest(bytes);
        let mut hex = String::with_capacity(64);
        for byte in hash {
            hex.push_str(&format!("{byte:02x}"));
        }
        Self(format!("sha256:{hex}"))
    }

    pub(super) fn hex(&self) -> &str {
        &self.0["sha256:".len()..]
    }
}

impl fmt::Display for ObjectDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for ObjectDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ObjectDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

/// A malformed object identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("content identity must be lowercase sha256:<64 hex>")]
pub struct InvalidDigest;
