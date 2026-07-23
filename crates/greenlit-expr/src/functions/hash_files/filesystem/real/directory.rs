//! One opened directory: native-order entry streaming plus child access
//! resolved root-relative from this directory's own accumulated path (never
//! by re-deriving a lexical path from scratch — see `node.rs`).

use std::io;
use std::path::{Path, PathBuf};

use rustix::fs::{Dir, FileType};

use crate::functions::hash_files::filesystem::{DirEntry, EntryKind, OpenDirectory, OpenedDir};

use super::{Access, RealFs, entry_kind, errno_to_io};

pub(super) struct RealOpenDir<'a> {
    fs: &'a RealFs,
    relative: PathBuf,
    directory: Dir,
}

impl<'a> RealOpenDir<'a> {
    pub(super) fn new(fs: &'a RealFs, relative: PathBuf, directory: Dir) -> Self {
        Self {
            fs,
            relative,
            directory,
        }
    }
}

impl<'a> OpenDirectory<'a> for RealOpenDir<'a> {
    fn next_entry(&mut self) -> io::Result<Option<DirEntry>> {
        loop {
            let Some(entry) = self.directory.next() else {
                return Ok(None);
            };
            let entry = entry.map_err(errno_to_io)?;
            let name = entry.file_name();
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            // `d_type` is not guaranteed by every filesystem (FUSE, some
            // network filesystems). Finding 5 of the hashFiles hardening
            // re-verification: resolving `DT_UNKNOWN` here — before the
            // traversal layer has decided whether this name could even
            // contribute a match — would `openat2`/`fstat` an entry the
            // toolkit's own `match || partialMatch` pruning would have
            // skipped outright.
            // https://github.com/actions/toolkit/blob/main/packages/glob/src/internal-globber.ts
            let kind = if entry.file_type() == FileType::Unknown {
                EntryKind::Unknown
            } else {
                entry_kind(entry.file_type())
            };
            return Ok(Some(DirEntry {
                name: String::from_utf8_lossy(name.to_bytes()).into_owned(),
                kind,
            }));
        }
    }

    fn open_child_dir(
        &self,
        name: &str,
        lexical_path: &Path,
        follow: bool,
    ) -> io::Result<OpenedDir<'a>> {
        let resolved = self.fs.open_relative(
            &self.relative,
            name,
            lexical_path,
            follow,
            Access::ReadDirectory,
        )?;
        super::wrap_dir(self.fs, resolved, lexical_path)
    }

    fn hash_child_file(
        &self,
        name: &str,
        lexical_path: &Path,
        follow: bool,
        check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]> {
        check_timeout()?;
        let resolved =
            self.fs
                .open_relative(&self.relative, name, lexical_path, follow, Access::ReadFile)?;
        check_timeout()?;
        super::hash_from_fd(resolved.target.fd, check_timeout)
    }

    fn resolve_unknown_kind(&self, name: &str, lexical_path: &Path) -> io::Result<EntryKind> {
        // Root-relative, `O_PATH`-only identity probe — the same resolver
        // and the same `openat2`/`RESOLVE_BENEATH` containment every other
        // access uses (finding 1), never the ancestor-fd-relative
        // `openat2` a `DT_UNKNOWN` entry used to get before the traversal
        // layer had even decided it was worth looking at (finding 5).
        let resolved =
            self.fs
                .open_relative(&self.relative, name, lexical_path, false, Access::Identity)?;
        Ok(entry_kind(FileType::from_raw_mode(
            resolved.target.stat.st_mode,
        )))
    }
}
