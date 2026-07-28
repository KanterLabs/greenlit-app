"""Filesystem identity bindings for live parity inputs and outputs."""

from __future__ import annotations

import hashlib
import os
import stat
from dataclasses import dataclass
from pathlib import Path

from .contract import CASE_ID, ROLES
from .errors import GateError


@dataclass(frozen=True)
class _Identity:
    device: int
    inode: int
    mode: int
    owner: int


@dataclass(frozen=True)
class _FileObservation:
    identity: _Identity
    size: int
    modified_ns: int
    changed_ns: int
    digest: str


def _observe_private_file(path: Path) -> _FileObservation:
    flags = os.O_RDONLY | os.O_CLOEXEC | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
        try:
            before = os.fstat(descriptor)
            if (
                not stat.S_ISREG(before.st_mode)
                or stat.S_IMODE(before.st_mode) != 0o600
                or before.st_uid != os.geteuid()
                or before.st_nlink != 1
            ):
                raise GateError("live parity evidence is not one private regular file")
            digest = hashlib.sha256()
            while True:
                chunk = os.read(descriptor, 1024 * 1024)
                if not chunk:
                    break
                digest.update(chunk)
            after = os.fstat(descriptor)
        finally:
            os.close(descriptor)
    except OSError as error:
        raise GateError(f"cannot read live parity evidence {path}: {error}") from error
    stable = ("st_dev", "st_ino", "st_mode", "st_uid", "st_nlink", "st_size")
    if any(getattr(before, field) != getattr(after, field) for field in stable):
        raise GateError(f"live parity evidence changed while it was read: {path}")
    if (
        before.st_mtime_ns != after.st_mtime_ns
        or before.st_ctime_ns != after.st_ctime_ns
    ):
        raise GateError(f"live parity evidence changed while it was read: {path}")
    return _FileObservation(
        _Identity(
            after.st_dev,
            after.st_ino,
            stat.S_IMODE(after.st_mode),
            after.st_uid,
        ),
        after.st_size,
        after.st_mtime_ns,
        after.st_ctime_ns,
        digest.hexdigest(),
    )


@dataclass(frozen=True)
class RoleEvidenceBinding:
    """Content and inode bindings for one completed authority role."""

    role: str
    files: tuple[tuple[Path, _FileObservation], ...]

    def verify(self, stage: str) -> None:
        for path, expected in self.files:
            if _observe_private_file(path) != expected:
                raise GateError(
                    f"{self.role} evidence identity or bytes changed at {stage}"
                )


def _normalized_absolute(path: Path, source: str) -> Path:
    if not path.is_absolute():
        raise GateError(f"{source} must be absolute")
    normalized = Path(os.path.abspath(os.fspath(path)))
    if normalized != path:
        raise GateError(f"{source} must be lexically normalized")
    return path


def _reject_symlink_components(path: Path, source: str) -> os.stat_result:
    current = Path(path.anchor)
    try:
        for part in path.parts[1:]:
            current /= part
            metadata = current.lstat()
            if stat.S_ISLNK(metadata.st_mode):
                raise GateError(f"{source} contains symlink component {current}")
        return path.lstat()
    except OSError as error:
        raise GateError(f"cannot inspect {source}: {error}") from error


