//! The local NDJSON metrics file: append-only writer, and reader for `litci
//! stats`.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use crate::error::MetricsError;
use crate::record::InvocationRecord;

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
            .append(true)
            .open(&self.file_path)
            .map_err(|source| MetricsError::OpenForWrite {
                path: self.file_path.clone(),
                source,
            })?;

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
    /// instead of failing. A malformed final line is also ignored because it
    /// can be the partial residue of an interrupted append; malformed lines
    /// before the final line remain corruption errors.
    pub fn read_all(&self) -> Result<Vec<InvocationRecord>, MetricsError> {
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

        let reader = BufReader::new(file);
        let mut lines = reader.lines().enumerate().peekable();
        let mut records = Vec::new();
        while let Some((index, line)) = lines.next() {
            let line = line.map_err(|source| MetricsError::ReadFile {
                path: self.file_path.clone(),
                source,
            })?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str(trimmed) {
                Ok(record) => records.push(record),
                Err(_) if lines.peek().is_none() => break,
                Err(source) => {
                    return Err(MetricsError::CorruptRecord {
                        path: self.file_path.clone(),
                        line: index + 1,
                        source,
                    });
                }
            }
        }
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SCHEMA_VERSION, StageDuration};

    fn sample_record(command: &str) -> InvocationRecord {
        InvocationRecord {
            schema_version: SCHEMA_VERSION,
            command: command.to_string(),
            started_at_unix_ms: 1_700_000_000_000,
            total_duration_ms: 2.5,
            stages: vec![StageDuration {
                name: "parse".to_string(),
                duration_ms: 1.0,
            }],
        }
    }

    #[test]
    fn append_then_read_all_round_trips_in_order() {
        let dir = tempfile::tempdir().expect("tempdir must be creatable");
        let store = MetricsStore::at(dir.path().join("runs.ndjson"));

        let first = sample_record("plan");
        let second = sample_record("run");
        store.append(&first).expect("first append must succeed");
        store.append(&second).expect("second append must succeed");

        let records = store.read_all().expect("read_all must succeed");
        assert_eq!(records, vec![first, second]);
    }

    #[test]
    fn read_all_on_missing_file_is_empty_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir must be creatable");
        let store = MetricsStore::at(dir.path().join("does-not-exist.ndjson"));

        let records = store.read_all().expect("missing file must read as empty");
        assert!(records.is_empty());
    }

    #[test]
    fn read_all_reports_the_line_number_of_a_corrupt_record() {
        let dir = tempfile::tempdir().expect("tempdir must be creatable");
        let path = dir.path().join("runs.ndjson");
        let store = MetricsStore::at(&path);

        let good = sample_record("plan");
        store.append(&good).expect("append must succeed");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("file must be reopenable")
            .write_all(b"not json\n")
            .expect("raw write must succeed");
        store
            .append(&sample_record("run"))
            .expect("later append must succeed");

        let err = store.read_all().expect_err("corrupt line must error");
        match err {
            MetricsError::CorruptRecord { line, .. } => assert_eq!(line, 2),
            other => panic!("expected CorruptRecord, got {other:?}"),
        }
    }

    #[test]
    fn read_all_recovers_records_before_a_malformed_trailing_line() {
        let dir = tempfile::tempdir().expect("tempdir must be creatable");
        let path = dir.path().join("runs.ndjson");
        let store = MetricsStore::at(&path);

        let good = sample_record("plan");
        store.append(&good).expect("append must succeed");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("file must be reopenable")
            .write_all(b"{\"schema_version\":1")
            .expect("partial trailing write must succeed");

        let records = store
            .read_all()
            .expect("a malformed trailing record must be ignored");
        assert_eq!(records, vec![good]);
    }

    #[test]
    fn append_creates_missing_parent_directories() {
        let dir = tempfile::tempdir().expect("tempdir must be creatable");
        let nested = dir
            .path()
            .join("nested")
            .join("metrics")
            .join("runs.ndjson");
        let store = MetricsStore::at(&nested);

        store
            .append(&sample_record("plan"))
            .expect("append must create parent dirs");
        assert!(nested.exists());
    }
}
