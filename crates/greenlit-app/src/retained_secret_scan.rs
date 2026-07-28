use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::Path;

use anyhow::{Result, anyhow};
use rustix::fd::OwnedFd;
use rustix::fs::{
    CWD, Dir, FileType, Mode, OFlags, ResolveFlags, Stat, fstat, open, openat2, readlinkat_raw,
};

mod matcher;

use matcher::Matcher;

const READ_CHUNK_BYTES: usize = 16 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_ENTRIES: usize = 16 * 1024;
const MAX_NAME_BYTES: usize = 4 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_TOTAL_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_MATCH_WORK: u64 = 256 * 1024 * 1024;
const MAX_SYMLINK_TARGET_BYTES: usize = 64 * 1024;

const ROOT_RESOLVE: ResolveFlags = ResolveFlags::NO_MAGICLINKS.union(ResolveFlags::NO_SYMLINKS);
const CHILD_RESOLVE: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_XDEV)
    .union(ResolveFlags::NO_MAGICLINKS)
    .union(ResolveFlags::NO_SYMLINKS);

/// Scans every retained artifact below `run_root` and the exact prepared bytes
/// that will be published after the scan for sensitive values and the bounded
/// transport encodings registered by the runtime masker.
///
/// Values may be strings or arbitrary byte strings through `AsRef<[u8]>`.
/// The scan is descriptor-relative, does not follow links or read special
/// nodes, and fails closed when the retained tree or the requested pattern
/// set exceeds a fixed resource bound.
pub(crate) fn scan_retained_run_and_prepared_bytes<I, V>(
    run_root: &Path,
    sensitive_values: I,
    prepared_bytes: &[&[u8]],
) -> Result<()>
where
    I: IntoIterator<Item = V>,
    V: AsRef<[u8]>,
{
    let matcher = Matcher::new(sensitive_values)?;
    let mut budget = ScanBudget::default();
    for bytes in prepared_bytes {
        scan_prepared_bytes(bytes, &matcher, &mut budget)?;
    }
    let inspected = openat2(
        CWD,
        run_root,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
        ROOT_RESOLVE,
    )
    .map_err(|_| inspection_error())?;
    let root_stat = fstat(&inspected).map_err(|_| inspection_error())?;
    let current_uid = rustix::process::getuid().as_raw();
    validate_metadata(&root_stat, FileType::Directory, current_uid)?;
    let (root, stable_root) = reopen_validated(
        &inspected,
        &root_stat,
        FileType::Directory,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        current_uid,
    )?;
    let directory = Dir::new(root).map_err(|_| inspection_error())?;
    scan_directory(
        directory,
        &matcher,
        &mut budget,
        stable_root.st_dev,
        current_uid,
        0,
    )
}

fn scan_prepared_bytes(bytes: &[u8], matcher: &Matcher, budget: &mut ScanBudget) -> Result<()> {
    let bytes_len =
        u64::try_from(bytes.len()).map_err(|_| resource_error("retained-file-bytes"))?;
    budget.check_file_size(bytes_len)?;
    budget.charge_file_bytes(bytes_len)?;
    if matcher.matches(bytes, &mut vec![0; matcher.len()], budget)? {
        return Err(secret_found_error());
    }
    Ok(())
}

fn scan_directory(
    mut directory: Dir,
    matcher: &Matcher,
    budget: &mut ScanBudget,
    root_device: u64,
    current_uid: u32,
    depth: usize,
) -> Result<()> {
    let before =
        fstat(directory.fd().map_err(|_| inspection_error())?).map_err(|_| inspection_error())?;
    validate_metadata(&before, FileType::Directory, current_uid)?;
    while let Some(entry) = directory.read() {
        let entry = entry.map_err(|_| inspection_error())?;
        let name = entry.file_name().to_bytes();
        if matches!(name, b"." | b"..") {
            continue;
        }
        budget.charge_entry(name.len())?;
        if matcher.matches(name, &mut vec![0; matcher.len()], budget)? {
            return Err(secret_found_error());
        }

        let parent = directory.fd().map_err(|_| inspection_error())?;
        let inspected = openat2(
            parent,
            entry.file_name(),
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            CHILD_RESOLVE,
        )
        .map_err(|_| unsafe_entry_error())?;
        let stat = fstat(&inspected).map_err(|_| inspection_error())?;
        if stat.st_dev != root_device {
            return Err(unsafe_entry_error());
        }
        let file_type = FileType::from_raw_mode(stat.st_mode);
        validate_metadata(&stat, file_type, current_uid)?;
        match file_type {
            FileType::Directory => {
                if depth >= MAX_DEPTH {
                    return Err(resource_error("directory-depth"));
                }
                let (child, _) = reopen_validated(
                    &inspected,
                    &stat,
                    FileType::Directory,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    current_uid,
                )?;
                scan_directory(
                    Dir::new(child).map_err(|_| inspection_error())?,
                    matcher,
                    budget,
                    root_device,
                    current_uid,
                    depth + 1,
                )?;
            }
            FileType::RegularFile => {
                let (file, stable_stat) = reopen_validated(
                    &inspected,
                    &stat,
                    FileType::RegularFile,
                    OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    current_uid,
                )?;
                scan_file(File::from(file), &stable_stat, matcher, budget, current_uid)?;
            }
            FileType::Symlink => {
                scan_symlink(&inspected, &stat, matcher, budget, current_uid)?;
            }
            _ => return Err(unsafe_entry_error()),
        }
    }
    let after =
        fstat(directory.fd().map_err(|_| inspection_error())?).map_err(|_| inspection_error())?;
    validate_metadata(&after, FileType::Directory, current_uid)?;
    if changed_during_scan(&before, &after) {
        return Err(changed_tree_error());
    }
    Ok(())
}

