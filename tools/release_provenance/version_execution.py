"""Descriptor-bound execution of the trusted release binary version probe."""

from __future__ import annotations

from contextlib import contextmanager
import os
import secrets
import signal
import stat
import subprocess
from collections.abc import Iterator
from dataclasses import dataclass
from pathlib import Path

from .common import ProvenanceError, open_regular


OUTPUT_LIMIT = 4096
TIMEOUT_SECONDS = 15
WORK_PREFIX = ".greenlit-provenance-version-"


@dataclass(frozen=True)
class VersionWorkspace:
    """Opened private paths used by exactly one version execution."""

    home: int
    stdout: int
    stderr: int


def _directory_flags() -> int:
    required = ("O_CLOEXEC", "O_DIRECTORY", "O_NOFOLLOW")
    if any(not hasattr(os, name) for name in required):
        raise ProvenanceError(
            "release provenance requires Linux no-follow directory descriptors"
        )
    return os.O_RDONLY | os.O_CLOEXEC | os.O_DIRECTORY | os.O_NOFOLLOW


def _open_runner_temp() -> int:
    raw = os.environ.get("RUNNER_TEMP")
    if (
        not raw
        or not raw.startswith("/")
        or raw.startswith("//")
        or os.path.normpath(raw) != raw
        or "\0" in raw
    ):
        raise ProvenanceError(
            "RUNNER_TEMP must name one canonical absolute private directory"
        )
    flags = _directory_flags()
    descriptor = os.open("/", flags)
    try:
        for component in Path(raw).parts[1:]:
            next_descriptor = os.open(component, flags, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = next_descriptor
    except OSError as error:
        os.close(descriptor)
        raise ProvenanceError(
            f"RUNNER_TEMP is not one descriptor-bound real directory: {error}"
        ) from error
    metadata = os.fstat(descriptor)
    if (
        not stat.S_ISDIR(metadata.st_mode)
        or stat.S_IMODE(metadata.st_mode) != 0o700
        or metadata.st_uid != os.geteuid()
    ):
        os.close(descriptor)
        raise ProvenanceError(
            "RUNNER_TEMP must be an owned mode-0700 real directory"
        )
    return descriptor


def _open_directory_at(parent: int, name: str, mode: int) -> int:
    descriptor = os.open(name, _directory_flags(), dir_fd=parent)
    metadata = os.fstat(descriptor)
    if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != os.geteuid():
        os.close(descriptor)
        raise ProvenanceError("release version workspace directory is unsafe")
    os.fchmod(descriptor, mode)
    return descriptor


def _create_file_at(parent: int, name: str) -> int:
    flags = os.O_CREAT | os.O_EXCL | os.O_RDWR | os.O_CLOEXEC | os.O_NOFOLLOW
    descriptor = os.open(name, flags, 0o600, dir_fd=parent)
    metadata = os.fstat(descriptor)
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.geteuid():
        os.close(descriptor)
        raise ProvenanceError("release version output file is unsafe")
    os.fchmod(descriptor, 0o600)
    return descriptor


def _empty_directory(descriptor: int) -> None:
    with os.scandir(descriptor) as entries:
        names = [entry.name for entry in entries]
    for name in names:
        metadata = os.stat(name, dir_fd=descriptor, follow_symlinks=False)
        if stat.S_ISDIR(metadata.st_mode):
            child = _open_directory_at(
                descriptor,
                name,
                stat.S_IMODE(metadata.st_mode),
            )
            identity = (metadata.st_dev, metadata.st_ino)
            try:
                current = os.fstat(child)
                if (current.st_dev, current.st_ino) != identity:
                    raise ProvenanceError(
                        "release version cleanup directory identity changed"
                    )
                _empty_directory(child)
            finally:
                os.close(child)
            current = os.stat(name, dir_fd=descriptor, follow_symlinks=False)
            if (current.st_dev, current.st_ino) != identity:
                raise ProvenanceError(
                    "release version cleanup directory identity changed"
                )
            os.rmdir(name, dir_fd=descriptor)
        else:
            os.unlink(name, dir_fd=descriptor)
    with os.scandir(descriptor) as entries:
        if next(entries, None) is not None:
            raise ProvenanceError("release version workspace cleanup was incomplete")


def _remove_workspace(parent: int, name: str, descriptor: int) -> None:
    identity = os.fstat(descriptor)
    _empty_directory(descriptor)
    current = os.stat(name, dir_fd=parent, follow_symlinks=False)
    if (current.st_dev, current.st_ino) != (identity.st_dev, identity.st_ino):
        raise ProvenanceError("release version workspace identity changed")
    os.rmdir(name, dir_fd=parent)
    try:
        os.stat(name, dir_fd=parent, follow_symlinks=False)
    except FileNotFoundError:
        return
    raise ProvenanceError("release version workspace cleanup was incomplete")


@contextmanager
def _workspace() -> Iterator[VersionWorkspace]:
    parent = _open_runner_temp()
    name = ""
    work = -1
    home = -1
    stdout = -1
    stderr = -1
    try:
        for _ in range(64):
            candidate = WORK_PREFIX + secrets.token_hex(16)
            try:
                os.mkdir(candidate, 0o700, dir_fd=parent)
            except FileExistsError:
                continue
            name = candidate
            break
        if not name:
            raise ProvenanceError("could not allocate a unique release version workspace")
        work = _open_directory_at(parent, name, 0o700)
        os.mkdir("home", 0o700, dir_fd=work)
        home = _open_directory_at(work, "home", 0o700)
        stdout = _create_file_at(work, "stdout")
        stderr = _create_file_at(work, "stderr")
        yield VersionWorkspace(home=home, stdout=stdout, stderr=stderr)
    except OSError as error:
        raise ProvenanceError(
            f"could not use the private release version workspace: {error}"
        ) from error
    finally:
        for descriptor in (stderr, stdout, home):
            if descriptor >= 0:
                os.close(descriptor)
        try:
            if name and work >= 0:
                _remove_workspace(parent, name, work)
            elif name:
                os.rmdir(name, dir_fd=parent)
        except OSError as error:
            raise ProvenanceError(
                f"could not clean the private release version workspace: {error}"
            ) from error
        finally:
            if work >= 0:
                os.close(work)
            os.close(parent)


def _terminate_group(process: subprocess.Popen[bytes]) -> None:
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    if process.poll() is None:
        process.wait()


def _read_output(descriptor: int) -> bytes:
    size = os.fstat(descriptor).st_size
    if size > OUTPUT_LIMIT:
        raise ProvenanceError("release binary version output is oversized")
    os.lseek(descriptor, 0, os.SEEK_SET)
    chunks = []
    remaining = OUTPUT_LIMIT + 1
    while remaining:
        chunk = os.read(descriptor, remaining)
        if not chunk:
            break
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def run_trusted_version(path: Path, expected: bytes) -> None:
    """Run one pinned trusted binary with all writable paths privately confined."""

    with _workspace() as workspace, open_regular(
        path, "trusted release binary"
    ) as binary:
        home = f"/proc/self/fd/{workspace.home}"
        executable = f"/proc/self/fd/{binary.fileno()}"
        environment = {
            "HOME": home,
            "LANG": "C",
            "LC_ALL": "C",
            "PATH": "/usr/bin:/bin",
            "RUNNER_TEMP": home,
            "TEMP": home,
            "TMP": home,
            "TMPDIR": home,
        }
        try:
            process = subprocess.Popen(
                [os.fspath(path), "--version"],
                executable=executable,
                cwd=home,
                env=environment,
                stdin=subprocess.DEVNULL,
                stdout=workspace.stdout,
                stderr=workspace.stderr,
                pass_fds=(binary.fileno(), workspace.home),
                start_new_session=True,
            )
            try:
                returncode = process.wait(timeout=TIMEOUT_SECONDS)
            except subprocess.TimeoutExpired as error:
                _terminate_group(process)
                raise ProvenanceError(
                    "trusted release binary --version timed out"
                ) from error
            _terminate_group(process)
        except OSError as error:
            raise ProvenanceError(
                f"could not execute trusted release binary: {error}"
            ) from error
        actual_stdout = _read_output(workspace.stdout)
        actual_stderr = _read_output(workspace.stderr)
        if returncode != 0:
            raise ProvenanceError(
                f"trusted release binary --version exited with status {returncode}"
            )
        if actual_stdout != expected or actual_stderr:
            raise ProvenanceError("trusted release binary version output is not exact")
