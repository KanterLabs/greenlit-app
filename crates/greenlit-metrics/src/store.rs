//! The local NDJSON metrics file: append-only writer, and reader for `litci
//! stats`.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::MetricsError;
use crate::record::{InvocationRecord, SCHEMA_VERSION};

/// A handle to the local, append-only NDJSON metrics file.
///
/// Per `AGENTS.md` ("Metrics") this is strictly local: nothing in this type,
/// or anywhere in this crate, performs network I/O. Every `plan`/`run`
/// invocation appends exactly one record via [`append`](Self::append);
/// `litci stats` reads history via [`read_all`](Self::read_all) and must
/// never call `append` for its own invocation.
#[derive(Debug, Clone)]
pub struct MetricsStore {
    file_path: PathBuf,
}

impl MetricsStore {
    /// Opens a store backed by an explicit NDJSON file path.
    ///
    /// Use this in tests, and for any future `--metrics-file` style override
    /// — [`open_default`](Self::open_default) is the only place this crate
    /// touches the real user home directory.
    pub fn at(file_path: impl Into<PathBuf>) -> Self {
        Self {
            file_path: file_path.into(),
        }
    }

    /// Opens the real per-user metrics store, resolving the home directory
    /// from the `HOME` environment variable.
    ///
    /// Greenlit v0 targets Linux x86_64 hosts only (`greenlit-v0-spec.md`
    /// "Tech"), where `HOME` is the standard, always-present convention, so
    /// no cross-platform home-directory crate is pulled in for this.
    pub fn open_default() -> Result<Self, MetricsError> {
        let home = std::env::var_os("HOME").ok_or(MetricsError::HomeDirUnavailable)?;
        Ok(Self::at(Self::default_path_under(Path::new(&home))))
    }

    /// The default metrics file path under a given home directory:
    /// `<home>/.litci/metrics/runs.ndjson` (`AGENTS.md`: "User-local state |
    /// `~/.litci/`"; `PHASE-1-engine-core.md` greenlit-metrics section).
    ///
    /// Pure and side-effect free (no filesystem or environment access) so it
    /// is deterministically testable and reusable by anything that already
    /// knows the home directory without going through
    /// [`open_default`](Self::open_default).
    pub fn default_path_under(home_dir: &Path) -> PathBuf {
        home_dir.join(".litci").join("metrics").join("runs.ndjson")
    }

    /// The NDJSON file path this store reads from and appends to.
    pub fn path(&self) -> &Path {
        &self.file_path
    }

    /// Appends `record` to the store as one NDJSON line, creating the parent
    /// directory and file on first use.
    ///
    /// If an interrupted earlier append left an unterminated tail, a complete
    /// JSON value is preserved and separated with its missing newline; an
    /// incomplete fragment is truncated before this record is appended.
    ///
    /// Every `litci plan`/`litci run` invocation calls this exactly once,
    /// with the record from [`crate::Invocation::finish`]. `litci stats`
    /// (read-only) must never call this for its own invocation
    /// (`PHASE-1-engine-core.md`).
    pub fn append(&self, record: &InvocationRecord) -> Result<(), MetricsError> {
        if let Some(parent) = self.file_path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| MetricsError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let mut line = serde_json::to_string(record).map_err(|source| MetricsError::Serialize {
            path: self.file_path.clone(),
            source,
        })?;
        line.push('\n');

        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&self.file_path)
            .map_err(|source| MetricsError::OpenForWrite {
                path: self.file_path.clone(),
                source,
            })?;

        let needs_separator = repair_unterminated_tail(&mut file, &self.file_path)?;
        if needs_separator {
            line.insert(0, '\n');
        }

