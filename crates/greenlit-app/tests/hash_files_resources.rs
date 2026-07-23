use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use greenlit_expr::{
    Context, DirEntry, EntryKind, EvalError, HashFilesError, HashFilesFs, OpenDirectory, OpenedDir,
    Value, evaluate, parse,
};

const WIDE_DIRECTORY_FILES: usize = 100_000;

/// See `hash_files_deadline.rs`'s copy of this adapter for why re-resolving
/// a child by its full lexical path is a faithful stand-in for these
/// recording fakes (unlike `RealFs`, which must never do that).
struct SimpleOpenDir<'a> {
    fs: &'a dyn HashFilesFs,
    entries: Box<dyn Iterator<Item = io::Result<DirEntry>> + 'a>,
}

impl<'a> OpenDirectory<'a> for SimpleOpenDir<'a> {
    fn next_entry(&mut self) -> io::Result<Option<DirEntry>> {
        self.entries.next().transpose()
    }

    fn open_child_dir(&self, _name: &str, path: &Path, _follow: bool) -> io::Result<OpenedDir<'a>> {
        self.fs.open_dir(path)
    }

    fn hash_child_file(
        &self,
        _name: &str,
        path: &Path,
        _follow: bool,
        check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]> {
        self.fs.hash_file_sha256(path, check_timeout)
    }
}

#[derive(Debug)]
struct StreamingWideDirectoryFs {
    root: PathBuf,
    yielded: Arc<AtomicUsize>,
    hashed: Arc<AtomicUsize>,
}

impl HashFilesFs for StreamingWideDirectoryFs {
    fn workspace_root(&self) -> &Path {
        &self.root
    }

    fn open_dir(&self, path: &Path) -> io::Result<OpenedDir<'_>> {
        if path != self.root {
            return Err(io::Error::other(
                "wide-directory files must be hashed before another directory is opened",
            ));
        }
        let yielded = Arc::clone(&self.yielded);
        let hashed = Arc::clone(&self.hashed);
        let mut next = 0;
        let entries: Box<dyn Iterator<Item = io::Result<DirEntry>>> =
            Box::new(std::iter::from_fn(move || {
                if next == WIDE_DIRECTORY_FILES {
                    return None;
                }
                if hashed.load(Ordering::Acquire) != next {
                    return Some(Err(io::Error::other(
                        "directory enumeration advanced before the preceding file was hashed",
                    )));
                }
                let ordinal = next;
                next += 1;
                yielded.fetch_add(1, Ordering::AcqRel);
                Some(Ok(DirEntry {
                    name: format!("file-{ordinal}.txt"),
                    kind: EntryKind::File,
                }))
            }));
        Ok(OpenedDir::new(
            path.to_path_buf(),
            Box::new(SimpleOpenDir { fs: self, entries }),
        ))
    }

    fn hash_file_sha256(
        &self,
        path: &Path,
        check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]> {
        check_timeout()?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| io::Error::other("wide-directory file name was not UTF-8"))?;
        let ordinal = file_name
            .strip_prefix("file-")
            .and_then(|name| name.strip_suffix(".txt"))
            .and_then(|value| value.parse::<usize>().ok())
            .ok_or_else(|| io::Error::other("unexpected wide-directory file name"))?;
        let expected = self.hashed.fetch_add(1, Ordering::AcqRel);
        if ordinal != expected {
            return Err(io::Error::other(format!(
                "wide-directory hash order changed: expected {expected}, received {ordinal}"
            )));
        }
        let mut digest = [0_u8; 32];
        digest[..size_of::<usize>()].copy_from_slice(&ordinal.to_le_bytes());
        check_timeout()?;
        Ok(digest)
    }

    fn entry_kind(&self, path: &Path) -> io::Result<EntryKind> {
        if path == self.root {
            Ok(EntryKind::Dir)
        } else {
            Ok(EntryKind::File)
        }
    }
}

