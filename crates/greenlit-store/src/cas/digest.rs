//! Validated SHA-256 object identities.

use std::fmt;

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

    pub(super) fn hex(&self) -> &str {
        &self.0["sha256:".len()..]
    }
}

impl fmt::Display for ObjectDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A malformed object identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("content identity must be lowercase sha256:<64 hex>")]
pub struct InvalidDigest;