class OutputRoot:
    """Open descriptor and pathname binding for the private evidence root."""

    def __init__(self, path: Path, descriptor: int, identity: _Identity) -> None:
        self.path = path
        self._descriptor = descriptor
        self._identity = identity

    @classmethod
    def bind(
        cls,
        path: Path,
        repository: Path,
        *,
        require_empty: bool = True,
    ) -> "OutputRoot":
        """Bind a private root outside the checkout, optionally requiring empty."""

        path = _normalized_absolute(path, "live parity output root")
        metadata = _reject_symlink_components(path, "live parity output root")
        try:
            resolved = path.resolve(strict=True)
            entries = list(path.iterdir())
        except OSError as error:
            raise GateError(f"cannot inspect live parity output root: {error}") from error
        if (
            resolved != path
            or not stat.S_ISDIR(metadata.st_mode)
            or stat.S_IMODE(metadata.st_mode) != 0o700
            or metadata.st_uid != os.geteuid()
        ):
            raise GateError(
                "live parity output root must be a real current-user mode-0700 directory"
            )
        if path == repository or path in repository.parents or repository in path.parents:
            raise GateError("live parity output root must be outside and disjoint")
        if require_empty and entries:
            raise GateError("live parity output root must initially be empty")
        flags = os.O_RDONLY | os.O_CLOEXEC
        flags |= getattr(os, "O_DIRECTORY", 0)
        flags |= getattr(os, "O_NOFOLLOW", 0)
        try:
            descriptor = os.open(path, flags)
            opened = os.fstat(descriptor)
        except OSError as error:
            raise GateError(f"cannot bind live parity output root: {error}") from error
        identity = _Identity(
            opened.st_dev,
            opened.st_ino,
            stat.S_IMODE(opened.st_mode),
            opened.st_uid,
        )
        result = cls(path, descriptor, identity)
        try:
            result.verify("initial binding")
        except GateError:
            os.close(descriptor)
            raise
        return result

    def close(self) -> None:
        os.close(self._descriptor)

    def __enter__(self) -> "OutputRoot":
        return self

    def __exit__(self, *_: object) -> None:
        self.close()

    def verify(self, stage: str) -> None:
        """Revalidate descriptor, path, ownership, and private mode."""

        try:
            opened = os.fstat(self._descriptor)
            metadata = self.path.lstat()
            resolved = self.path.resolve(strict=True)
        except OSError as error:
            raise GateError(
                f"live parity output root changed at {stage}: {error}"
            ) from error
        observed = _Identity(
            opened.st_dev,
            opened.st_ino,
            stat.S_IMODE(opened.st_mode),
            opened.st_uid,
        )
        path_identity = _Identity(
            metadata.st_dev,
            metadata.st_ino,
            stat.S_IMODE(metadata.st_mode),
            metadata.st_uid,
        )
        if (
            observed != self._identity
            or path_identity != self._identity
            or resolved != self.path
            or not stat.S_ISDIR(opened.st_mode)
            or opened.st_uid != os.geteuid()
            or stat.S_IMODE(opened.st_mode) != 0o700
        ):
            raise GateError(f"live parity output root identity changed at {stage}")

    def require_layout(self, roles: tuple[str, ...], stage: str) -> None:
        """Require exactly the role files produced so far, with private modes."""

        self.verify(stage)
        expected_top = {f"seed-{role}.json" for role in roles}
        if roles:
            expected_top.add("captures")
        try:
            top = {entry.name: entry for entry in os.scandir(self.path)}
        except OSError as error:
            raise GateError(f"cannot inspect live parity layout at {stage}: {error}") from error
        if set(top) != expected_top:
            raise GateError(f"live parity output layout is not exact at {stage}")
        for name in expected_top - {"captures"}:
            self._private_file(self.path / name, stage)
        if not roles:
            return
        captures = self.path / "captures"
        try:
            capture_metadata = captures.lstat()
            capture_entries = {entry.name for entry in os.scandir(captures)}
        except OSError as error:
            raise GateError(f"cannot inspect live parity captures at {stage}: {error}") from error
        expected_captures = {f"{CASE_ID}-{role}.json" for role in roles}
        if (
            not stat.S_ISDIR(capture_metadata.st_mode)
            or stat.S_IMODE(capture_metadata.st_mode) != 0o700
            or capture_metadata.st_uid != os.geteuid()
            or capture_entries != expected_captures
        ):
            raise GateError(f"live parity capture layout is not exact at {stage}")
        for name in expected_captures:
            self._private_file(captures / name, stage)

    def bind_role(self, role: str, stage: str) -> RoleEvidenceBinding:
        """Pin both canonical files for one newly completed authority."""

        if role not in ROLES:
            raise GateError(f"cannot bind unknown live parity role {role!r}")
        self.verify(stage)
        paths = (
            self.path / f"seed-{role}.json",
            self.path / "captures" / f"{CASE_ID}-{role}.json",
        )
        return RoleEvidenceBinding(
            role,
            tuple((path, _observe_private_file(path)) for path in paths),
        )

    @staticmethod
    def _private_file(path: Path, stage: str) -> None:
        try:
            _observe_private_file(path)
        except GateError as error:
            raise GateError(f"cannot inspect live parity file at {stage}: {error}") from error


@dataclass(frozen=True)
class BinaryBinding:
    """Stable executable identity for the release-built Greenlit binary."""

    path: Path
    identity: _Identity
    size: int
    digest: str

    @classmethod
    def bind(cls, path: Path) -> "BinaryBinding":
        path = _normalized_absolute(path, "Greenlit release binary path")
        _reject_symlink_components(path, "Greenlit release binary path")
        identity, size, digest = cls._observe(path)
        return cls(path, identity, size, digest)

    def verify(self, stage: str) -> None:
        try:
            _reject_symlink_components(self.path, "Greenlit release binary path")
            resolved = self.path.resolve(strict=True)
            identity, size, digest = self._observe(self.path)
        except (GateError, OSError) as error:
            raise GateError(f"Greenlit release binary changed at {stage}: {error}") from error
        if (
            identity != self.identity
            or size != self.size
            or digest != self.digest
            or resolved != self.path
        ):
            raise GateError(f"Greenlit release binary identity changed at {stage}")

    @staticmethod
    def _observe(path: Path) -> tuple[_Identity, int, str]:
        flags = os.O_RDONLY | os.O_CLOEXEC | getattr(os, "O_NOFOLLOW", 0)
        try:
            descriptor = os.open(path, flags)
            try:
                before = os.fstat(descriptor)
                if (
                    not stat.S_ISREG(before.st_mode)
                    or before.st_mode & 0o111 == 0
                    or before.st_nlink != 1
                ):
                    raise GateError(
                        "Greenlit release binary is not one regular executable"
                    )
                digest = hashlib.sha256()
                while True:
                    chunk = os.read(descriptor, 1024 * 1024)
                    if not chunk:
                        break
                    digest.update(chunk)
                after = os.fstat(descriptor)
            finally:
                os.close(descriptor)
        except OSError as error:
            raise GateError(f"cannot hash Greenlit release binary: {error}") from error
        stable_fields = (
            "st_dev",
            "st_ino",
            "st_size",
            "st_mode",
            "st_uid",
            "st_mtime_ns",
            "st_ctime_ns",
        )
        if any(getattr(before, field) != getattr(after, field) for field in stable_fields):
            raise GateError("Greenlit release binary changed while it was hashed")
        return (
            _Identity(
                after.st_dev,
                after.st_ino,
                stat.S_IMODE(after.st_mode),
                after.st_uid,
            ),
            after.st_size,
            digest.hexdigest(),
        )
