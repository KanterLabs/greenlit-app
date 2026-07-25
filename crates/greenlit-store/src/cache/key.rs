//! Cache key matching: the exact rule `actions/cache` observes when it asks
//! the service to restore an entry.
//!
//! This module is pure — it decides *which* of a set of already-known entries
//! a lookup selects, with no filesystem or network access, so the rule can be
//! pinned by table-driven oracle tests (`TESTING.md` class 1) instead of being
//! implied by store plumbing.
//!
//! # The rule
//!
//! `@actions/cache`'s `getCacheEntry` sends one request carrying the ordered
//! key list `[key, ...restoreKeys]` plus a `version`
//! (`packages/cache/src/internal/cacheHttpClient.ts`, `getCacheEntry`:
//! `` `cache?keys=${encodeURIComponent(keys.join(','))}&version=${version}` ``).
//! GitHub's documented matching behavior for that list is:
//!
//! 1. **`version` scopes everything.** The version is a hash of the resolved
//!    `path` list and compression method, so entries saved for a different
//!    path set are never candidates — an entry whose version differs is
//!    invisible to this lookup, no matter how well its key matches.
//! 2. **The first key is an exact match.** "The `key` is searched for an
//!    exact match" — a cache saved under `k` is returned for `k` and for
//!    nothing else at this position.
//! 3. **Every later key is a prefix match.** "If there are no exact matches
//!    for `key`, the action searches for partial matches of the restore
//!    keys" — `restore-keys: [npm-]` matches `npm-abc123`.
//! 4. **Keys are tried in order, and the first key that matches anything
//!    wins.** A later restore key is never consulted once an earlier one has
//!    produced a candidate, even if the later one would match a newer entry.
//! 5. **Within one key's candidates, the most recently created entry wins.**
//!
//! The returned [`Match`] carries the key of the entry that actually matched,
//! which is what lets `actions/cache` decide between `cache-hit: true` (the
//! matched key equals the primary key) and a restore-key partial hit.
//!
//! Deliberate deviation from hosted GitHub, recorded in `ARCHITECTURE.md`'s
//! known-issues log: GitHub additionally scopes caches by git ref, so a run
//! sees its own branch, its base branch, and the default branch. Greenlit's
//! store is one machine-local scope shared by every branch of every checkout
//! of a repository; a local run therefore restores entries a hosted run on an
//! unrelated branch would not see. Enforcing ref scoping locally would make
//! `litci run` miss caches the developer just created on the branch they are
//! working on, which is the opposite of useful.

/// One entry a lookup may select, reduced to just the fields matching needs.
///
/// `created_unix` orders candidates within a single key's match set; it is
/// whole seconds since the Unix epoch, and ties are broken by `key` so the
/// selection is total and deterministic rather than dependent on directory
/// enumeration order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The key the entry was saved under.
    pub key: String,
    /// The opaque version the entry was saved under.
    pub version: String,
    /// Creation time, whole seconds since the Unix epoch.
    pub created_unix: u64,
}

/// The outcome of a successful lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// The key of the entry that matched — not necessarily the key that was
    /// asked for, since restore keys match by prefix.
    pub key: String,
    /// Whether this was an exact match on the lookup's primary key.
    pub exact: bool,
}

