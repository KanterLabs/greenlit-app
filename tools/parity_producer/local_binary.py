"""Immutable release-binary identity for Greenlit parity execution."""

from __future__ import annotations

import hashlib
import os
import re
import stat
from dataclasses import dataclass
from pathlib import Path

from parity_producer.bounded_process import run_bounded
from parity_producer.common import ProducerError


VERSION = re.compile(r"^litci \S+ \(([0-9a-f]{40})\)\n$")
MAX_VERSION_OUTPUT_BYTES = 4096


@dataclass
class PinnedReleaseBinary:
    """An opened release executable whose pathname and bytes must stay stable."""

    path: Path
    descriptor: int
    digest: str
    device: int
    inode: int
    size: int

    @classmethod
    def open(cls, path: Path) -> "PinnedReleaseBinary":
        """Open one no-follow executable and hash the exact retained inode."""
        flags = os.O_RDONLY | os.O_CLOEXEC
        if hasattr(os, "O_NOFOLLOW"):
            flags |= os.O_NOFOLLOW
        try:
            descriptor = os.open(path, flags)
            metadata = os.fstat(descriptor)
        except OSError as error:
            raise ProducerError(f"cannot open release binary {path}: {error}") from error
        if (
            not stat.S_ISREG(metadata.st_mode)
            or metadata.st_mode & 0o111 == 0
            or path.name != "litci"
            or path.parent.name != "release"
        ):
            os.close(descriptor)
            raise ProducerError(
                "local parity binary must be executable <target-dir>/release/litci"
            )
        try:
            digest = _hash_descriptor(descriptor)
        except ProducerError:
            os.close(descriptor)
            raise
        return cls(
            path=path,
            descriptor=descriptor,
            digest=digest,
            device=metadata.st_dev,
            inode=metadata.st_ino,
            size=metadata.st_size,
        )

    @property
    def command(self) -> str:
        """Return the Linux procfd path that executes this exact open inode."""
        return f"/proc/self/fd/{self.descriptor}"

    def validate_version(
        self, trusted_source_commit: str, environment: dict[str, str]
    ) -> None:
        """Require the compile-time source identity in exact version output."""
        result = run_bounded(
            [self.command, "--version"],
            label=f"release binary identity {self.path}",
            cwd=Path("/"),
            environment=environment,
            pass_fds=(self.descriptor,),
            timeout_seconds=10,
            stdout_limit=MAX_VERSION_OUTPUT_BYTES,
            stderr_limit=MAX_VERSION_OUTPUT_BYTES,
        )
        try:
            output = result.stdout.decode("utf-8", errors="strict")
        except UnicodeDecodeError as error:
            raise ProducerError("release binary version output is not UTF-8") from error
        match = VERSION.fullmatch(output)
        if (
            result.returncode != 0
            or result.stderr
            or match is None
            or match.group(1) != trusted_source_commit
        ):
            raise ProducerError(
                "release binary version does not bind the trusted source commit"
            )

    def verify_unchanged(self) -> None:
        """Reject in-place mutation or pathname replacement after execution."""
        try:
            descriptor_metadata = os.fstat(self.descriptor)
            path_metadata = self.path.lstat()
        except OSError as error:
            raise ProducerError(f"cannot revalidate release binary: {error}") from error
        identity = (
            descriptor_metadata.st_dev,
            descriptor_metadata.st_ino,
            descriptor_metadata.st_size,
        )
        path_identity = (
            path_metadata.st_dev,
            path_metadata.st_ino,
            path_metadata.st_size,
        )
        expected = (self.device, self.inode, self.size)
        if (
            not stat.S_ISREG(path_metadata.st_mode)
            or identity != expected
            or path_identity != expected
            or _hash_descriptor(self.descriptor) != self.digest
        ):
            raise ProducerError("release binary changed during parity execution")

    def close(self) -> None:
        """Close the pinned descriptor."""
        os.close(self.descriptor)

    def __enter__(self) -> "PinnedReleaseBinary":
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


def _hash_descriptor(descriptor: int) -> str:
    digest = hashlib.sha256()
    try:
        os.lseek(descriptor, 0, os.SEEK_SET)
        while True:
            chunk = os.read(descriptor, 1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
        os.lseek(descriptor, 0, os.SEEK_SET)
    except OSError as error:
        raise ProducerError(f"cannot hash the opened release binary: {error}") from error
    return digest.hexdigest()
