//! Exact private regular-file handling below the validated CAS root.

use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use rustix::fs::{Mode, OFlags, fchmod, openat};

use super::{CasError, io_error, unsafe_path_error, validate_owner};

const FILE_MODE: u32 = 0o600;

pub(super) fn create_new_file_at(
    parent: &File,
    path: &Path,
    name: &OsStr,
) -> Result<File, CasError> {
    let file = openat(
        parent,
        name,
        OFlags::RDWR
            | OFlags::CREATE
            | OFlags::EXCL
            | OFlags::CLOEXEC
            | OFlags::NOFOLLOW
            | OFlags::NONBLOCK,
        Mode::RUSR | Mode::WUSR,
    )
    .map(File::from)
    .map_err(|error| io_error(path, error.into()))?;
    let metadata = file.metadata().map_err(|error| io_error(path, error))?;
    validate_owner(path, &metadata)?;
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_file() || metadata.nlink() != 1 || mode & !FILE_MODE != 0 {
        return Err(unsafe_path_error(
            path,
            format!(
                "new file has unsafe type, mode 0{mode:03o}, or link count {}",
                metadata.nlink()
            ),
        ));
    }
    fchmod(&file, Mode::RUSR | Mode::WUSR).map_err(|error| io_error(path, error.into()))?;
    validate_file(path, &file)?;
    Ok(file)
}

pub(super) fn open_existing_file_at(
    parent: &File,
    path: &Path,
    name: &OsStr,
    access: OFlags,
) -> Result<File, CasError> {
    let file = openat(
        parent,
        name,
        access | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| io_error(path, error.into()))?;
    validate_file(path, &file)?;
    Ok(file)
}

fn validate_file(path: &Path, file: &File) -> Result<(), CasError> {
    let metadata = file.metadata().map_err(|error| io_error(path, error))?;
    validate_owner(path, &metadata)?;
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_file() || metadata.nlink() != 1 || mode != FILE_MODE {
        return Err(unsafe_path_error(
            path,
            format!(
                "file has unsafe type, mode 0{mode:03o}, or link count {}",
                metadata.nlink()
            ),
        ));
    }
    Ok(())
}
