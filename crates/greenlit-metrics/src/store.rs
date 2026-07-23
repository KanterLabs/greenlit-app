//! The local NDJSON metrics file: append-only writer, and reader for `litci
//! stats`.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustix::fs::{Mode, OFlags, open};

use crate::error::MetricsError;
use crate::record::{InvocationRecord, SCHEMA_VERSION};

mod decode;
mod path;
mod recent;

use decode::{DecodeError, decode_record};

/// One metrics record may contain the timings for a very large workflow, so
/// the boundary is intentionally generous while still preventing a corrupt
/// local tail from driving an unbounded allocation in `plan` or `stats`.
const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;

/// A handle to the local, append-only NDJSON metrics file.
///
/// Per `AGENTS.md` ("Metrics") this is strictly local: nothing in this type,
/// or anywhere in this crate, performs network I/O. Every `plan`/`run`
/// invocation appends exactly one record via [`append`](Self::append);
/// `litci stats` reads bounded history via [`read_recent`](Self::read_recent) and must
/// never call `append` for its own invocation.
#[derive(Debug, Clone)]
pub struct MetricsStore {
    file_path: PathBuf,
    // `open_default` resolves and opens HOME once. Holding the directory fd
    // both allows HOME itself to be a symlink and prevents later path
    // replacement from redirecting `.litci/metrics` traversal.
    default_home: Option<Arc<File>>,
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
            default_home: None,
        }
    }

    /// Opens the real per-user metrics store, resolving the home directory
    /// from the `HOME` environment variable.
    ///
    /// Greenlit v0 targets Linux x86_64 hosts only (`greenlit-v0-spec.md`
    /// "Tech"), where `HOME` is the standard, always-present convention, so
    /// no cross-platform home-directory crate is pulled in for this. `HOME`
    /// itself is resolved once and held open, while `.litci`, `metrics`, and
    /// `runs.ndjson` are traversed without following symlinks.
    pub fn open_default() -> Result<Self, MetricsError> {
        let home = std::env::var_os("HOME").ok_or(MetricsError::HomeDirUnavailable)?;
        let home = Path::new(&home);
        if !home.is_absolute() {
            return Err(MetricsError::InvalidHomeDir);
        }
        // Resolve HOME itself so common configurations where it is a symlink
        // remain supported. Every component below this already-opened
        // directory is subsequently traversed with `O_NOFOLLOW`.
        let resolved_home =
            std::fs::canonicalize(home).map_err(|_| MetricsError::InvalidHomeDir)?;
        let fd = open(
            &resolved_home,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|_| MetricsError::InvalidHomeDir)?;
        Ok(Self {
            file_path: Self::default_path_under(&resolved_home),
            default_home: Some(Arc::new(File::from(fd))),
        })
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
    /// record is preserved and separated with its missing newline; an
    /// incomplete fragment is truncated before this record is appended. Tail
    /// repair searches only one bounded record window and refuses to truncate
    /// when it cannot prove the preceding newline boundary. An exclusive
    /// cross-process file lock covers inspection, repair, and the append
    /// itself, so another Greenlit process cannot be truncated out by a stale
    /// repair decision.
    ///
    /// Every `litci plan`/`litci run` invocation calls this exactly once,
    /// with the record from [`crate::Invocation::finish`]. `litci stats`
    /// (read-only) must never call this for its own invocation
    /// (`PHASE-1-engine-core.md`).
    pub fn append(&self, record: &InvocationRecord) -> Result<(), MetricsError> {
        let mut line = serde_json::to_string(record).map_err(|source| MetricsError::Serialize {
            path: self.file_path.clone(),
            source,
        })?;
        if line.len() > MAX_RECORD_BYTES {
            return Err(MetricsError::RecordWriteLimit {
                path: self.file_path.clone(),
                max_bytes: MAX_RECORD_BYTES,
            });
        }
        line.push('\n');

        let mut file = self.open_for_append()?;
        file.lock().map_err(|source| MetricsError::LockFile {
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
    /// ignored because it can be the residue of an interrupted append when
    /// it is within the per-record safety bound; every newline-terminated
    /// malformed or oversized line remains a corruption error.
    pub fn read_all(&self) -> Result<Vec<InvocationRecord>, MetricsError> {
        self.read_records()
    }

    /// Reads at most the newest `limit` records, preserving chronological
    /// order while keeping both work and memory bounded for long-lived
    /// installations.
    ///
    /// This reader walks backward from the file tail and stops as soon as it
    /// has the requested records. It therefore does not inspect corruption
    /// outside the retained window. A fixed 16 MiB aggregate serialized-byte
    /// budget means it can return fewer than `limit` unusually large records.
    pub fn read_recent(&self, limit: usize) -> Result<Vec<InvocationRecord>, MetricsError> {
        self.read_recent_records(limit)
    }

    fn read_records(&self) -> Result<Vec<InvocationRecord>, MetricsError> {
        let file = match self.open_for_read()? {
            Some(file) => file,
            None => return Ok(Vec::new()),
        };
        file.lock_shared()
            .map_err(|source| MetricsError::LockFile {
                path: self.file_path.clone(),
                source,
            })?;

        let mut reader = BufReader::new(file);
        let mut records = Vec::new();
        let mut line = Vec::new();
        let mut line_number = 0usize;
        loop {
            line.clear();
            let bytes_read = read_bounded_line(&mut reader, &mut line, &self.file_path)?;
            if bytes_read == 0 {
                break;
            }
            line_number += 1;
            let terminated = line.ends_with(b"\n");
            let trimmed = line.trim_ascii();
            if trimmed.is_empty() {
                continue;
            }
            match decode_record(trimmed) {
                Ok(record) => {
                    records.push(record);
                }
                // Only an unterminated final fragment can be a torn append.
                // A malformed line ending in `\n` was fully committed and
                // is corruption even when it is the last line.
                Err(DecodeError::Corrupt(_)) if !terminated => break,
                Err(DecodeError::Corrupt(source)) => {
                    return Err(MetricsError::CorruptRecord {
                        path: self.file_path.clone(),
                        line: line_number,
                        source,
                    });
                }
                Err(DecodeError::Unsupported(found)) => {
                    return Err(MetricsError::UnsupportedSchema {
                        path: self.file_path.clone(),
                        line: line_number,
                        found,
                        supported: SCHEMA_VERSION,
                    });
                }
            }
        }
        Ok(records)
    }
}

fn read_bounded_line(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    path: &Path,
) -> Result<usize, MetricsError> {
    let limit = MAX_RECORD_BYTES.saturating_add(1);
    let bytes_read = reader
        .take(limit as u64)
        .read_until(b'\n', line)
        .map_err(|source| MetricsError::ReadFile {
            path: path.to_path_buf(),
            source,
        })?;
    let content_len = line.strip_suffix(b"\n").map_or(line.len(), <[u8]>::len);
    if content_len > MAX_RECORD_BYTES {
        return Err(MetricsError::RecordReadLimit {
            path: path.to_path_buf(),
            max_bytes: MAX_RECORD_BYTES,
        });
    }
    Ok(bytes_read)
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

    let tail_start =
        find_tail_start(file, file_len, path)?.ok_or_else(|| MetricsError::RecordReadLimit {
            path: path.to_path_buf(),
            max_bytes: MAX_RECORD_BYTES,
        })?;
    if tail_start == file_len {
        return Ok(false);
    }

    let tail_len = file_len - tail_start;
    if tail_len > MAX_RECORD_BYTES as u64 {
        // A record is committed only by its terminating newline. An
        // oversized unterminated suffix is therefore a torn append, not a
        // stored record to retain. Truncate it at the last committed
        // newline without ever materializing the suffix in memory.
        file.set_len(tail_start)
            .map_err(|source| MetricsError::WriteRecord {
                path: path.to_path_buf(),
                source,
            })?;
        return Ok(false);
    }

    file.seek(SeekFrom::Start(tail_start))
        .map_err(|source| MetricsError::ReadFile {
            path: path.to_path_buf(),
            source,
        })?;
    let mut tail = Vec::new();
    (&mut *file)
        .take(tail_len)
        .read_to_end(&mut tail)
        .map_err(|source| MetricsError::ReadFile {
            path: path.to_path_buf(),
            source,
        })?;

    // Tail repair decides only whether one complete JSON value is present;
    // schema compatibility belongs to the readers. `IgnoredAny` validates
    // the bounded bytes without materializing a second generic JSON tree, so
    // a complete future or legacy record is preserved for an actionable
    // compatibility error from `stats`.
    if serde_json::from_slice::<serde::de::IgnoredAny>(&tail).is_ok() {
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
) -> Result<Option<u64>, MetricsError> {
    const SCAN_CHUNK_BYTES: usize = 8 * 1024;
    // One byte admits a record just over the write limit without allocating
    // it; the other admits the preceding newline that proves a safe truncate
    // boundary for exactly that oversized suffix.
    const TAIL_SCAN_SLACK_BYTES: usize = 2;

    let mut cursor = file_len;
    let scan_bound = (MAX_RECORD_BYTES.saturating_add(TAIL_SCAN_SLACK_BYTES)) as u64;
    let scan_floor = file_len.saturating_sub(scan_bound);
    let mut buffer = [0_u8; SCAN_CHUNK_BYTES];
    while cursor > scan_floor {
        let bytes_to_read = (cursor - scan_floor).min(SCAN_CHUNK_BYTES as u64) as usize;
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
            return Ok(Some(cursor + index as u64 + 1));
        }
    }
    if scan_floor == 0 {
        Ok(Some(0))
    } else {
        Ok(None)
    }
}
