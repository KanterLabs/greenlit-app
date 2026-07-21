//! Directory traversal in toolkit-compatible discovery order.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::{EntryKind, HashFilesError, HashFilesFs};

enum WalkItem {
    Visit(PathBuf, EntryKind),
    LeaveDirectory(PathBuf),
}

/// Traverses one toolkit-derived search root in its native directory-read
/// order. Symbolic links use lstat-like entry kinds: with following off, a
/// link to a file is still yielded while a link to a directory is not
/// traversed. With following on, canonical directories in the active DFS
/// chain detect cycles without imposing an arbitrary depth ceiling.
/// https://github.com/actions/toolkit/blob/main/packages/glob/src/internal-globber.ts
pub(super) fn walk_search_root(
    fs: &dyn HashFilesFs,
    root: &Path,
    follow_symlinks: bool,
    out: &mut Vec<PathBuf>,
    symlink_sourced: &mut HashSet<PathBuf>,
) -> Result<(), HashFilesError> {
    let root_kind = match fs.entry_kind(root) {
        Ok(kind) => kind,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(HashFilesError::Io {
                path: root.display().to_string(),
                source,
            });
        }
    };
    let mut stack = vec![WalkItem::Visit(root.to_path_buf(), root_kind)];
    let mut active_directories = HashSet::new();
    while let Some(item) = stack.pop() {
        match item {
            WalkItem::LeaveDirectory(canonical) => {
                active_directories.remove(&canonical);
            }
            WalkItem::Visit(path, EntryKind::File) => out.push(path),
            WalkItem::Visit(path, EntryKind::Dir) => {
                let canonical = fs
                    .canonicalize(&path)
                    .map_err(|source| HashFilesError::Io {
                        path: path.display().to_string(),
                        source,
                    })?;
                if !active_directories.insert(canonical.clone()) {
                    continue;
                }
                let entries = fs.read_dir(&path).map_err(|source| HashFilesError::Io {
                    path: path.display().to_string(),
                    source,
                })?;
                stack.push(WalkItem::LeaveDirectory(canonical));
                for entry in entries.into_iter().rev() {
                    stack.push(WalkItem::Visit(path.join(entry.name), entry.kind));
                }
            }
            WalkItem::Visit(path, EntryKind::Symlink) => match fs.read_dir(&path) {
                Ok(_) if follow_symlinks => stack.push(WalkItem::Visit(path, EntryKind::Dir)),
                Ok(_) => {}
                Err(_) => {
                    // lstat exposes the link itself as a non-directory
                    // match. The hash script's later file read follows a
                    // file link; a broken link is omitted there.
                    symlink_sourced.insert(path.clone());
                    out.push(path);
                }
            },
        }
    }
    Ok(())
}
