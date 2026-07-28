"""Filter-independent raw worktree and index verification."""

from __future__ import annotations

import hashlib
import os
import stat
from pathlib import Path
from typing import Callable


GitOutput = Callable[..., bytes]
Entry = tuple[bytes, bytes]


class WorktreeContractError(ValueError):
    """The raw checkout differs from its committed HEAD tree."""


def _tree_entries(raw: bytes) -> dict[bytes, Entry]:
    entries: dict[bytes, Entry] = {}
    for record in (item for item in raw.split(b"\0") if item):
        try:
            metadata, path = record.split(b"\t", 1)
            mode, kind, object_id = metadata.split()
        except ValueError as error:
            raise WorktreeContractError("Git returned a malformed HEAD tree") from error
        if (
            not path
            or path.startswith(b"/")
            or b"//" in path
            or any(part in {b"", b".", b".."} for part in path.split(b"/"))
            or path.split(b"/", 1)[0] == b".git"
            or kind != b"blob"
            or mode not in {b"100644", b"100755", b"120000"}
            or path in entries
        ):
            raise WorktreeContractError(
                "HEAD must contain only unique regular files and symbolic links"
            )
        entries[path] = (mode, object_id)
    return entries


def _index_entries(raw: bytes) -> dict[bytes, Entry]:
    entries: dict[bytes, Entry] = {}
    for record in (item for item in raw.split(b"\0") if item):
        try:
            metadata, path = record.split(b"\t", 1)
            mode, object_id, stage = metadata.split()
        except ValueError as error:
            raise WorktreeContractError("Git returned a malformed index") from error
        if stage != b"0" or not path or path in entries:
            raise WorktreeContractError(
                "trusted repository index has unmerged or duplicate entries"
            )
        entries[path] = (mode, object_id)
    return entries


def _identity(metadata: os.stat_result) -> tuple[int, ...]:
    return (
        metadata.st_dev,
        metadata.st_ino,
        metadata.st_mode,
        metadata.st_size,
        metadata.st_mtime_ns,
        metadata.st_ctime_ns,
    )


def _blob_digest(algorithm: str, raw: bytes) -> bytes:
    digest = hashlib.new(algorithm)
    digest.update(f"blob {len(raw)}\0".encode("ascii"))
    digest.update(raw)
    return digest.hexdigest().encode("ascii")


def _regular_digest(
    descriptor: int,
    metadata: os.stat_result,
    algorithm: str,
) -> bytes:
    digest = hashlib.new(algorithm)
    digest.update(f"blob {metadata.st_size}\0".encode("ascii"))
    total = 0
    while True:
        chunk = os.read(descriptor, 1024 * 1024)
        if not chunk:
            break
        total += len(chunk)
        digest.update(chunk)
    if total != metadata.st_size:
        raise WorktreeContractError(
            "tracked file changed size while its raw bytes were verified"
        )
    return digest.hexdigest().encode("ascii")


def _parent_descriptor(root_descriptor: int, path: bytes) -> tuple[int, bytes]:
    parts = path.split(b"/")
    descriptor = os.dup(root_descriptor)
    try:
        for component in parts[:-1]:
            child = os.open(
                component,
                os.O_RDONLY | os.O_CLOEXEC | os.O_DIRECTORY | os.O_NOFOLLOW,
                dir_fd=descriptor,
            )
            os.close(descriptor)
            descriptor = child
    except BaseException:
        os.close(descriptor)
        raise
    return descriptor, parts[-1]


def _validate_entry(
    root_descriptor: int,
    path: bytes,
    mode: bytes,
    object_id: bytes,
    algorithm: str,
) -> None:
    parent, name = _parent_descriptor(root_descriptor, path)
    descriptor: int | None = None
    try:
        before = os.stat(name, dir_fd=parent, follow_symlinks=False)
        if mode == b"120000":
            if not stat.S_ISLNK(before.st_mode):
                raise WorktreeContractError(
                    "tracked symbolic-link type differs from HEAD"
                )
            raw = os.readlink(name, dir_fd=parent)
            if not isinstance(raw, bytes):
                raw = os.fsencode(raw)
            digest = _blob_digest(algorithm, raw)
        else:
            descriptor = os.open(
                name,
                os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW | os.O_NONBLOCK,
                dir_fd=parent,
            )
            opened = os.fstat(descriptor)
            expected_executable = mode == b"100755"
            if (
                not stat.S_ISREG(opened.st_mode)
                or _identity(opened) != _identity(before)
                or bool(opened.st_mode & stat.S_IXUSR) != expected_executable
            ):
                raise WorktreeContractError(
                    "tracked file type or executable mode differs from HEAD"
                )
            digest = _regular_digest(descriptor, opened, algorithm)
            if _identity(os.fstat(descriptor)) != _identity(opened):
                raise WorktreeContractError(
                    "tracked file changed while its raw bytes were verified"
                )
        after = os.stat(name, dir_fd=parent, follow_symlinks=False)
        if _identity(after) != _identity(before) or digest != object_id:
            raise WorktreeContractError(
                "tracked raw worktree bytes differ from HEAD"
            )
    except OSError as error:
        raise WorktreeContractError(
            f"cannot verify tracked raw worktree entry: {error}"
        ) from error
    finally:
        if descriptor is not None:
            os.close(descriptor)
        os.close(parent)


def _assert_no_extras(root: Path, tracked: set[bytes]) -> None:
    directories = {
        b"/".join(parts[:index])
        for path in tracked
        for parts in (path.split(b"/"),)
        for index in range(1, len(parts))
    }
    pending = [(root, b"")]
    while pending:
        directory, prefix = pending.pop()
        try:
            entries = list(os.scandir(directory))
        except OSError as error:
            raise WorktreeContractError(
                f"cannot enumerate raw worktree: {error}"
            ) from error
        for entry in entries:
            name = os.fsencode(entry.name)
            if not prefix and name == b".git":
                continue
            relative = name if not prefix else prefix + b"/" + name
            try:
                is_directory = entry.is_dir(follow_symlinks=False)
            except OSError as error:
                raise WorktreeContractError(
                    f"cannot inspect raw worktree entry: {error}"
                ) from error
            if is_directory and relative in directories:
                pending.append((Path(entry.path), relative))
            elif relative not in tracked:
                raise WorktreeContractError(
                    "trusted repository contains an untracked or ignored entry"
                )


def validate_exact_worktree(root: Path, git_output: GitOutput) -> None:
    """Require HEAD, index, raw bytes, modes, links, and topology to agree."""

    tree = _tree_entries(
        git_output("ls-tree", "-r", "-z", "--full-tree", "HEAD")
    )
    index = _index_entries(git_output("ls-files", "--stage", "-z"))
    if index != tree:
        raise WorktreeContractError(
            "trusted repository index differs from the exact HEAD tree"
        )
    object_format = git_output("rev-parse", "--show-object-format").strip()
    if object_format not in {b"sha1", b"sha256"}:
        raise WorktreeContractError("Git returned an unsupported object format")
    root_descriptor = os.open(
        root,
        os.O_RDONLY | os.O_CLOEXEC | os.O_DIRECTORY | os.O_NOFOLLOW,
    )
    try:
        for path, (mode, object_id) in sorted(tree.items()):
            _validate_entry(
                root_descriptor,
                path,
                mode,
                object_id,
                object_format.decode("ascii"),
            )
    finally:
        os.close(root_descriptor)
    _assert_no_extras(root, set(tree))


__all__ = ["WorktreeContractError", "validate_exact_worktree"]
