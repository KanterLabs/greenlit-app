"""HEAD, index, and working-tree byte/mode comparison."""

from __future__ import annotations

import hashlib
import os
import stat
from dataclasses import dataclass
from pathlib import Path

from .errors import GateError


@dataclass(frozen=True)
class TreeEntry:
    """One immutable HEAD tree entry."""

    mode: bytes
    object_id: bytes


def parse_tree(raw: bytes) -> dict[bytes, TreeEntry]:
    """Parse one NUL-delimited recursive HEAD inventory."""

    result: dict[bytes, TreeEntry] = {}
    for record in raw.split(b"\0"):
        if not record:
            continue
        header, separator, path = record.partition(b"\t")
        fields = header.split(b" ")
        if (
            separator != b"\t"
            or len(fields) != 3
            or not path
            or path in result
            or fields[1] not in {b"blob", b"commit"}
        ):
            raise GateError("Git HEAD tree inventory is malformed or duplicated")
        mode, _, object_id = fields
        if mode not in {b"100644", b"100755", b"120000", b"160000"}:
            raise GateError(f"Git HEAD has unsupported mode at {path!r}")
        result[path] = TreeEntry(mode, object_id)
    return result


def parse_index(raw: bytes) -> dict[bytes, TreeEntry]:
    """Parse one index inventory and reject hidden state flags."""

    result: dict[bytes, TreeEntry] = {}
    for record in raw.split(b"\0"):
        if not record:
            continue
        header, separator, path = record.partition(b"\t")
        fields = header.split(b" ")
        if separator != b"\t" or len(fields) != 4 or not path:
            raise GateError("Git index inventory is malformed")
        tag, mode, object_id, stage = fields
        if tag != b"H":
            raise GateError(
                f"Git index has skip-worktree, assume-unchanged, or noncanonical "
                f"state at {path!r}"
            )
        if stage != b"0" or path in result:
            raise GateError("Git index contains an unmerged or duplicate tracked path")
        result[path] = TreeEntry(mode, object_id)
    return result


def _blob_digest(path: bytes, size: int) -> str:
    digest = hashlib.sha1()
    digest.update(f"blob {size}\0".encode("ascii"))
    flags = os.O_RDONLY | os.O_CLOEXEC | getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(path, flags)
    try:
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode) or before.st_size != size:
            raise GateError(f"tracked file is not stable and regular: {path!r}")
        while True:
            chunk = os.read(descriptor, 1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
        after = os.fstat(descriptor)
    finally:
        os.close(descriptor)
    if (
        before.st_dev,
        before.st_ino,
        before.st_size,
        before.st_mode,
        before.st_mtime_ns,
        before.st_ctime_ns,
    ) != (
        after.st_dev,
        after.st_ino,
        after.st_size,
        after.st_mode,
        after.st_mtime_ns,
        after.st_ctime_ns,
    ):
        raise GateError(f"tracked file changed while it was hashed: {path!r}")
    return digest.hexdigest()


def verify_worktree(repository: Path, entries: dict[bytes, TreeEntry]) -> None:
    """Compare every tracked byte/mode and reject every extra path."""

    repository_bytes = os.fsencode(repository)
    expected_directories: set[bytes] = set()
    for path in entries:
        parts = path.split(b"/")
        if (
            any(part in {b"", b".", b".."} for part in parts)
            or parts[0] == b".git"
        ):
            raise GateError(f"Git HEAD contains an unsafe path {path!r}")
        for index in range(1, len(parts)):
            expected_directories.add(b"/".join(parts[:index]))
    for relative, entry in entries.items():
        full_path = os.path.join(repository_bytes, relative)
        try:
            metadata = os.lstat(full_path)
        except OSError as error:
            raise GateError(f"tracked path is missing: {relative!r}: {error}") from error
        if entry.mode in {b"100644", b"100755"}:
            expected_mode = 0o644 if entry.mode == b"100644" else 0o755
            if (
                not stat.S_ISREG(metadata.st_mode)
                or stat.S_IMODE(metadata.st_mode) != expected_mode
                or metadata.st_nlink != 1
            ):
                raise GateError(f"tracked file mode or type differs from HEAD: {relative!r}")
            try:
                digest = _blob_digest(full_path, metadata.st_size)
            except OSError as error:
                raise GateError(f"cannot hash tracked file {relative!r}: {error}") from error
            if digest.encode("ascii") != entry.object_id:
                raise GateError(f"tracked file bytes differ from HEAD: {relative!r}")
        elif entry.mode == b"120000":
            if not stat.S_ISLNK(metadata.st_mode):
                raise GateError(f"tracked symlink type differs from HEAD: {relative!r}")
            try:
                target = os.readlink(full_path)
            except OSError as error:
                raise GateError(f"cannot read tracked symlink {relative!r}: {error}") from error
            target_bytes = os.fsencode(target)
            digest = hashlib.sha1(
                b"blob " + str(len(target_bytes)).encode("ascii") + b"\0" + target_bytes
            ).hexdigest()
            if digest.encode("ascii") != entry.object_id:
                raise GateError(f"tracked symlink bytes differ from HEAD: {relative!r}")
        else:
            raise GateError(f"live parity does not accept tracked gitlinks: {relative!r}")

    pending = [(repository_bytes, b"")]
    while pending:
        directory, prefix = pending.pop()
        try:
            children = list(os.scandir(directory))
        except OSError as error:
            raise GateError(f"cannot enumerate live parity source: {error}") from error
        for child in children:
            name = os.fsencode(child.name)
            relative = name if not prefix else prefix + b"/" + name
            if not prefix and relative == b".git":
                if not child.is_dir(follow_symlinks=False):
                    raise GateError("repository .git entry is not a direct directory")
                continue
            if relative in entries:
                continue
            if relative not in expected_directories:
                raise GateError(f"live parity source contains untracked path {relative!r}")
            if not child.is_dir(follow_symlinks=False):
                raise GateError(f"tracked directory path is not a real directory: {relative!r}")
            pending.append((os.path.join(directory, name), relative))