#[derive(Debug)]
struct DepthFirstPullFs {
    root: PathBuf,
    child_hashed: Arc<AtomicBool>,
}

impl HashFilesFs for DepthFirstPullFs {
    fn workspace_root(&self) -> &Path {
        &self.root
    }

    fn open_dir(&self, path: &Path) -> io::Result<OpenedDir<'_>> {
        let entries: Box<dyn Iterator<Item = io::Result<DirEntry>>> = if path == self.root {
            let child_hashed = Arc::clone(&self.child_hashed);
            let mut next = 0_u8;
            Box::new(std::iter::from_fn(move || {
                let entry = match next {
                    0 => Ok(DirEntry {
                        name: "child".to_string(),
                        kind: EntryKind::Dir,
                    }),
                    1 if child_hashed.load(Ordering::Acquire) => Ok(DirEntry {
                        name: "later.txt".to_string(),
                        kind: EntryKind::File,
                    }),
                    1 => Err(io::Error::other(
                        "parent's later sibling was pulled before child descent completed",
                    )),
                    _ => return None,
                };
                next = next.saturating_add(1);
                Some(entry)
            }))
        } else if path == self.root.join("child") {
            Box::new(std::iter::once(Ok(DirEntry {
                name: "deep.txt".to_string(),
                kind: EntryKind::File,
            })))
        } else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "unexpected depth-first directory",
            ));
        };
        Ok(OpenedDir::new(
            path.to_path_buf(),
            Box::new(SimpleOpenDir { fs: self, entries }),
        ))
    }

    fn hash_file_sha256(
        &self,
        path: &Path,
        check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]> {
        check_timeout()?;
        if path == self.root.join("child/deep.txt") {
            self.child_hashed.store(true, Ordering::Release);
            return Ok([1_u8; 32]);
        }
        if path == self.root.join("later.txt") && self.child_hashed.load(Ordering::Acquire) {
            return Ok([2_u8; 32]);
        }
        Err(io::Error::other("hashFiles traversal order changed"))
    }

    fn entry_kind(&self, path: &Path) -> io::Result<EntryKind> {
        if path == self.root {
            Ok(EntryKind::Dir)
        } else {
            Ok(EntryKind::File)
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum RegistryBoundary {
    DirectoryCount,
    RetainedPathBytes,
}

#[derive(Debug)]
struct RegistryBoundaryFs {
    root: PathBuf,
    boundary: RegistryBoundary,
}

impl RegistryBoundaryFs {
    fn identity(&self, path: &Path) -> io::Result<PathBuf> {
        if path == self.root {
            return Ok(path.to_path_buf());
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| io::Error::other("alias name was not UTF-8"))?;
        match self.boundary {
            RegistryBoundary::DirectoryCount => {
                Ok(PathBuf::from(format!("/workspace/targets/{name}")))
            }
            RegistryBoundary::RetainedPathBytes => Ok(PathBuf::from(format!(
                "/workspace/targets/{}-{name}",
                "x".repeat(4096)
            ))),
        }
    }
}

impl HashFilesFs for RegistryBoundaryFs {
    fn workspace_root(&self) -> &Path {
        &self.root
    }

    fn open_dir(&self, path: &Path) -> io::Result<OpenedDir<'_>> {
        let identity = self.identity(path)?;
        let entries: Box<dyn Iterator<Item = io::Result<DirEntry>>> = if path != self.root {
            Box::new(std::iter::empty())
        } else {
            let mut ordinal = 0_usize;
            Box::new(std::iter::from_fn(move || {
                let current = ordinal;
                ordinal = ordinal.saturating_add(1);
                Some(Ok(DirEntry {
                    name: format!("alias-{current}"),
                    kind: EntryKind::Symlink,
                }))
            }))
        };
        Ok(OpenedDir::new(
            identity,
            Box::new(SimpleOpenDir { fs: self, entries }),
        ))
    }

    fn hash_file_sha256(
        &self,
        _path: &Path,
        _check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]> {
        Err(io::Error::other(
            "alias-registry boundary contains directories only",
        ))
    }

    fn entry_kind(&self, path: &Path) -> io::Result<EntryKind> {
        if path == self.root {
            Ok(EntryKind::Dir)
        } else {
            Ok(EntryKind::Symlink)
        }
    }
}

#[test]
fn public_hash_files_streams_wide_directories_and_caps_its_alias_registry() {
    // Invariant class: the injected filesystem is the public recording
    // boundary. It exposes 100,000 files but refuses a second directory pull
    // until the prior file is hashed, proving discovery stays streaming and
    // preserves native DFS order instead of retaining the wide directory.
    let yielded = Arc::new(AtomicUsize::new(0));
    let hashed = Arc::new(AtomicUsize::new(0));
    let context = Context::new(Arc::new(StreamingWideDirectoryFs {
        root: PathBuf::from("/workspace"),
        yielded: Arc::clone(&yielded),
        hashed: Arc::clone(&hashed),
    }));
    let value = evaluate_source("hashFiles('**/*.txt')", &context)
        .expect("wide-directory hashFiles evaluation");
    assert!(matches!(value, Value::String(digest) if digest.len() == 64));
    assert_eq!(yielded.load(Ordering::Acquire), WIDE_DIRECTORY_FILES);
    assert_eq!(hashed.load(Ordering::Acquire), WIDE_DIRECTORY_FILES);

    // The same pull boundary pins true DFS: traversal must fully descend into
    // the first child and hash its file before requesting the parent's later
    // sibling. This keeps state proportional to depth without changing the
    // toolkit's observable generator order.
    let child_hashed = Arc::new(AtomicBool::new(false));
    let context = Context::new(Arc::new(DepthFirstPullFs {
        root: PathBuf::from("/workspace"),
        child_hashed: Arc::clone(&child_hashed),
    }));
    let value = evaluate_source("hashFiles('**/*.txt')", &context)
        .expect("depth-first hashFiles evaluation");
    assert!(matches!(value, Value::String(digest) if digest.len() == 64));
    assert!(child_hashed.load(Ordering::Acquire));

    // The exact first-alias registry is a separate, fixed memory budget.
    // Infinite public-boundary streams independently pin its identity-count
    // and retained-directory-identity-byte ceilings without testing a private
    // traversal helper.
    let rows = [
        (
            RegistryBoundary::DirectoryCount,
            "directory count",
            262_144_usize,
        ),
        (
            RegistryBoundary::RetainedPathBytes,
            "retained path bytes",
            32 * 1024 * 1024,
        ),
    ];
    for (boundary, name, expected_limit) in rows {
        let context = Context::new(Arc::new(RegistryBoundaryFs {
            root: PathBuf::from("/workspace"),
            boundary,
        }));
        let error = evaluate_source("hashFiles('--follow-symbolic-links', '**/*.txt')", &context)
            .expect_err("unbounded alias stream must hit its fixed registry limit");
        let rendered = error.to_string();
        let actual_limit = match error {
            EvalError::HashFiles(HashFilesError::DirectoryCountLimit { max_directories }) => {
                max_directories
            }
            EvalError::HashFiles(HashFilesError::DirectoryPathBytesLimit { max_bytes }) => {
                max_bytes
            }
            other => panic!("{name} returned an unexpected error: {other:?}"),
        };
        assert_eq!(actual_limit, expected_limit, "{name}");
        assert!(
            rendered.contains("narrow the patterns to traverse fewer canonical directories"),
            "{name} lacked corrective guidance: {rendered}"
        );
    }
}

/// Records whether its deliberately "unreadable" `src/vendor` directory was
/// ever opened, to pin toolkit-equivalent `match || partialMatch` pruning
/// (finding 5 of the hashFiles hardening review): `src/*.js` must never
/// even attempt to open an unrelated sibling directory, exactly as GitHub's
/// own glob walker skips it.
/// https://github.com/actions/toolkit/blob/main/packages/glob/src/internal-globber.ts
#[derive(Debug)]
struct PruningRecorderFs {
    root: PathBuf,
    vendor_open_attempts: Arc<AtomicUsize>,
}

impl HashFilesFs for PruningRecorderFs {
    fn workspace_root(&self) -> &Path {
        &self.root
    }

    fn open_dir(&self, path: &Path) -> io::Result<OpenedDir<'_>> {
        if path == self.root.join("src/vendor") {
            self.vendor_open_attempts.fetch_add(1, Ordering::AcqRel);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "src/vendor is deliberately unreadable",
            ));
        }
        let entries: Box<dyn Iterator<Item = io::Result<DirEntry>>> =
            if path == self.root.join("src") {
                Box::new(
                    vec![
                        Ok(DirEntry {
                            name: "app.js".to_string(),
                            kind: EntryKind::File,
                        }),
                        Ok(DirEntry {
                            name: "vendor".to_string(),
                            kind: EntryKind::Dir,
                        }),
                    ]
                    .into_iter(),
                )
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("unexpected directory open: {}", path.display()),
                ));
            };
        Ok(OpenedDir::new(
            path.to_path_buf(),
            Box::new(SimpleOpenDir { fs: self, entries }),
        ))
    }

    fn hash_file_sha256(
        &self,
        path: &Path,
        check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]> {
        check_timeout()?;
        if path == self.root.join("src/app.js") {
            return Ok([9_u8; 32]);
        }
        Err(io::Error::other(format!(
            "unexpected file hash request: {}",
            path.display()
        )))
    }

    fn entry_kind(&self, path: &Path) -> io::Result<EntryKind> {
        if path == self.root.join("src") {
            Ok(EntryKind::Dir)
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("unexpected entry_kind query: {}", path.display()),
            ))
        }
    }
}

