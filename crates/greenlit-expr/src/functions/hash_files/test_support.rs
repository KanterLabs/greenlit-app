//! In-memory filesystem fakes for oracle tests at the true I/O boundary.

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use super::{DirEntry, EntryKind, HashFilesFs, OpenDirectory, OpenedDir};

/// A fake with no files anywhere, for evaluations that do not call
/// `hashFiles()` but still require a context filesystem.
#[derive(Debug)]
pub struct NoFiles {
    root: PathBuf,
}

impl NoFiles {
    /// Builds a fake rooted at `root` with no files anywhere.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        NoFiles { root: root.into() }
    }
}

impl HashFilesFs for NoFiles {
    fn workspace_root(&self) -> &Path {
        &self.root
    }

    fn open_dir(&self, path: &Path) -> io::Result<OpenedDir<'_>> {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no such directory: {}", path.display()),
        ))
    }

    fn entry_kind(&self, path: &Path) -> io::Result<EntryKind> {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no such path: {}", path.display()),
        ))
    }

    fn hash_file_sha256(
        &self,
        path: &Path,
        _check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]> {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no such file: {}", path.display()),
        ))
    }
}

/// An in-memory tree whose directories are inferred from file prefixes.
/// Directory entries preserve insertion order for deterministic hashes.
/// Children are resolved by joining a bare name onto this directory's own
/// already-resolved lexical path rather than by reopening from the root;
/// this fake has no real TOCTOU exposure (it holds no descriptors), so the
/// path-joining that `RealFs` must avoid for security is harmless here.
#[derive(Debug, Default)]
pub struct InMemoryFs {
    root: PathBuf,
    home: Option<PathBuf>,
    files: Vec<(PathBuf, Vec<u8>)>,
    symlinks: BTreeMap<PathBuf, PathBuf>,
}

impl InMemoryFs {
    /// Builds an empty in-memory tree rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        InMemoryFs {
            root: root.into(),
            home: None,
            files: Vec::new(),
            symlinks: BTreeMap::new(),
        }
    }

    /// Adds a file at `path` in insertion order.
    pub fn with_file(mut self, path: impl Into<PathBuf>, content: impl Into<Vec<u8>>) -> Self {
        self.files.push((path.into(), content.into()));
        self
    }

    fn is_directory(&self, path: &Path) -> bool {
        path == self.root
            || self.home.as_deref() == Some(path)
            || self.files.iter().any(|(candidate, _)| {
                candidate
                    .strip_prefix(path)
                    .is_ok_and(|rest| !rest.as_os_str().is_empty())
            })
            || self.symlinks.keys().any(|candidate| {
                candidate
                    .strip_prefix(path)
                    .is_ok_and(|rest| !rest.as_os_str().is_empty())
            })
    }

    fn resolve(&self, path: &Path) -> io::Result<PathBuf> {
        let mut current = path.to_path_buf();
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(current.clone()) {
                return Err(io::Error::other(format!(
                    "symbolic-link cycle at {}",
                    path.display()
                )));
            }
            let mut next = None;
            for (link, target) in &self.symlinks {
                if let Ok(rest) = current.strip_prefix(link) {
                    next = Some(if rest.as_os_str().is_empty() {
                        target.clone()
                    } else {
                        target.join(rest)
                    });
                    break;
                }
            }
            match next {
                Some(path) => current = path,
                None => return Ok(current),
            }
        }
    }

    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        let resolved = self.resolve(path)?;
        self.files
            .iter()
            .find(|(candidate, _)| *candidate == resolved)
            .map(|(_, content)| content.clone())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no such file: {}", path.display()),
                )
            })
    }

    fn open_directory(&self, path: &Path) -> io::Result<OpenedDir<'_>> {
        let resolved = self.resolve(path)?;
        if !self.is_directory(&resolved) {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("not a directory: {}", path.display()),
            ));
        }
        Ok(OpenedDir::new(
            resolved.clone(),
            Box::new(InMemoryOpenDir {
                fs: self,
                dir: resolved,
                file_index: 0,
                symlink_index: 0,
            }),
        ))
    }
}

impl HashFilesFs for InMemoryFs {
    fn workspace_root(&self) -> &Path {
        &self.root
    }

    fn home_dir(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    fn open_dir(&self, path: &Path) -> io::Result<OpenedDir<'_>> {
        self.open_directory(path)
    }

