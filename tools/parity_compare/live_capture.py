"""Secure binding and exact reads for private live parity captures."""

from __future__ import annotations

import os
import re
import stat
from dataclasses import dataclass
from pathlib import Path

from . import ContractError
from .repository import RepositoryIdentity


CASE_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
ROLES = ("oracle", "github-actions", "greenlit-release")
MAX_CAPTURE_BYTES = 8 * 1024 * 1024
_DirectoryIdentity = tuple[int, int, int, int, int, int, int]
_FileIdentity = tuple[int, int, int, int, int, int, int, int]


class LiveCaptureError(ContractError):
    """A private live-capture filesystem contract was violated."""


@dataclass(frozen=True)
class LiveCaptureRootIdentity:
    """One private capture root and its fixed direct captures directory."""

    root: Path
    captures: Path
    _root_identity: _DirectoryIdentity
    _captures_identity: _DirectoryIdentity


def _directory_identity(path: Path, label: str) -> _DirectoryIdentity:
    try:
        metadata = path.lstat()
    except OSError as error:
        raise LiveCaptureError(f"{label} is not accessible: {error}") from error
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
        raise LiveCaptureError(f"{label} must be a real non-symlink directory")
    if metadata.st_uid != os.geteuid():
        raise LiveCaptureError(f"{label} must be owned by the current uid")
    if stat.S_IMODE(metadata.st_mode) != 0o700:
        raise LiveCaptureError(f"{label} must have exact mode 0700")
    return (
        metadata.st_dev,
        metadata.st_ino,
        stat.S_IFMT(metadata.st_mode),
        stat.S_IMODE(metadata.st_mode),
        metadata.st_uid,
        metadata.st_mtime_ns,
        metadata.st_ctime_ns,
    )


def _assert_disjoint(capture_root: Path, repository_root: Path) -> None:
    if (
        capture_root == repository_root
        or capture_root.is_relative_to(repository_root)
        or repository_root.is_relative_to(capture_root)
    ):
        raise LiveCaptureError(
            "live capture root must be outside and disjoint from the repository"
        )


def _reject_symlink_components(path: Path) -> None:
    current = Path(path.anchor)
    for part in path.parts[1:]:
        current /= part
        try:
            metadata = current.lstat()
        except OSError as error:
            raise LiveCaptureError(
                f"live capture root component is not accessible: {error}"
            ) from error
        if stat.S_ISLNK(metadata.st_mode):
            raise LiveCaptureError(
                f"live capture root contains symlink component {current}"
            )


def bind_live_capture_root(
    capture_root: Path,
    repository: RepositoryIdentity,
) -> LiveCaptureRootIdentity:
    """Bind a private external capture root before loading any observation."""
    if not capture_root.is_absolute():
        raise LiveCaptureError("live capture root must be an absolute path")
    supplied = capture_root
    _reject_symlink_components(supplied)
    _directory_identity(supplied, "live capture root")
    try:
        root = supplied.resolve(strict=True)
    except (OSError, RuntimeError) as error:
        raise LiveCaptureError(
            f"live capture root cannot be resolved: {error}"
        ) from error
    _assert_disjoint(root, repository.root)
    root_identity = _directory_identity(root, "live capture root")
    captures = root / "captures"
    captures_identity = _directory_identity(
        captures, "live capture root's direct captures directory"
    )
    identity = LiveCaptureRootIdentity(
        root=root,
        captures=captures,
        _root_identity=root_identity,
        _captures_identity=captures_identity,
    )
    _assert_directories(identity)
    return identity


def _assert_directories(identity: LiveCaptureRootIdentity) -> None:
    if (
        _directory_identity(identity.root, "live capture root")
        != identity._root_identity
        or _directory_identity(
            identity.captures,
            "live capture root's direct captures directory",
        )
        != identity._captures_identity
    ):
        raise LiveCaptureError(
            "live capture root identity changed during parity validation"
        )


def assert_live_capture_root_unchanged(
    identity: LiveCaptureRootIdentity,
) -> None:
    """Require the private root and captures directory to remain unchanged."""
    _assert_directories(identity)


def assert_live_capture_topology(
    identity: LiveCaptureRootIdentity,
    case_id: str,
) -> None:
    """Require the private live root to contain only its fixed six files."""

    if CASE_ID.fullmatch(case_id) is None:
        raise LiveCaptureError("invalid live capture case identity")
    expected = (
        (
            identity.root,
            identity._root_identity,
            {"captures", *(f"seed-{role}.json" for role in ROLES)},
            "live capture root",
        ),
        (
            identity.captures,
            identity._captures_identity,
            {f"{case_id}-{role}.json" for role in ROLES},
            "live captures directory",
        ),
    )
    for path, directory_identity, names, label in expected:
        descriptor = _open_bound_directory(
            identity,
            path,
            directory_identity,
            label,
        )
        try:
            observed = set(os.listdir(descriptor))
        except OSError as error:
            raise LiveCaptureError(f"cannot enumerate {label}: {error}") from error
        finally:
            os.close(descriptor)
        if observed != names:
            raise LiveCaptureError(
                f"{label} must contain exactly its fixed parity evidence files"
            )
    _assert_directories(identity)