/// Records every `DT_UNKNOWN` resolution attempt by child name, to pin
/// finding 5 of the hashFiles hardening re-verification against the case
/// the previous fake couldn't express at all: a directory-enumeration
/// result whose kind is unreported must be pruned from the name/path alone,
/// exactly like a directly-reported kind, *before* the extra resolution
/// call `DT_UNKNOWN` requires is ever made — not merely before an unrelated
/// directory is opened.
/// https://github.com/actions/toolkit/blob/main/packages/glob/src/internal-globber.ts
#[derive(Debug)]
struct UnknownKindPruningFs {
    root: PathBuf,
    resolved: Arc<Mutex<Vec<String>>>,
}

impl HashFilesFs for UnknownKindPruningFs {
    fn workspace_root(&self) -> &Path {
        &self.root
    }

    fn open_dir(&self, path: &Path) -> io::Result<OpenedDir<'_>> {
        if path != self.root.join("src") {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("unexpected directory open: {}", path.display()),
            ));
        }
        let entries: Box<dyn Iterator<Item = io::Result<DirEntry>>> = Box::new(
            vec![
                Ok(DirEntry {
                    name: "app.js".to_string(),
                    kind: EntryKind::Unknown,
                }),
                Ok(DirEntry {
                    name: "vendor".to_string(),
                    kind: EntryKind::Unknown,
                }),
            ]
            .into_iter(),
        );
        Ok(OpenedDir::new(
            path.to_path_buf(),
            Box::new(UnknownKindOpenDir { fs: self, entries }),
        ))
    }

    fn hash_file_sha256(
        &self,
        path: &Path,
        check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]> {
        check_timeout()?;
        if path == self.root.join("src/app.js") {
            return Ok([7_u8; 32]);
        }
        Err(io::Error::other(format!(
            "unexpected file hash request: {}",
            path.display()
        )))
    }

    fn entry_kind(&self, path: &Path) -> io::Result<EntryKind> {
        if path == self.root.join("src") {
            Ok(EntryKind::Dir)
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("unexpected entry_kind query: {}", path.display()),
            ))
        }
    }
}

