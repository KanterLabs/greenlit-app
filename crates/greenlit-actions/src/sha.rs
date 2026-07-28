//! A validated, canonical commit SHA.
//!
//! GitHub's own guidance for action pinning is explicit that this must be a
//! *full* SHA, not an abbreviated one: "you must use a commit's full SHA
//! value, and not an abbreviated value... SHAs are immutable and therefore
//! more reliable than tags or branches"
//! (<https://docs.github.com/en/actions/how-tos/write-workflows/choose-what-workflows-do/find-and-customize-actions>).
//! [`CommitSha`] enforces exactly that shape everywhere a resolved ref
//! flows: 40 hexadecimal characters, normalized to lowercase so the same
//! commit never produces two different content-addressed store paths on a
//! case-preserving-but-insensitive filesystem or a careless uppercase paste.

use std::fmt;

/// The fixed length of a full (non-abbreviated) Git commit SHA-1 hex digest.
pub const COMMIT_SHA_LENGTH: usize = 40;

/// A commit SHA that has been validated as exactly 40 hexadecimal
/// characters and normalized to lowercase.
///
/// This is the only shape [`crate::store::ActionStore`] accepts for a
/// content-addressed directory name, and the only shape
/// [`crate::resolve::RefResolver`] implementations return — both a defense
/// against a short/ambiguous ref ever reaching the store, and (via
/// lowercasing) a guarantee that the same commit always maps to the same
/// store path regardless of how a ref string or API response happened to
/// capitalize it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommitSha(String);

impl CommitSha {
    /// Validates `raw` as a full 40-character hexadecimal SHA, returning the
    /// lowercase-normalized value.
    ///
    /// # Errors
    /// Returns [`InvalidCommitSha`] when `raw` is not exactly
    /// [`COMMIT_SHA_LENGTH`] ASCII hex characters.
    pub fn parse(raw: &str) -> Result<Self, InvalidCommitSha> {
        if raw.len() != COMMIT_SHA_LENGTH || !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(InvalidCommitSha {
                value: raw.to_owned(),
            });
        }
        Ok(Self(raw.to_ascii_lowercase()))
    }

    /// Whether `raw` is already shaped like a full commit SHA (40 hex
    /// characters), without allocating a [`CommitSha`].
    ///
    /// Used by [`crate::resolve::resolve_ref`] to decide whether a `uses:`
    /// ref can resolve to itself without ever consulting a
    /// [`crate::resolve::RefResolver`] — `PHASE-3-actions.md`: "A ref that
    /// is already a full 40-hex SHA resolves to itself without network."
    #[must_use]
    pub fn looks_like_sha(raw: &str) -> bool {
        raw.len() == COMMIT_SHA_LENGTH && raw.bytes().all(|b| b.is_ascii_hexdigit())
    }

    /// The lowercase 40-character hex string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CommitSha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for CommitSha {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// `raw` is not a valid full commit SHA.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("'{value}' is not a full 40-character commit SHA")]
pub struct InvalidCommitSha {
    /// The rejected input.
    pub value: String,
}
