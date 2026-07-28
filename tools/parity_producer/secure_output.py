"""Descriptor-relative atomic publication beneath a trusted checkout."""

from __future__ import annotations

import errno
import os
import secrets
import stat
from pathlib import Path, PurePosixPath

from parity_producer.common import ProducerError


def read_bytes_beneath(
    checkout: Path,
    relative: Path,
    limit: int,
    source: str,
) -> bytes:
    """Read one bounded regular file without following any path symlink."""
    parts = PurePosixPath(relative.as_posix()).parts
    if not parts or any(part in {"", ".", ".."} for part in parts):
        raise ProducerError(f"unsafe parity input path {relative}")
    parent_descriptor = _open_parent(checkout, parts[:-1], create_leaf=False)
    flags = os.O_RDONLY | os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    descriptor = -1
    try:
        descriptor = os.open(parts[-1], flags, dir_fd=parent_descriptor)
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode):
            raise ProducerError(f"{source} is not a regular file")
        if metadata.st_size > limit:
            raise ProducerError(f"{source} exceeds the {limit}-byte safety limit")
        with os.fdopen(descriptor, "rb", closefd=True) as handle:
            descriptor = -1
            raw = handle.read(limit + 1)
    except OSError as error:
        raise ProducerError(f"cannot read {source}: {error}") from error
    finally:
        if descriptor >= 0:
            os.close(descriptor)
        os.close(parent_descriptor)
    if len(raw) > limit:
        raise ProducerError(f"{source} exceeds the {limit}-byte safety limit")
    return raw


def write_bytes_beneath(
    checkout: Path,
    relative: Path,
    raw: bytes,
    *,
    create_parent_leaf: bool,
    created_directory_mode: int = 0o755,
) -> Path:
    """Atomically write owner-private bytes without following parent symlinks."""
    parts = PurePosixPath(relative.as_posix()).parts
    if not parts or any(part in {"", ".", ".."} for part in parts):
        raise ProducerError(f"unsafe parity output path {relative}")
    parent_descriptor = _open_parent(
        checkout,
        parts[:-1],
        create_leaf=create_parent_leaf,
        created_directory_mode=created_directory_mode,
    )
    name = parts[-1]
    temporary = f".{name}.{secrets.token_hex(12)}"
    descriptor = -1
    published = False
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(
            temporary,
            flags,
            0o600,
            dir_fd=parent_descriptor,
        )
        os.fchmod(descriptor, 0o600)
        with os.fdopen(descriptor, "wb", closefd=True) as handle:
            descriptor = -1
            handle.write(raw)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(
            temporary,
            name,
            src_dir_fd=parent_descriptor,
            dst_dir_fd=parent_descriptor,
        )
        published = True
        os.fsync(parent_descriptor)
    except OSError as error:
        raise ProducerError(f"cannot publish parity output {relative}: {error}") from error
    finally:
        if descriptor >= 0:
            os.close(descriptor)
        if not published:
            try:
                os.unlink(temporary, dir_fd=parent_descriptor)
            except FileNotFoundError:
                pass
        os.close(parent_descriptor)
    return checkout / relative


def _open_parent(
    checkout: Path,
    parts: tuple[str, ...],
    *,
    create_leaf: bool,
    created_directory_mode: int = 0o755,
) -> int:
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = _open_absolute_directory(checkout, flags)
    except OSError as error:
        raise ProducerError(f"cannot open parity checkout {checkout}: {error}") from error
    try:
        for index, part in enumerate(parts):
            try:
                child = os.open(part, flags, dir_fd=descriptor)
            except OSError as error:
                may_create = (
                    error.errno == errno.ENOENT
                    and create_leaf
                    and index == len(parts) - 1
                )
                if not may_create:
                    raise
                os.mkdir(part, mode=created_directory_mode, dir_fd=descriptor)
                child = os.open(part, flags, dir_fd=descriptor)
                os.fchmod(child, created_directory_mode)
            os.close(descriptor)
            descriptor = child
        return descriptor
    except OSError as error:
        os.close(descriptor)
        raise ProducerError(
            "parity output parent is missing, unsafe, or not a directory"
        ) from error


def _open_absolute_directory(path: Path, flags: int) -> int:
    if not path.is_absolute():
        raise ProducerError(f"parity root must be absolute: {path}")
    try:
        descriptor = os.open(path.anchor, flags)
        for part in path.parts[1:]:
            child = os.open(part, flags, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = child
        return descriptor
    except OSError as error:
        if "descriptor" in locals():
            os.close(descriptor)
        raise ProducerError(
            f"parity root contains a missing or unsafe component: {path}"
        ) from error
