#!/usr/bin/env python3
"""Race-resistant regular-file I/O for release transfer bundles."""

from __future__ import annotations

from contextlib import contextmanager
from dataclasses import dataclass
import hashlib
import os
import stat
from pathlib import Path
from typing import BinaryIO, Iterator


MAX_FILE_BYTES = 512 * 1024 * 1024
MAX_BUNDLE_BYTES = 2 * 1024 * 1024 * 1024


class BundleError(Exception):
    """A malformed or identity-mismatched split-job release bundle."""


@dataclass(frozen=True)
class BoundDirectory:
    """One no-follow directory descriptor and its bound member inventory."""

    descriptor: int
    names: frozenset[str]
    label: str


def _identity(metadata: os.stat_result) -> tuple[int, ...]:
    return (
        metadata.st_dev,
        metadata.st_ino,
        metadata.st_mode,
        metadata.st_uid,
        metadata.st_gid,
        metadata.st_nlink,
        metadata.st_size,
        metadata.st_mtime_ns,
        metadata.st_ctime_ns,
    )


def _flags(*, directory: bool) -> int:
    no_follow = getattr(os, "O_NOFOLLOW", None)
    if no_follow is None:
        raise BundleError("release transfer requires O_NOFOLLOW")
    flags = os.O_RDONLY | os.O_CLOEXEC | os.O_NONBLOCK | no_follow
    if directory:
        directory_flag = getattr(os, "O_DIRECTORY", None)
        if directory_flag is None:
            raise BundleError("release transfer requires O_DIRECTORY")
        flags |= directory_flag
    return flags


def _component(name: str) -> None:
    if name in ("", ".", "..") or "/" in name or "\0" in name:
        raise BundleError("release transfer member path is invalid")


def _open_path(path: str | Path, flags: int) -> int:
    candidate = Path(path)
    if not candidate.is_absolute() or any(
        part in (".", "..") for part in candidate.parts[1:]
    ):
        raise BundleError("release transfer paths must be absolute and normalized")
    if candidate == Path("/"):
        return os.open("/", flags)
    parent = os.open("/", _flags(directory=True))
    try:
        for component in candidate.parts[1:-1]:
            next_parent = os.open(
                component,
                _flags(directory=True),
                dir_fd=parent,
            )
            os.close(parent)
            parent = next_parent
        return os.open(candidate.name, flags, dir_fd=parent)
    finally:
        os.close(parent)


@contextmanager
def reader(
    path: Path,
    mode: int,
    limit: int,
) -> Iterator[tuple[BinaryIO, os.stat_result]]:
    """Open one no-follow file and reject identity changes across its use."""

    with _reader(path, mode, limit, None, str(path)) as opened:
        yield opened


@contextmanager
def child_reader(
    parent: BoundDirectory,
    name: str,
    mode: int,
    limit: int,
) -> Iterator[tuple[BinaryIO, os.stat_result]]:
    """Open one file relative to an already-bound parent descriptor."""

    _component(name)
    with _reader(
        name,
        mode,
        limit,
        parent.descriptor,
        f"{parent.label}/{name}",
    ) as opened:
        yield opened


@contextmanager
def _reader(
    path: str | Path,
    mode: int,
    limit: int,
    directory_descriptor: int | None,
    label: str,
) -> Iterator[tuple[BinaryIO, os.stat_result]]:
    try:
        if directory_descriptor is None:
            descriptor = _open_path(path, _flags(directory=False))
        else:
            descriptor = os.open(
                path,
                _flags(directory=False),
                dir_fd=directory_descriptor,
            )
    except OSError as error:
        raise BundleError(f"cannot open release transfer input: {error}") from error
    stream = os.fdopen(descriptor, "rb", closefd=True)
    before: os.stat_result | None = None
    try:
        before = os.fstat(descriptor)
        if (
            not stat.S_ISREG(before.st_mode)
            or stat.S_IMODE(before.st_mode) != mode
            or before.st_uid != os.geteuid()
            or before.st_gid != os.getegid()
            or before.st_nlink != 1
            or before.st_size > limit
        ):
            raise BundleError(
                f"release transfer input has invalid identity or mode: {label}"
            )
        yield stream, before
    except OSError as error:
        raise BundleError(f"cannot read release transfer input: {error}") from error
    finally:
        try:
            if before is not None and _identity(os.fstat(descriptor)) != _identity(before):
                raise BundleError(
                    f"release transfer input changed while read: {label}"
                )
        except OSError as error:
            raise BundleError(
                f"cannot recheck release transfer input: {error}"
            ) from error
        finally:
            stream.close()


def hash_stream(stream: BinaryIO) -> str:
    """Hash one bounded bundle stream from its current position."""

    digest = hashlib.sha256()
    total = 0
    while chunk := stream.read(1024 * 1024):
        total += len(chunk)
        if total > MAX_BUNDLE_BYTES:
            raise BundleError("release transfer bundle is oversized")
        digest.update(chunk)
    return digest.hexdigest()


def sha256(path: Path) -> str:
    """Hash one exact no-follow mode-0600 bundle."""

    with reader(path, 0o600, MAX_BUNDLE_BYTES) as (stream, _):
        return hash_stream(stream)


@contextmanager
def directory_reader(path: Path, mode: int) -> Iterator[BoundDirectory]:
    """Bind and enumerate one no-follow directory across caller use."""

    with _directory(path, mode, None, str(path)) as opened:
        yield opened


@contextmanager
def child_directory(
    parent: BoundDirectory,
    name: str,
    mode: int,
) -> Iterator[BoundDirectory]:
    """Bind one child relative to an already-bound parent descriptor."""

    _component(name)
    with _directory(
        name,
        mode,
        parent.descriptor,
        f"{parent.label}/{name}",
    ) as opened:
        yield opened


@contextmanager
def _directory(
    path: str | Path,
    mode: int,
    directory_descriptor: int | None,
    label: str,
) -> Iterator[BoundDirectory]:
    before: os.stat_result | None = None
    try:
        if directory_descriptor is None:
            descriptor = _open_path(path, _flags(directory=True))
        else:
            descriptor = os.open(
                path,
                _flags(directory=True),
                dir_fd=directory_descriptor,
            )
        try:
            before = os.fstat(descriptor)
            if (
                not stat.S_ISDIR(before.st_mode)
                or stat.S_IMODE(before.st_mode) != mode
                or before.st_uid != os.geteuid()
                or before.st_gid != os.getegid()
            ):
                raise BundleError(
                    f"release transfer directory has invalid mode: {label}"
                )
            names = frozenset(os.listdir(descriptor))
            yield BoundDirectory(descriptor, names, label)
        finally:
            try:
                if before is not None and _identity(os.fstat(descriptor)) != _identity(
                    before
                ):
                    raise BundleError(
                        f"release transfer directory changed while read: {label}"
                    )
            finally:
                os.close(descriptor)
    except OSError as error:
        raise BundleError(f"cannot inspect release transfer directory: {error}") from error