    fn entry_kind(&self, path: &Path) -> io::Result<EntryKind> {
        if self.symlinks.contains_key(path) {
            return Ok(EntryKind::Symlink);
        }
        let resolved = self.resolve(path)?;
        if self
            .files
            .iter()
            .any(|(candidate, _)| *candidate == resolved)
        {
            Ok(EntryKind::File)
        } else if self.is_directory(&resolved) {
            Ok(EntryKind::Dir)
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no such path: {}", path.display()),
            ))
        }
    }

    fn hash_file_sha256(
        &self,
        path: &Path,
        check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]> {
        hash_bytes(&self.read_file(path)?, check_timeout)
    }
}

pub(super) fn hash_bytes(
    contents: &[u8],
    check_timeout: &mut dyn FnMut() -> io::Result<()>,
) -> io::Result<[u8; 32]> {
    use sha2::{Digest, Sha256};
    check_timeout()?;
    let mut hash = Sha256::new();
    for chunk in contents.chunks(64 * 1024) {
        check_timeout()?;
        hash.update(chunk);
    }
    check_timeout()?;
    Ok(hash.finalize().into())
}

struct InMemoryOpenDir<'a> {
    fs: &'a InMemoryFs,
    dir: PathBuf,
    file_index: usize,
    symlink_index: usize,
}

impl<'a> OpenDirectory<'a> for InMemoryOpenDir<'a> {
    fn next_entry(&mut self) -> io::Result<Option<DirEntry>> {
        while self.file_index < self.fs.files.len() {
            let index = self.file_index;
            self.file_index += 1;
            let Some(entry) = file_child(&self.fs.files[index].0, &self.dir) else {
                continue;
            };
            let appeared_earlier = self.fs.files[..index]
                .iter()
                .filter_map(|(path, _)| file_child(path, &self.dir))
                .any(|candidate| candidate.name == entry.name);
            if !appeared_earlier {
                return Ok(Some(entry));
            }
        }

        while self.symlink_index < self.fs.symlinks.len() {
            let index = self.symlink_index;
            self.symlink_index += 1;
            let Some(path) = self.fs.symlinks.keys().nth(index) else {
                continue;
            };
            let Some(entry) = direct_symlink_child(path, &self.dir) else {
                continue;
            };
            let appeared_as_file = self
                .fs
                .files
                .iter()
                .filter_map(|(path, _)| file_child(path, &self.dir))
                .any(|candidate| candidate.name == entry.name);
            let appeared_as_symlink = self
                .fs
                .symlinks
                .keys()
                .take(index)
                .filter_map(|path| direct_symlink_child(path, &self.dir))
                .any(|candidate| candidate.name == entry.name);
            if !appeared_as_file && !appeared_as_symlink {
                return Ok(Some(entry));
            }
        }
        Ok(None)
    }

    fn open_child_dir(
        &self,
        name: &str,
        _lexical_path: &Path,
        follow: bool,
    ) -> io::Result<OpenedDir<'a>> {
        let child = self.dir.join(name);
        let target = if follow {
            self.fs.resolve(&child)?
        } else if self.fs.symlinks.contains_key(&child) {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("not a directory: {}", child.display()),
            ));
        } else {
            child
        };
        if !self.fs.is_directory(&target) {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("not a directory: {}", target.display()),
            ));
        }
        self.fs.open_directory(&target)
    }

    fn hash_child_file(
        &self,
        name: &str,
        _lexical_path: &Path,
        follow: bool,
        check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]> {
        let child = self.dir.join(name);
        let target = if follow {
            self.fs.resolve(&child)?
        } else {
            child
        };
        hash_bytes(&self.fs.read_file(&target)?, check_timeout)
    }
}

fn file_child(path: &Path, dir: &Path) -> Option<DirEntry> {
    let rest = path.strip_prefix(dir).ok()?;
    let mut components = rest.components();
    let first = components.next()?;
    Some(DirEntry {
        name: first.as_os_str().to_string_lossy().into_owned(),
        kind: if components.next().is_some() {
            EntryKind::Dir
        } else {
            EntryKind::File
        },
    })
}

fn direct_symlink_child(path: &Path, dir: &Path) -> Option<DirEntry> {
    let rest = path.strip_prefix(dir).ok()?;
    let mut components = rest.components();
    let first = components.next()?;
    if components.next().is_some() {
        return None;
    }
    Some(DirEntry {
        name: first.as_os_str().to_string_lossy().into_owned(),
        kind: EntryKind::Symlink,
    })
}