struct UnknownKindOpenDir<'a> {
    fs: &'a UnknownKindPruningFs,
    entries: Box<dyn Iterator<Item = io::Result<DirEntry>> + 'a>,
}

impl<'a> OpenDirectory<'a> for UnknownKindOpenDir<'a> {
    fn next_entry(&mut self) -> io::Result<Option<DirEntry>> {
        self.entries.next().transpose()
    }

    fn open_child_dir(&self, _name: &str, path: &Path, _follow: bool) -> io::Result<OpenedDir<'a>> {
        self.fs.open_dir(path)
    }

    fn hash_child_file(
        &self,
        _name: &str,
        path: &Path,
        _follow: bool,
        check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]> {
        self.fs.hash_file_sha256(path, check_timeout)
    }

    fn resolve_unknown_kind(&self, name: &str, lexical_path: &Path) -> io::Result<EntryKind> {
        self.fs
            .resolved
            .lock()
            .expect("resolved-kind recorder mutex")
            .push(name.to_string());
        if lexical_path == self.fs.root.join("src/app.js") {
            Ok(EntryKind::File)
        } else if lexical_path == self.fs.root.join("src/vendor") {
            // Reached only if the invariant this test pins is broken; a
            // plain I/O error surfaces through `evaluate` rather than
            // panicking this thread, matching every other "unexpected"
            // fixture query in this file.
            Err(io::Error::other(
                "an irrelevant DT_UNKNOWN entry must never be resolved: src/vendor",
            ))
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "unexpected resolve_unknown_kind query: {}",
                    lexical_path.display()
                ),
            ))
        }
    }
}

