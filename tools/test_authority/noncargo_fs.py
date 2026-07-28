"""Race-resistant repository source reads for non-Cargo authority."""

from __future__ import annotations

import os
import stat
from pathlib import Path, PurePosixPath

from .model import GateError


READ_CHUNK = 64 * 1024
DIRECTORY_FLAGS = os.O_RDONLY | os.O_CLOEXEC | os.O_DIRECTORY | os.O_NOFOLLOW
FILE_FLAGS = os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW


def _identity(metadata: os.stat_result) -> tuple[int, ...]:
    """Return fields that must remain fixed throughout one authority read."""

    return (
        metadata.st_dev,
        metadata.st_ino,
        metadata.st_mode,
        metadata.st_nlink,
        metadata.st_uid,
        metadata.st_gid,
        metadata.st_size,
        metadata.st_mtime_ns,
        metadata.st_ctime_ns,
    )


def _display(root: Path, relative: str) -> Path:
    return root / relative


class SourceTree:
    """An opened repository tree traversed only with no-follow directory FDs."""

    def __init__(self, root: Path) -> None:
        self.root = Path(os.path.abspath(root))
        self._root_fd = self._open_absolute_directory(self.root)
        self._root_identity = _identity(os.fstat(self._root_fd))

    @staticmethod
    def _open_absolute_directory(path: Path) -> int:
        if not path.is_absolute():
            raise GateError(f"{path}: repository root must be absolute")
        descriptor = os.open("/", DIRECTORY_FLAGS)
        try:
            for part in path.parts[1:]:
                try:
                    child = os.open(part, DIRECTORY_FLAGS, dir_fd=descriptor)
                except OSError as error:
                    raise GateError(
                        f"{path}: repository path contains a link or "
                        f"non-directory component: {error}"
                    ) from error
                metadata = os.fstat(child)
                if not stat.S_ISDIR(metadata.st_mode):
                    os.close(child)
                    raise GateError(f"{path}: repository root must be a real directory")
                os.close(descriptor)
                descriptor = child
            return descriptor
        except BaseException:
            os.close(descriptor)
            raise

    def close(self) -> None:
        if self._root_fd >= 0:
            os.close(self._root_fd)
            self._root_fd = -1

    def __enter__(self) -> SourceTree:
        return self

    def __exit__(self, *_unused: object) -> None:
        self.close()

    def _check_root(self) -> None:
        if self._root_fd < 0:
            raise GateError("reviewed source tree is already closed")
        if _identity(os.fstat(self._root_fd)) != self._root_identity:
            raise GateError(f"{self.root}: repository root changed during review")

    def open_directory(self, relative: str) -> int:
        """Open a canonical relative directory without following any component."""

        self._check_root()
        descriptor = os.dup(self._root_fd)
        traversed: list[str] = []
        try:
            for part in PurePosixPath(relative).parts:
                traversed.append(part)
                try:
                    before = os.stat(part, dir_fd=descriptor, follow_symlinks=False)
                except OSError as error:
                    raise GateError(
                        f"{_display(self.root, '/'.join(traversed))}: "
                        f"could not inspect reviewed directory: {error}"
                    ) from error
                if stat.S_ISLNK(before.st_mode) or not stat.S_ISDIR(before.st_mode):
                    raise GateError(
                        f"{_display(self.root, '/'.join(traversed))}: "
                        "reviewed source parent must be a real directory"
                    )
                try:
                    child = os.open(part, DIRECTORY_FLAGS, dir_fd=descriptor)
                except OSError as error:
                    raise GateError(
                        f"{_display(self.root, '/'.join(traversed))}: "
                        f"could not open reviewed directory: {error}"
                    ) from error
                if _identity(before) != _identity(os.fstat(child)):
                    os.close(child)
                    raise GateError(
                        f"{_display(self.root, '/'.join(traversed))}: "
                        "reviewed directory changed while it was opened"
                    )
                os.close(descriptor)
                descriptor = child
            return descriptor
        except BaseException:
            os.close(descriptor)
            raise

    def lstat(self, relative: str) -> os.stat_result | None:
        """Inspect one leaf without following it, returning None when absent."""

        path = PurePosixPath(relative)
        self._check_root()
        descriptor = os.dup(self._root_fd)
        try:
            traversed: list[str] = []
            for part in path.parts[:-1]:
                traversed.append(part)
                try:
                    metadata = os.stat(
                        part,
                        dir_fd=descriptor,
                        follow_symlinks=False,
                    )
                except FileNotFoundError:
                    return None
                except OSError as error:
                    raise GateError(
                        f"{_display(self.root, '/'.join(traversed))}: "
                        f"could not inspect reviewed directory: {error}"
                    ) from error
                if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(
                    metadata.st_mode
                ):
                    raise GateError(
                        f"{_display(self.root, '/'.join(traversed))}: local "
                        "Python candidate parent is not a real directory"
                    )
                try:
                    child = os.open(part, DIRECTORY_FLAGS, dir_fd=descriptor)
                except OSError as error:
                    raise GateError(
                        f"{_display(self.root, '/'.join(traversed))}: "
                        f"could not open reviewed directory: {error}"
                    ) from error
                if _identity(metadata) != _identity(os.fstat(child)):
                    os.close(child)
                    raise GateError(
                        f"{_display(self.root, '/'.join(traversed))}: "
                        "reviewed directory changed while it was opened"
                    )
                os.close(descriptor)
                descriptor = child
            try:
                return os.stat(
                    path.name,
                    dir_fd=descriptor,
                    follow_symlinks=False,
                )
            except FileNotFoundError:
                return None
            except OSError as error:
                raise GateError(
                    f"{_display(self.root, relative)}: "
                    f"could not inspect reviewed source: {error}"
                ) from error
        finally:
            os.close(descriptor)

    def read_regular_snapshot(
        self,
        relative: str,
        limit: int,
    ) -> tuple[bytes, tuple[int, ...]]:
        """Read a bounded regular leaf and retain its exact stable identity."""

        path = PurePosixPath(relative)
        parent = "." if len(path.parts) == 1 else str(path.parent)
        directory = (
            os.dup(self._root_fd)
            if parent == "."
            else self.open_directory(parent)
        )
        descriptor = -1
        try:
            try:
                before = os.stat(
                    path.name,
                    dir_fd=directory,
                    follow_symlinks=False,
                )
            except OSError as error:
                raise GateError(
                    f"{_display(self.root, relative)}: "
                    f"could not inspect reviewed source: {error}"
                ) from error
            if (
                stat.S_ISLNK(before.st_mode)
                or not stat.S_ISREG(before.st_mode)
                or before.st_nlink != 1
            ):
                raise GateError(
                    f"{_display(self.root, relative)}: reviewed source must be "
                    "one regular non-symlink file"
                )
            try:
                descriptor = os.open(
                    path.name,
                    FILE_FLAGS,
                    dir_fd=directory,
                )
            except OSError as error:
                raise GateError(
                    f"{_display(self.root, relative)}: "
                    f"could not open reviewed source: {error}"
                ) from error
            opened = os.fstat(descriptor)
            if _identity(before) != _identity(opened):
                raise GateError(
                    f"{_display(self.root, relative)}: "
                    "reviewed source changed while it was opened"
                )
            chunks: list[bytes] = []
            retained = 0
            while retained <= limit:
                chunk = os.read(
                    descriptor,
                    min(READ_CHUNK, limit + 1 - retained),
                )
                if not chunk:
                    break
                chunks.append(chunk)
                retained += len(chunk)
            if retained > limit:
                raise GateError(
                    f"{_display(self.root, relative)}: "
                    "reviewed source exceeds the byte limit"
                )
            after = os.fstat(descriptor)
            if _identity(opened) != _identity(after) or retained != after.st_size:
                raise GateError(
                    f"{_display(self.root, relative)}: "
                    "reviewed source bytes changed during the read"
                )
            return b"".join(chunks), _identity(opened)
        except OSError as error:
            raise GateError(
                f"{_display(self.root, relative)}: "
                f"could not read reviewed source: {error}"
            ) from error
        finally:
            if descriptor >= 0:
                os.close(descriptor)
            os.close(directory)
            self._check_root()

    def read_regular(self, relative: str, limit: int) -> bytes:
        """Read a bounded stable regular leaf without retaining its identity."""

        raw, _identity_value = self.read_regular_snapshot(relative, limit)
        return raw


def stable_directory_identity(descriptor: int) -> tuple[int, ...]:
    """Expose one directory identity for streaming traversal stability checks."""

    return _identity(os.fstat(descriptor))
