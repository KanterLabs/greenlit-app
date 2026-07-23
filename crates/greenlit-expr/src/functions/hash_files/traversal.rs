//! Directory traversal in toolkit-compatible discovery order.

use std::io;
use std::path::{Path, PathBuf};

use super::deadline::HashFilesDeadline;
use super::patterns::{CompiledPattern, could_match_descendant, is_included};
use super::{EntryKind, HashFilesError, HashFilesFs, OpenedDir};

// These are security/resource ceilings rather than runner semantics. The
// runner's helper-process timeout bounds elapsed time but not memory retained
// while suppressing a compact symbolic-link alias graph. The limits allow a
// large normal checkout while keeping that untrusted registry below a fixed
// process-memory envelope.
const MAX_TRACKED_DIRECTORIES: usize = 262_144;
const MAX_TRACKED_CANONICAL_PATH_BYTES: usize = 32 * 1024 * 1024;

/// Exact canonical-directory identities retained up to fixed count and byte
/// ceilings. Directory enumeration itself retains only the active DFS stack;
/// this separate registry ensures a compact alias DAG cannot amplify work.
pub(super) struct DirectoryRegistry {
    paths: std::collections::HashSet<Box<Path>>,
    retained_path_bytes: usize,
}

impl DirectoryRegistry {
    pub(super) fn new() -> Self {
        Self {
            paths: std::collections::HashSet::new(),
            retained_path_bytes: 0,
        }
    }

    fn record(&mut self, canonical: PathBuf) -> Result<bool, HashFilesError> {
        if self.paths.contains(canonical.as_path()) {
            return Ok(false);
        }
        if self.paths.len() >= MAX_TRACKED_DIRECTORIES {
            return Err(HashFilesError::DirectoryCountLimit {
                max_directories: MAX_TRACKED_DIRECTORIES,
            });
        }
        let path_bytes = canonical.as_os_str().as_encoded_bytes().len();
        let retained_path_bytes = self
            .retained_path_bytes
            .checked_add(path_bytes)
            .filter(|total| *total <= MAX_TRACKED_CANONICAL_PATH_BYTES)
            .ok_or(HashFilesError::DirectoryPathBytesLimit {
                max_bytes: MAX_TRACKED_CANONICAL_PATH_BYTES,
            })?;
        self.paths.insert(canonical.into_boxed_path());
        self.retained_path_bytes = retained_path_bytes;
        Ok(true)
    }
}

/// Where a matched file's bytes come from: an entry point resolved
/// directly by [`HashFilesFs`], or a child resolved relative to an
/// already-open ancestor [`OpenedDir`] (findings 1/6/7 of the hashFiles
/// hardening review — never a lexical path re-resolved from the root).
pub(super) enum HashSource<'p, 'fs> {
    /// `path` is itself an entry point (a literal search root with no
    /// wildcard segment).
    EntryPoint,
    /// `path` is `name`, a child of `parent`.
    Child {
        parent: &'p OpenedDir<'fs>,
        name: &'p str,
        follow: bool,
    },
}

/// One directory frame kept open on the traversal's own explicit stack —
/// exactly the ancestors currently between the search root and wherever
/// the DFS is descending.
///
/// `path` (the caller's single shared buffer) carries exactly one extra
/// component for every non-root frame, pushed when that frame was entered;
/// `pushed` records whether ascending back out of this frame must pop that
/// one component. Because each frame owns only that flag — never a copy of
/// the path itself — retained lexical state stays proportional to depth
/// (finding 4 of the hashFiles hardening review), not to depth squared.
struct Frame<'fs> {
    opened: OpenedDir<'fs>,
    pushed: bool,
}