fn scan_file(
    mut file: File,
    before: &Stat,
    matcher: &Matcher,
    budget: &mut ScanBudget,
    current_uid: u32,
) -> Result<()> {
    let expected_size = u64::try_from(before.st_size).map_err(|_| resource_error("file-size"))?;
    budget.check_file_size(expected_size)?;
    let mut states = vec![0; matcher.len()];
    let mut file_bytes = 0_u64;
    let mut buffer = [0_u8; READ_CHUNK_BYTES];
    loop {
        let read = file.read(&mut buffer).map_err(|_| inspection_error())?;
        if read == 0 {
            break;
        }
        let read = u64::try_from(read).map_err(|_| resource_error("file-size"))?;
        file_bytes = file_bytes
            .checked_add(read)
            .ok_or_else(|| resource_error("file-size"))?;
        if file_bytes > MAX_FILE_BYTES {
            return Err(resource_error("file-size"));
        }
        budget.charge_file_bytes(read)?;
        if matcher.matches(
            &buffer[..usize::try_from(read).map_err(|_| resource_error("file-size"))?],
            &mut states,
            budget,
        )? {
            return Err(secret_found_error());
        }
    }
    let after = fstat(&file).map_err(|_| inspection_error())?;
    validate_metadata(&after, FileType::RegularFile, current_uid)?;
    if changed_during_scan(before, &after)
        || file_bytes != u64::try_from(after.st_size).map_err(|_| resource_error("file-size"))?
    {
        return Err(changed_tree_error());
    }
    Ok(())
}

fn scan_symlink(
    inspected: &OwnedFd,
    before: &Stat,
    matcher: &Matcher,
    budget: &mut ScanBudget,
    current_uid: u32,
) -> Result<()> {
    let expected_size =
        usize::try_from(before.st_size).map_err(|_| resource_error("symlink-target-bytes"))?;
    if expected_size > MAX_SYMLINK_TARGET_BYTES {
        return Err(resource_error("symlink-target-bytes"));
    }
    let mut target = vec![0_u8; MAX_SYMLINK_TARGET_BYTES + 1];
    let bytes = readlinkat_raw(inspected, "", &mut target).map_err(|_| inspection_error())?;
    if bytes > MAX_SYMLINK_TARGET_BYTES || bytes != expected_size {
        return Err(changed_tree_error());
    }
    target.truncate(bytes);
    budget.charge_file_bytes(
        u64::try_from(bytes).map_err(|_| resource_error("symlink-target-bytes"))?,
    )?;
    if matcher.matches(&target, &mut vec![0; matcher.len()], budget)? {
        return Err(secret_found_error());
    }
    let after = fstat(inspected).map_err(|_| inspection_error())?;
    validate_metadata(&after, FileType::Symlink, current_uid)?;
    if changed_during_scan(before, &after) {
        return Err(changed_tree_error());
    }
    Ok(())
}

fn reopen_validated(
    inspected: &OwnedFd,
    expected: &Stat,
    expected_type: FileType,
    flags: OFlags,
    current_uid: u32,
) -> Result<(OwnedFd, Stat)> {
    let descriptor_path = format!("/proc/self/fd/{}", inspected.as_raw_fd());
    let reopened = open(descriptor_path, flags, Mode::empty()).map_err(|_| inspection_error())?;
    let actual = fstat(&reopened).map_err(|_| inspection_error())?;
    if (actual.st_dev, actual.st_ino) != (expected.st_dev, expected.st_ino)
        || FileType::from_raw_mode(actual.st_mode) != expected_type
    {
        return Err(unsafe_entry_error());
    }
    validate_metadata(&actual, expected_type, current_uid)?;
    if changed_during_scan(expected, &actual) {
        return Err(changed_tree_error());
    }
    Ok((reopened, actual))
}