        file.write_all(line.as_bytes())
            .map_err(|source| MetricsError::WriteRecord {
                path: self.file_path.clone(),
                source,
            })
    }

    /// Reads and parses every record currently in the store, in file (i.e.
    /// chronological append) order.
    ///
    /// A metrics file that does not exist yet — the state of a fresh
    /// install before the first `plan`/`run` — is treated as empty history
    /// rather than an error, so `litci stats` can render "no history yet"
    /// instead of failing. An unterminated malformed final fragment is
    /// ignored because it can be the residue of an interrupted append;
    /// every newline-terminated malformed line remains a corruption error.
    pub fn read_all(&self) -> Result<Vec<InvocationRecord>, MetricsError> {
        self.read_records(None)
    }

    /// Reads at most the newest `limit` records, preserving chronological
    /// order while keeping memory bounded for long-lived installations.
    pub fn read_recent(&self, limit: usize) -> Result<Vec<InvocationRecord>, MetricsError> {
        self.read_records(Some(limit))
    }

    fn read_records(&self, limit: Option<usize>) -> Result<Vec<InvocationRecord>, MetricsError> {
        let file = match std::fs::File::open(&self.file_path) {
            Ok(file) => file,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(MetricsError::ReadFile {
                    path: self.file_path.clone(),
                    source,
                });
            }
        };

        let mut reader = BufReader::new(file);
        let mut records = VecDeque::new();
        let mut line = String::new();
        let mut line_number = 0usize;
        loop {
            line.clear();
            let bytes_read =
                reader
                    .read_line(&mut line)
                    .map_err(|source| MetricsError::ReadFile {
                        path: self.file_path.clone(),
                        source,
                    })?;
            if bytes_read == 0 {
                break;
            }
            line_number += 1;
            let terminated = line.ends_with('\n');
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str(trimmed) {
                Ok(record) => {
                    let record: InvocationRecord = record;
                    if record.schema_version != SCHEMA_VERSION {
                        return Err(MetricsError::UnsupportedSchema {
                            path: self.file_path.clone(),
                            line: line_number,
                            found: record.schema_version,
                            supported: SCHEMA_VERSION,
                        });
                    }
                    records.push_back(record);
                    if let Some(limit) = limit {
                        while records.len() > limit {
                            records.pop_front();
                        }
                    }
                }
                // Only an unterminated final fragment can be a torn append.
                // A malformed line ending in `\n` was fully committed and
                // is corruption even when it is the last line.
                Err(_) if !terminated => break,
                Err(source) => {
                    return Err(MetricsError::CorruptRecord {
                        path: self.file_path.clone(),
                        line: line_number,
                        source,
                    });
                }
            }
        }
        Ok(records.into_iter().collect())
    }
}

/// Makes an interrupted final append safe before another record is written.
///
/// A complete JSON value that lost only its terminating newline is retained;
/// the caller prefixes the new record with a newline. A malformed fragment is
/// truncated back to the last committed newline. Without this repair, the
/// next append would concatenate its record onto the fragment and turn the
/// recoverable tail into a permanently corrupt, newline-terminated line.
fn repair_unterminated_tail(file: &mut std::fs::File, path: &Path) -> Result<bool, MetricsError> {
    let file_len = file
        .metadata()
        .map_err(|source| MetricsError::ReadFile {
            path: path.to_path_buf(),
            source,
        })?
        .len();
    if file_len == 0 {
        return Ok(false);
    }

    let tail_start = find_tail_start(file, file_len, path)?;
    if tail_start == file_len {
        return Ok(false);
    }

    file.seek(SeekFrom::Start(tail_start))
        .map_err(|source| MetricsError::ReadFile {
            path: path.to_path_buf(),
            source,
        })?;
    let mut tail = Vec::new();
    (&mut *file)
        .take(file_len - tail_start)
        .read_to_end(&mut tail)
        .map_err(|source| MetricsError::ReadFile {
            path: path.to_path_buf(),
            source,
        })?;

    if serde_json::from_slice::<serde_json::Value>(&tail).is_ok() {
        return Ok(true);
    }

    file.set_len(tail_start)
        .map_err(|source| MetricsError::WriteRecord {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(false)
}

fn find_tail_start(
    file: &mut std::fs::File,
    file_len: u64,
    path: &Path,
) -> Result<u64, MetricsError> {
    const SCAN_CHUNK_BYTES: usize = 8 * 1024;

    let mut cursor = file_len;
    let mut buffer = [0_u8; SCAN_CHUNK_BYTES];
    while cursor > 0 {
        let bytes_to_read = cursor.min(SCAN_CHUNK_BYTES as u64) as usize;
        cursor -= bytes_to_read as u64;
        file.seek(SeekFrom::Start(cursor))
            .and_then(|_| file.read_exact(&mut buffer[..bytes_to_read]))
            .map_err(|source| MetricsError::ReadFile {
                path: path.to_path_buf(),
                source,
            })?;
        if let Some(index) = buffer[..bytes_to_read]
            .iter()
            .rposition(|byte| *byte == b'\n')
        {
            return Ok(cursor + index as u64 + 1);
        }
    }
    Ok(0)
}