/// Traverses one toolkit-derived search root in its native directory-read
/// order. Symbolic links use lstat-like entry kinds: with following off, a
/// link to a file is still yielded while a link to a directory is not
/// traversed. The runner remembers canonical directories only in the active
/// DFS chain. A compact in-workspace symlink DAG can therefore revisit the
/// same target exponentially before its 120-second child-process timeout,
/// amplifying filesystem work without a safe memory bound.
/// Greenlit deliberately strengthens that policy to one visit per canonical
/// directory across the invocation: the first-discovered lexical path keeps
/// runner order, while later aliases are skipped. This exact registry has
/// fixed entry and retained-path-byte ceilings; crossing either fails with a
/// corrective action instead of growing without bound. This can omit duplicate
/// alias-path hashes the runner would include, but prevents untrusted checkout
/// topology from exhausting the host process before the deadline fires.
/// https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Misc/layoutbin/hashFiles/index.js
///
/// Every child is reached through the parent's own [`OpenedDir`]
/// (`open_child_dir`/`hash_child_file`), never by rejoining a lexical path
/// and asking the filesystem to re-resolve it from the workspace root, and
/// before either is even attempted, `compiled` decides whether the
/// candidate could still lead to a match at all (finding 5's toolkit-style
/// `match || partialMatch` pruning) — see `filesystem/real/node.rs` and
/// `patterns.rs::could_match_descendant`.
pub(super) fn walk_search_root<'fs>(
    fs: &'fs dyn HashFilesFs,
    root: &Path,
    follow_symlinks: bool,
    compiled: &[CompiledPattern],
    visit_file: &mut dyn FnMut(&Path, HashSource<'_, 'fs>, bool) -> Result<(), HashFilesError>,
    visited_directories: &mut DirectoryRegistry,
    deadline: &HashFilesDeadline,
) -> Result<(), HashFilesError> {
    deadline.check()?;
    let root_kind_result = fs.entry_kind(root);
    deadline.check()?;
    let root_kind = match root_kind_result {
        Ok(kind) => kind,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(deadline.map_io(root, source)),
    };

    let mut path = root.to_path_buf();
    let mut stack: Vec<Frame<'fs>> = Vec::new();

    match root_kind {
        // `entry_kind` always resolves the real type via `fstat` (finding 5
        // only concerns directory *enumeration*'s `DT_UNKNOWN`); kept only
        // for `match` exhaustiveness.
        EntryKind::Unknown => {
            return Err(deadline.map_io(
                &path,
                io::Error::other("hashFiles entry_kind unexpectedly returned an unresolved kind"),
            ));
        }
        // New defect: a search root reached here is only a candidate —
        // GitHub yields a non-directory only on a *full* pattern match
        // (`match`, negatives folded), never merely because it was the
        // literal prefix a glob pattern rooted at.
        // https://github.com/actions/toolkit/blob/main/packages/glob/src/internal-globber.ts
        EntryKind::File => {
            if is_included(compiled, &path) {
                return visit_file(&path, HashSource::EntryPoint, false);
            }
            return Ok(());
        }
        EntryKind::Dir => {
            let opened = fs
                .open_dir(&path)
                .map_err(|source| deadline.map_io(&path, source))?;
            deadline.check()?;
            if visited_directories.record(opened.identity.clone())? {
                stack.push(Frame {
                    opened,
                    pushed: false,
                });
            } else {
                return Ok(());
            }
        }
        EntryKind::Symlink => {
            // Identity and enumeration must come from one opened directory
            // object; the probe below either becomes the frame we descend
            // into or is dropped entirely (never traversed).
            deadline.check()?;
            match fs.open_dir(&path) {
                Ok(opened) if follow_symlinks => {
                    deadline.check()?;
                    if visited_directories.record(opened.identity.clone())? {
                        stack.push(Frame {
                            opened,
                            pushed: false,
                        });
                    }
                }
                Ok(_) => {}
                Err(source) if source.kind() == io::ErrorKind::TimedOut => {
                    return Err(deadline.map_io(&path, source));
                }
                Err(_) => {
                    // lstat exposes the link itself as a non-directory
                    // candidate; the outer hasher's own read decides
                    // whether a dangling target is a lenient skip or a hard
                    // failure (finding 8) — but only on a *full* match (new
                    // defect: a partial-only match — a dangling link merely
                    // sitting at a glob's literal search root — must yield
                    // nothing, not a `DanglingSymlink` failure).
                    if is_included(compiled, &path) {
                        return visit_file(&path, HashSource::EntryPoint, true);
                    }
                    return Ok(());
                }
            }
        }
        // FIFOs, sockets, and devices must never be opened as files: doing
        // so can block indefinitely or expose host resources.
        EntryKind::Other => return Ok(()),
    }

    drive(
        &mut path,
        follow_symlinks,
        compiled,
        visit_file,
        visited_directories,
        deadline,
        &mut stack,
    )
}