fn changed_during_scan(before: &Stat, after: &Stat) -> bool {
    (before.st_dev, before.st_ino, before.st_size) != (after.st_dev, after.st_ino, after.st_size)
        || (
            before.st_mode,
            before.st_uid,
            before.st_gid,
            before.st_nlink,
        ) != (after.st_mode, after.st_uid, after.st_gid, after.st_nlink)
        || (before.st_mtime, before.st_mtime_nsec) != (after.st_mtime, after.st_mtime_nsec)
        || (before.st_ctime, before.st_ctime_nsec) != (after.st_ctime, after.st_ctime_nsec)
}

fn validate_metadata(stat: &Stat, file_type: FileType, current_uid: u32) -> Result<()> {
    if FileType::from_raw_mode(stat.st_mode) != file_type || stat.st_uid != current_uid {
        return Err(unsafe_metadata_error());
    }
    let mode = stat.st_mode & 0o7777;
    let safe = match file_type {
        FileType::Directory => mode == 0o700,
        FileType::RegularFile => mode == 0o600 && stat.st_nlink == 1,
        FileType::Symlink => stat.st_nlink == 1,
        _ => false,
    };
    if !safe {
        return Err(unsafe_metadata_error());
    }
    Ok(())
}

#[derive(Default)]
struct ScanBudget {
    entries: usize,
    name_bytes: usize,
    file_bytes: u64,
    match_work: u64,
}

impl ScanBudget {
    fn charge_entry(&mut self, name_bytes: usize) -> Result<()> {
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or_else(|| resource_error("entry-count"))?;
        if self.entries > MAX_ENTRIES {
            return Err(resource_error("entry-count"));
        }
        self.name_bytes = self
            .name_bytes
            .checked_add(name_bytes)
            .ok_or_else(|| resource_error("retained-name-bytes"))?;
        if self.name_bytes > MAX_NAME_BYTES {
            return Err(resource_error("retained-name-bytes"));
        }
        Ok(())
    }

    fn check_file_size(&self, bytes: u64) -> Result<()> {
        if bytes > MAX_FILE_BYTES || self.file_bytes.saturating_add(bytes) > MAX_TOTAL_FILE_BYTES {
            return Err(resource_error("retained-file-bytes"));
        }
        Ok(())
    }

    fn charge_file_bytes(&mut self, bytes: u64) -> Result<()> {
        self.file_bytes = self
            .file_bytes
            .checked_add(bytes)
            .ok_or_else(|| resource_error("retained-file-bytes"))?;
        if self.file_bytes > MAX_TOTAL_FILE_BYTES {
            return Err(resource_error("retained-file-bytes"));
        }
        Ok(())
    }

    fn charge_match_work(&mut self, bytes: usize, patterns: usize) -> Result<()> {
        let work = u64::try_from(bytes)
            .ok()
            .and_then(|bytes| {
                u64::try_from(patterns)
                    .ok()
                    .and_then(|patterns| bytes.checked_mul(patterns))
            })
            .ok_or_else(|| resource_error("matching-work"))?;
        self.match_work = self
            .match_work
            .checked_add(work)
            .ok_or_else(|| resource_error("matching-work"))?;
        if self.match_work > MAX_MATCH_WORK {
            return Err(resource_error("matching-work"));
        }
        Ok(())
    }
}

fn secret_found_error() -> anyhow::Error {
    anyhow!(
        "retained run contains credential-bearing data\n  fix: delete the affected retained run and remove the secret-producing output before retrying"
    )
}

fn unsafe_entry_error() -> anyhow::Error {
    anyhow!(
        "retained-run secret scan rejected an unsafe link, mount, or special entry\n  fix: remove unsafe entries from the retained run directory, then retry"
    )
}

fn unsafe_metadata_error() -> anyhow::Error {
    anyhow!(
        "retained-run secret scan rejected unsafe artifact metadata\n  fix: remove hard links, set retained directories to 0700 and files to 0600 under the current user, then retry"
    )
}

fn inspection_error() -> anyhow::Error {
    anyhow!(
        "could not safely inspect every retained-run artifact\n  fix: make the retained run directory stable and readable, then retry"
    )
}

fn changed_tree_error() -> anyhow::Error {
    anyhow!(
        "the retained run changed during its secret scan\n  fix: stop processes writing to the retained run directory, then retry"
    )
}

fn resource_error(limit: &'static str) -> anyhow::Error {
    anyhow!(
        "retained-run secret scan exceeded its {limit} safety limit\n  fix: remove excessive retained artifacts or reduce the sensitive input set, then retry"
    )
}
