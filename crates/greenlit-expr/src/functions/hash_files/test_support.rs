//! In-memory filesystem fakes for oracle tests at the true I/O boundary.

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use super::{DirEntry, EntryKind, HashFilesFs};

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

    fn read_dir(&self, _path: &Path) -> io::Result<Vec<DirEntry>> {
        Ok(Vec::new())
    }

    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no such file: {}", path.display()),
        ))
    }
}

/// An in-memory tree whose directories are inferred from file prefixes.
/// Directory entries preserve insertion order for deterministic hashes.
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

    fn direct_children(&self, dir: &Path) -> Vec<DirEntry> {
        let mut seen_dirs = std::collections::BTreeSet::new();
        let mut entries = Vec::new();
        let push_unique = |name: String, kind: EntryKind, entries: &mut Vec<DirEntry>| {
            if !entries.iter().any(|entry| entry.name == name) {
                entries.push(DirEntry { name, kind });
            }
        };
        for (path, _) in &self.files {
            if let Ok(rest) = path.strip_prefix(dir) {
                let mut components = rest.components();
                if let Some(first) = components.next() {
                    let name = first.as_os_str().to_string_lossy().into_owned();
                    if components.next().is_some() {
                        if seen_dirs.insert(name.clone()) {
                            push_unique(name, EntryKind::Dir, &mut entries);
                        }
                    } else {
                        push_unique(name, EntryKind::File, &mut entries);
                    }
                }
            }
        }
        for path in self.symlinks.keys() {
            if let Ok(rest) = path.strip_prefix(dir) {
                let mut components = rest.components();
                if let Some(first) = components.next()
                    && components.next().is_none()
                {
                    let name = first.as_os_str().to_string_lossy().into_owned();
                    push_unique(name, EntryKind::Symlink, &mut entries);
                }
            }
        }
        entries
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
}

impl HashFilesFs for InMemoryFs {
    fn workspace_root(&self) -> &Path {
        &self.root
    }

    fn home_dir(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        let resolved = self.resolve(path)?;
        if !self.is_directory(&resolved) {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("not a directory: {}", path.display()),
            ));
        }
        Ok(self.direct_children(&resolved))
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

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.resolve(path)
    }
}