def _open_bound_directory(
    identity: LiveCaptureRootIdentity,
    path: Path,
    expected: _DirectoryIdentity,
    label: str,
) -> int:
    _assert_directories(identity)
    flags = os.O_RDONLY | os.O_CLOEXEC | os.O_DIRECTORY | os.O_NOFOLLOW
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise LiveCaptureError(f"cannot open {label}: {error}") from error
    try:
        metadata = os.fstat(descriptor)
        observed = (
            metadata.st_dev,
            metadata.st_ino,
            stat.S_IFMT(metadata.st_mode),
            stat.S_IMODE(metadata.st_mode),
            metadata.st_uid,
            metadata.st_mtime_ns,
            metadata.st_ctime_ns,
        )
        if observed != expected:
            raise LiveCaptureError(f"{label} changed while it was being opened")
    except BaseException:
        os.close(descriptor)
        raise
    return descriptor


def _file_identity(metadata: os.stat_result, label: str) -> _FileIdentity:
    if not stat.S_ISREG(metadata.st_mode):
        raise LiveCaptureError(f"{label} must be a regular non-symlink file")
    if metadata.st_uid != os.geteuid():
        raise LiveCaptureError(f"{label} must be owned by the current uid")
    if stat.S_IMODE(metadata.st_mode) != 0o600:
        raise LiveCaptureError(f"{label} must have exact mode 0600")
    if metadata.st_nlink != 1:
        raise LiveCaptureError(f"{label} must have exactly one filesystem link")
    if metadata.st_size < 0 or metadata.st_size > MAX_CAPTURE_BYTES:
        raise LiveCaptureError(
            f"{label} exceeds the {MAX_CAPTURE_BYTES}-byte size limit"
        )
    return (
        metadata.st_dev,
        metadata.st_ino,
        metadata.st_size,
        stat.S_IMODE(metadata.st_mode),
        metadata.st_uid,
        metadata.st_nlink,
        metadata.st_mtime_ns,
        metadata.st_ctime_ns,
    )


def _read_descriptor(descriptor: int, expected_size: int, label: str) -> bytes:
    chunks: list[bytes] = []
    total = 0
    while True:
        try:
            chunk = os.read(descriptor, min(65536, MAX_CAPTURE_BYTES + 1 - total))
        except OSError as error:
            raise LiveCaptureError(f"cannot read {label}: {error}") from error
        if not chunk:
            break
        chunks.append(chunk)
        total += len(chunk)
        if total > MAX_CAPTURE_BYTES:
            raise LiveCaptureError(
                f"{label} exceeds the {MAX_CAPTURE_BYTES}-byte size limit"
            )
    if total != expected_size:
        raise LiveCaptureError(f"{label} changed size while it was being read")
    return b"".join(chunks)


def _read_bound_file(
    identity: LiveCaptureRootIdentity,
    directory_path: Path,
    directory_identity: _DirectoryIdentity,
    filename: str,
    label: str,
) -> bytes:
    directory = _open_bound_directory(
        identity,
        directory_path,
        directory_identity,
        f"{label} directory",
    )
    descriptor: int | None = None
    try:
        flags = os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW | os.O_NONBLOCK
        try:
            descriptor = os.open(filename, flags, dir_fd=directory)
        except OSError as error:
            raise LiveCaptureError(f"cannot open {label}: {error}") from error
        before = _file_identity(os.fstat(descriptor), label)
        raw = _read_descriptor(descriptor, before[2], label)
        after = _file_identity(os.fstat(descriptor), label)
        if after != before:
            raise LiveCaptureError(f"{label} changed while it was being read")
        path_metadata = os.stat(filename, dir_fd=directory, follow_symlinks=False)
        if _file_identity(path_metadata, label) != before:
            raise LiveCaptureError(f"{label} path changed while it was being read")
    except OSError as error:
        raise LiveCaptureError(f"cannot verify {label}: {error}") from error
    finally:
        if descriptor is not None:
            os.close(descriptor)
        os.close(directory)
    _assert_directories(identity)
    return raw


def read_live_capture(
    identity: LiveCaptureRootIdentity,
    case_id: str,
    role: str,
) -> bytes:
    """Read exact stable bytes from captures/<case>-<role>.json without links."""
    if CASE_ID.fullmatch(case_id) is None:
        raise LiveCaptureError("invalid live capture case identity")
    if role not in ROLES:
        raise LiveCaptureError("invalid live capture producer role")
    filename = f"{case_id}-{role}.json"
    return _read_bound_file(
        identity,
        identity.captures,
        identity._captures_identity,
        filename,
        f"live capture {filename!r}",
    )


def read_live_observation(
    identity: LiveCaptureRootIdentity,
    path: Path,
    role: str,
) -> bytes:
    """Read the exact role-bound capture-root observation positional."""
    if role not in ROLES:
        raise LiveCaptureError("invalid live observation producer role")
    expected = identity.root / f"seed-{role}.json"
    supplied = Path(os.path.abspath(path))
    if supplied != expected:
        raise LiveCaptureError(
            f"{role} observation path must be exactly {expected}"
        )
    return _read_bound_file(
        identity,
        identity.root,
        identity._root_identity,
        expected.name,
        f"live observation {expected.name!r}",
    )


__all__ = [
    "LiveCaptureError",
    "LiveCaptureRootIdentity",
    "assert_live_capture_topology",
    "assert_live_capture_root_unchanged",
    "bind_live_capture_root",
    "read_live_capture",
    "read_live_observation",
]