fn drive<'fs>(
    path: &mut PathBuf,
    follow_symlinks: bool,
    compiled: &[CompiledPattern],
    visit_file: &mut dyn FnMut(&Path, HashSource<'_, 'fs>, bool) -> Result<(), HashFilesError>,
    visited_directories: &mut DirectoryRegistry,
    deadline: &HashFilesDeadline,
    stack: &mut Vec<Frame<'fs>>,
) -> Result<(), HashFilesError> {
    while let Some(top) = stack.last_mut() {
        deadline.check()?;
        let pulled = top.opened.next_entry();
        // The loop-level check is immediately before this potentially
        // blocking pull. Check again immediately afterward so an entry that
        // completes at the fixed deadline cannot begin more traversal or
        // hashing work.
        deadline.check()?;
        let entry = match pulled {
            Ok(Some(entry)) => entry,
            Ok(None) => {
                let pushed = top.pushed;
                stack.pop();
                if pushed {
                    path.pop();
                }
                continue;
            }
            Err(source) => return Err(deadline.map_io(path, source)),
        };

        path.push(&entry.name);

        let kind = if entry.kind == EntryKind::Unknown {
            // Finding 5: decide relevance from the name/path alone
            // (toolkit's own `match || partialMatch`) *before* spending the
            // extra resolution call `DT_UNKNOWN` forces — an irrelevant
            // entry must never be opened or `stat`ed just because its
            // native type was unreported.
            // https://github.com/actions/toolkit/blob/main/packages/glob/src/internal-globber.ts
            if !is_included(compiled, path) && !could_match_descendant(compiled, path) {
                path.pop();
                continue;
            }
            let parent = &stack
                .last()
                .ok_or_else(|| io::Error::other("hashFiles traversal stack is empty"))
                .map_err(|source| deadline.map_io(path, source))?
                .opened;
            match parent.resolve_unknown_kind(&entry.name, path) {
                Ok(resolved) => {
                    deadline.check()?;
                    resolved
                }
                Err(source) if source.kind() == io::ErrorKind::NotFound => {
                    // Vanished between enumeration and resolution — never a
                    // hard failure (see the `EntryKind::Dir` vanished-entry
                    // handling below for the same rationale).
                    path.pop();
                    continue;
                }
                Err(source) => return Err(deadline.map_io(path, source)),
            }
        } else {
            entry.kind
        };

        match kind {
            EntryKind::Unknown => {
                // `resolve_unknown_kind` implementations must resolve to a
                // concrete kind; this exists only for `match` exhaustiveness.
                return Err(deadline.map_io(
                    path,
                    io::Error::other("hashFiles resolved an entry to an unknown kind"),
                ));
            }
            EntryKind::Other => {
                path.pop();
            }
            EntryKind::File => {
                if is_included(compiled, path) {
                    let parent = &stack
                        .last()
                        .ok_or_else(|| io::Error::other("hashFiles traversal stack is empty"))
                        .map_err(|source| deadline.map_io(path, source))?
                        .opened;
                    visit_file(
                        path,
                        HashSource::Child {
                            parent,
                            name: &entry.name,
                            follow: false,
                        },
                        false,
                    )?;
                }
                path.pop();
            }
            EntryKind::Dir => {
                if could_match_descendant(compiled, path) {
                    let opened_result = stack
                        .last()
                        .ok_or_else(|| io::Error::other("hashFiles traversal stack is empty"))
                        .and_then(|top| top.opened.open_child_dir(&entry.name, path, false));
                    match opened_result {
                        Ok(opened) => {
                            deadline.check()?;
                            if visited_directories.record(opened.identity.clone())? {
                                stack.push(Frame {
                                    opened,
                                    pushed: true,
                                });
                            } else {
                                path.pop();
                            }
                        }
                        Err(source) if source.kind() == io::ErrorKind::NotFound => {
                            // The rename-escape fix (finding 1) turns a
                            // renamed-away ancestor into `ENOENT` instead of
                            // silently resolving through the stale object;
                            // an entry a moment ago listed is now simply
                            // gone, exactly like the search root's own
                            // vanished-before-traversal case above — never
                            // a hard failure, and never outside content.
                            path.pop();
                        }
                        Err(source) => return Err(deadline.map_io(path, source)),
                    }
                } else {
                    path.pop();
                }
            }
            EntryKind::Symlink => {
                if !is_included(compiled, path) && !could_match_descendant(compiled, path) {
                    path.pop();
                    continue;
                }
                let probe = stack
                    .last()
                    .ok_or_else(|| io::Error::other("hashFiles traversal stack is empty"))
                    .and_then(|top| top.opened.open_child_dir(&entry.name, path, true));
                match probe {
                    Ok(opened) if follow_symlinks => {
                        deadline.check()?;
                        if visited_directories.record(opened.identity.clone())? {
                            stack.push(Frame {
                                opened,
                                pushed: true,
                            });
                        } else {
                            path.pop();
                        }
                    }
                    Ok(_) => {
                        path.pop();
                    }
                    Err(source) if source.kind() == io::ErrorKind::TimedOut => {
                        return Err(deadline.map_io(path, source));
                    }
                    Err(_) => {
                        let parent = &stack
                            .last()
                            .ok_or_else(|| io::Error::other("hashFiles traversal stack is empty"))
                            .map_err(|source| deadline.map_io(path, source))?
                            .opened;
                        visit_file(
                            path,
                            HashSource::Child {
                                parent,
                                name: &entry.name,
                                follow: true,
                            },
                            true,
                        )?;
                        path.pop();
                    }
                }
            }
        }
    }
    Ok(())
}
