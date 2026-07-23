//! One opened directory: native-order entry streaming plus child access
//! resolved relative to this same held object (never by re-deriving a
//! lexical path — see `node.rs`).

use std::io;
use std::path::Path;
use std::sync::Arc;

use rustix::fs::{Dir, FileType, OFlags, fstat};

use crate::functions::hash_files::filesystem::{DirEntry, OpenDirectory, OpenedDir};

use super::node::DirNode;
use super::{Access, RealFs, entry_kind, errno_to_io, open_component};

pub(super) struct RealOpenDir<'a> {
    fs: &'a RealFs,
    node: Arc<DirNode>,
    directory: Dir,
}

impl<'a> RealOpenDir<'a> {
    pub(super) fn new(fs: &'a RealFs, node: Arc<DirNode>, directory: Dir) -> Self {
        Self {
            fs,
            node,
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
            let file_type = if entry.file_type() == FileType::Unknown {
                // `d_type` is not guaranteed by every filesystem; fall back
                // to an identity-only open of the SAME already-open
                // directory's own fd — still held-parent-relative, not a
                // re-resolved lexical path.
                let fd = self.directory.fd().map_err(errno_to_io)?;
                let inspected = open_component(
                    fd,
                    name.to_bytes(),
                    OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                )?;
                let stat = fstat(&inspected).map_err(errno_to_io)?;
                FileType::from_raw_mode(stat.st_mode)
            } else {
                entry.file_type()
            };
            return Ok(Some(DirEntry {
                name: String::from_utf8_lossy(name.to_bytes()).into_owned(),
                kind: entry_kind(file_type),
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
            &self.node,
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
                .open_relative(&self.node, name, lexical_path, follow, Access::ReadFile)?;
        check_timeout()?;
        super::hash_from_fd(resolved.target.fd, check_timeout)
    }
}