#[test]
fn public_hash_files_prunes_unrelated_directories_before_opening_them() {
    // Invariant class: an unreadable (or merely irrelevant) directory that
    // cannot contribute a match must never be opened or statted at all —
    // not "opened then its error tolerated". `src/vendor` records every
    // open attempt directly at the public recording boundary.
    let vendor_open_attempts = Arc::new(AtomicUsize::new(0));
    let context = Context::new(Arc::new(PruningRecorderFs {
        root: PathBuf::from("/workspace"),
        vendor_open_attempts: Arc::clone(&vendor_open_attempts),
    }));
    let value = evaluate_source("hashFiles('src/*.js')", &context)
        .expect("pruned hashFiles evaluation over an unreadable sibling");
    assert!(
        matches!(&value, Value::String(digest) if digest.len() == 64),
        "literal-prefix rooting must still reach and hash the matching file: {value:?}"
    );
    assert_eq!(
        vendor_open_attempts.load(Ordering::Acquire),
        0,
        "an unrelated directory must never be opened, not merely tolerated when it errors"
    );

    // Finding 5's `DT_UNKNOWN` case specifically: both siblings' native
    // enumeration type is unreported, so resolving either one costs an
    // extra call. Only the one pruning decides is still relevant may ever
    // pay that cost.
    let resolved = Arc::new(Mutex::new(Vec::new()));
    let context = Context::new(Arc::new(UnknownKindPruningFs {
        root: PathBuf::from("/workspace"),
        resolved: Arc::clone(&resolved),
    }));
    let value = evaluate_source("hashFiles('src/*.js')", &context)
        .expect("pruned hashFiles evaluation over DT_UNKNOWN entries");
    assert!(
        matches!(&value, Value::String(digest) if digest.len() == 64),
        "a relevant DT_UNKNOWN entry must still be resolved, matched, and hashed: {value:?}"
    );
    assert_eq!(
        resolved
            .lock()
            .expect("resolved-kind recorder mutex")
            .as_slice(),
        ["app.js"],
        "an irrelevant DT_UNKNOWN sibling must never be resolved, only the matching one"
    );
}

fn evaluate_source(source: &str, context: &Context) -> Result<Value, EvalError> {
    let expression = parse(source).unwrap_or_else(|error| panic!("parse({source:?}): {error}"));
    evaluate(&expression, context)
}