/// Selects the entry `keys` restores from `candidates`, or `None` on a miss.
///
/// `keys` is the ordered list `[key, ...restore_keys]` exactly as
/// `actions/cache` sends it. An empty `keys` never matches.
///
/// See the module documentation for the rule this implements.
#[must_use]
pub fn select<'a>(
    keys: &[String],
    version: &str,
    candidates: impl IntoIterator<Item = &'a Candidate>,
) -> Option<Match> {
    // Version scoping (rule 1) applies to every position, so filter once
    // rather than re-checking inside each key's pass.
    let scoped: Vec<&Candidate> = candidates
        .into_iter()
        .filter(|entry| entry.version == version)
        .collect();

    for (position, key) in keys.iter().enumerate() {
        // Rule 2 and 3: the first key is exact, every later key is a prefix.
        let exact = position == 0;
        let best = scoped
            .iter()
            .filter(|entry| {
                if exact {
                    entry.key == *key
                } else {
                    entry.key.starts_with(key.as_str())
                }
            })
            // Rule 5: newest first, `key` breaking ties for a total order.
            .max_by(|left, right| {
                left.created_unix
                    .cmp(&right.created_unix)
                    .then_with(|| left.key.cmp(&right.key))
            });

        // Rule 4: the first key that matches anything ends the search.
        if let Some(entry) = best {
            return Some(Match {
                key: entry.key.clone(),
                exact,
            });
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::{Candidate, Match, select};

    /// Builds a candidate; `version` defaults to `"v1"` so the many
    /// same-version cases stay readable.
    fn entry(key: &str, created_unix: u64) -> Candidate {
        Candidate {
            key: key.to_string(),
            version: "v1".to_string(),
            created_unix,
        }
    }

    fn versioned(key: &str, version: &str, created_unix: u64) -> Candidate {
        Candidate {
            key: key.to_string(),
            version: version.to_string(),
            created_unix,
        }
    }

    fn keys(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    /// One row per documented rule plus its edge cases, per `TESTING.md`
    /// class 1. `name` identifies the rule a failure broke.
    #[test]
    fn matching_follows_the_documented_rule() {
        struct Row {
            name: &'static str,
            keys: Vec<String>,
            candidates: Vec<Candidate>,
            expected: Option<Match>,
        }

        let rows = vec![
            Row {
                name: "primary key matches exactly",
                keys: keys(&["npm-abc"]),
                candidates: vec![entry("npm-abc", 10)],
                expected: Some(Match {
                    key: "npm-abc".to_string(),
                    exact: true,
                }),
            },
            Row {
                name: "primary key does not match by prefix",
                keys: keys(&["npm-"]),
                candidates: vec![entry("npm-abc", 10)],
                expected: None,
            },
            Row {
                name: "restore key matches by prefix",
                keys: keys(&["npm-abc", "npm-"]),
                candidates: vec![entry("npm-xyz", 10)],
                expected: Some(Match {
                    key: "npm-xyz".to_string(),
                    exact: false,
                }),
            },
            Row {
                name: "exact primary wins over a newer restore-key match",
                keys: keys(&["npm-abc", "npm-"]),
                candidates: vec![entry("npm-abc", 1), entry("npm-zzz", 99)],
                expected: Some(Match {
                    key: "npm-abc".to_string(),
                    exact: true,
                }),
            },
            Row {
                name: "earlier restore key wins over a newer later-key match",
                keys: keys(&["miss", "npm-", "yarn-"]),
                candidates: vec![entry("npm-old", 1), entry("yarn-new", 99)],
                expected: Some(Match {
                    key: "npm-old".to_string(),
                    exact: false,
                }),
            },
            Row {
                name: "newest wins within one restore key",
                keys: keys(&["miss", "npm-"]),
                candidates: vec![entry("npm-old", 1), entry("npm-new", 2)],
                expected: Some(Match {
                    key: "npm-new".to_string(),
                    exact: false,
                }),
            },
            Row {
                name: "key breaks a creation-time tie deterministically",
                keys: keys(&["miss", "npm-"]),
                candidates: vec![entry("npm-a", 7), entry("npm-b", 7)],
                expected: Some(Match {
                    key: "npm-b".to_string(),
                    exact: false,
                }),
            },
            Row {
                name: "a different version is invisible even on an exact key",
                keys: keys(&["npm-abc"]),
                candidates: vec![versioned("npm-abc", "other", 10)],
                expected: None,
            },
            Row {
                name: "version scoping applies to restore keys too",
                keys: keys(&["miss", "npm-"]),
                candidates: vec![versioned("npm-x", "other", 10), entry("npm-y", 1)],
                expected: Some(Match {
                    key: "npm-y".to_string(),
                    exact: false,
                }),
            },
            Row {
                name: "an empty restore key prefix-matches everything",
                keys: keys(&["miss", ""]),
                candidates: vec![entry("anything", 3)],
                expected: Some(Match {
                    key: "anything".to_string(),
                    exact: false,
                }),
            },
            Row {
                name: "no keys never matches",
                keys: Vec::new(),
                candidates: vec![entry("npm-abc", 10)],
                expected: None,
            },
            Row {
                name: "no candidates is a miss",
                keys: keys(&["npm-abc", "npm-"]),
                candidates: Vec::new(),
                expected: None,
            },
            Row {
                name: "prefix match is not substring match",
                keys: keys(&["miss", "abc"]),
                candidates: vec![entry("xx-abc-yy", 10)],
                expected: None,
            },
        ];

        for row in rows {
            let actual = select(&row.keys, "v1", row.candidates.iter());
            assert_eq!(actual, row.expected, "rule: {}", row.name);
        }
    }
}
