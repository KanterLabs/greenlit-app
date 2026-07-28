//! Descriptor-relative reads of private retained-run artifacts.

use std::fs::{self, File};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rustix::fs::{CWD, Mode, OFlags, ResolveFlags, openat, openat2};
use rustix::io::Errno;
use serde::de::DeserializeOwned;

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;

pub(crate) struct RunsDirectory {
    path: PathBuf,
    handle: File,
}

pub(crate) struct RetainedRun {
    run_id: String,
    path: PathBuf,
    handle: File,
}

pub(crate) fn open_runs_directory() -> anyhow::Result<RunsDirectory> {
    let path = super::runs_root()?;
    let handle = openat2(
        CWD,
        &path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
        ResolveFlags::NO_MAGICLINKS | ResolveFlags::NO_SYMLINKS,
    )
    .map(File::from)
    .map_err(|error| {
        anyhow::anyhow!(
            "could not open run evidence at {}: {error}\n  fix: run a workflow first, or use `litci doctor` to diagnose unsafe local state",
            path.display()
        )
    })?;
    validate_private_directory(&path, &handle)?;
    Ok(RunsDirectory { path, handle })
}

impl RunsDirectory {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn open_run(&self, run_id: &str) -> anyhow::Result<RetainedRun> {
        let run_id = super::validate_run_id(run_id)?;
        let path = self.path.join(&run_id);
        let handle = openat(
            &self.handle,
            run_id.as_str(),
            OFlags::RDONLY
                | OFlags::DIRECTORY
                | OFlags::CLOEXEC
                | OFlags::NOFOLLOW
                | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(|error| {
            anyhow::anyhow!(
                "run evidence '{run_id}' is unavailable or unsafe: {error}\n  fix: choose another run or use `litci doctor` to diagnose local state"
            )
        })?;
        validate_private_directory(&path, &handle)?;
        Ok(RetainedRun {
            run_id,
            path,
            handle,
        })
    }

    pub(crate) fn run_ids(&self) -> anyhow::Result<Vec<String>> {
        let descriptor = PathBuf::from(format!("/proc/self/fd/{}", self.handle.as_raw_fd()));
        let entries = fs::read_dir(&descriptor).map_err(|error| {
            anyhow::anyhow!(
                "could not list run evidence at {}: {error}\n  fix: use `litci doctor` to diagnose local state",
                self.path.display()
            )
        })?;
        let mut run_ids = entries
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| super::validate_run_id(name).is_ok())
            .collect::<Vec<_>>();
        run_ids.sort();
        Ok(run_ids)
    }
}

impl RetainedRun {
    pub(crate) fn run_id(&self) -> &str {
        &self.run_id
    }

    pub(crate) fn artifact_path(&self, name: &'static str) -> PathBuf {
        self.path.join(name)
    }

    pub(crate) fn open_artifact(&self, name: &'static str) -> anyhow::Result<File> {
        let path = self.artifact_path(name);
        let file = openat(
            &self.handle,
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(|error| {
            anyhow::anyhow!(
                "could not read retained run artifact {}: {error}\n  fix: choose a completed run or use `litci doctor` to diagnose local state",
                path.display()
            )
        })?;
        validate_private_file(&path, &file)?;
        Ok(file)
    }

    pub(crate) fn has_artifact(&self, name: &'static str) -> anyhow::Result<bool> {
        match openat(
            &self.handle,
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(file) => {
                let file = File::from(file);
                validate_private_file(&self.artifact_path(name), &file)?;
                Ok(true)
            }
            Err(Errno::NOENT) => Ok(false),
            Err(error) => Err(anyhow::anyhow!(
                "could not inspect retained run artifact {}: {error}\n  fix: preserve the run directory and use `litci doctor`",
                self.artifact_path(name).display()
            )),
        }
    }

    pub(super) fn read_json<T: DeserializeOwned>(
        &self,
        name: &'static str,
        description: &'static str,
    ) -> anyhow::Result<T> {
        let path = self.artifact_path(name);
        let mut file = self.open_artifact(name)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(|error| {
            anyhow::anyhow!(
                "could not read {description} {}: {error}\n  fix: preserve the run directory and use `litci doctor`",
                path.display()
            )
        })?;
        serde_json::from_slice(&bytes).map_err(|error| {
            anyhow::anyhow!(
                "{description} {} is not valid JSON: {error}\n  fix: preserve the run directory and use `litci doctor`",
                path.display()
            )
        })
    }
}

fn validate_private_directory(path: &Path, directory: &File) -> anyhow::Result<()> {
    let metadata = directory.metadata().map_err(|error| {
        anyhow::anyhow!(
            "could not inspect retained run directory {}: {error}\n  fix: preserve ~/.litci and use `litci doctor`",
            path.display()
        )
    })?;
    let current_uid = rustix::process::getuid().as_raw();
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_dir() || metadata.uid() != current_uid || mode != PRIVATE_DIRECTORY_MODE {
        anyhow::bail!(
            "refused unsafe retained run directory {} (owner uid {}, mode 0{mode:03o})\n  fix: preserve ~/.litci and use `litci doctor`",
            path.display(),
            metadata.uid()
        );
    }
    Ok(())
}

fn validate_private_file(path: &Path, file: &File) -> anyhow::Result<()> {
    let metadata = file.metadata().map_err(|error| {
        anyhow::anyhow!(
            "could not inspect retained run artifact {}: {error}\n  fix: preserve ~/.litci and use `litci doctor`",
            path.display()
        )
    })?;
    let current_uid = rustix::process::getuid().as_raw();
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_file()
        || metadata.uid() != current_uid
        || metadata.nlink() != 1
        || mode != PRIVATE_FILE_MODE
    {
        anyhow::bail!(
            "refused unsafe retained run artifact {} (owner uid {}, mode 0{mode:03o}, links {})\n  fix: preserve ~/.litci and use `litci doctor`",
            path.display(),
            metadata.uid(),
            metadata.nlink()
        );
    }
    Ok(())
}
