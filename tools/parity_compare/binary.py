"""Exact release-binary provenance validation for parity evidence."""

from __future__ import annotations

import hashlib
import os
import re
import selectors
import signal
import stat
import subprocess
import time
from pathlib import Path

from . import ContractError
from .binary_process import (
    ProcessContainmentError,
    establish_process_boundary,
    terminate_descendants,
)


SHA256 = re.compile(r"^[0-9a-f]{64}$")
COMMIT = re.compile(r"^[0-9a-f]{40}$")
VERSION = re.compile(r"^litci \S+ \(([0-9a-f]{40})\)\n$")
READ_CHUNK_BYTES = 1024 * 1024
VERSION_TIMEOUT_SECONDS = 10
MAX_VERSION_OUTPUT_BYTES = 4096


class BinaryProvenanceError(ContractError):
    """A Greenlit release binary violates its exact-byte contract."""


def _identity(metadata: os.stat_result) -> tuple[int, ...]:
    return (
        metadata.st_dev,
        metadata.st_ino,
        metadata.st_mode,
        metadata.st_size,
        metadata.st_mtime_ns,
        metadata.st_ctime_ns,
    )


def _open_executable_nofollow(binary: Path) -> tuple[int, os.stat_result]:
    try:
        named = binary.lstat()
    except OSError as error:
        raise BinaryProvenanceError(
            f"cannot inspect Greenlit release binary {binary}: {error}"
        ) from error
    if not stat.S_ISREG(named.st_mode) or named.st_mode & 0o111 == 0:
        raise BinaryProvenanceError(
            "Greenlit release binary is not a regular non-symlink executable: "
            f"{binary}"
        )
    nofollow = getattr(os, "O_NOFOLLOW", None)
    if nofollow is None:
        raise BinaryProvenanceError(
            "this host cannot open the Greenlit release binary without following links"
        )
    flags = os.O_RDONLY | nofollow | getattr(os, "O_CLOEXEC", 0)
    try:
        descriptor = os.open(binary, flags)
    except OSError as error:
        raise BinaryProvenanceError(
            f"cannot open Greenlit release binary {binary}: {error}"
        ) from error
    try:
        opened = os.fstat(descriptor)
    except OSError as error:
        os.close(descriptor)
        raise BinaryProvenanceError(
            f"cannot inspect opened Greenlit release binary {binary}: {error}"
        ) from error
    if (
        not stat.S_ISREG(opened.st_mode)
        or opened.st_mode & 0o111 == 0
        or (opened.st_dev, opened.st_ino) != (named.st_dev, named.st_ino)
    ):
        os.close(descriptor)
        raise BinaryProvenanceError(
            f"Greenlit release binary changed while it was opened: {binary}"
        )
    return descriptor, opened


def _descriptor_sha256(descriptor: int, binary: Path) -> str:
    digest = hashlib.sha256()
    try:
        os.lseek(descriptor, 0, os.SEEK_SET)
        while True:
            chunk = os.read(descriptor, READ_CHUNK_BYTES)
            if not chunk:
                break
            digest.update(chunk)
        os.lseek(descriptor, 0, os.SEEK_SET)
    except OSError as error:
        raise BinaryProvenanceError(
            f"cannot read Greenlit release binary {binary}: {error}"
        ) from error
    return digest.hexdigest()


def _minimal_environment() -> dict[str, str]:
    return {
        "HOME": "/",
        "LANG": "C",
        "LC_ALL": "C",
        "PATH": "/usr/bin:/bin",
        "TZ": "UTC",
    }


def _kill_process_group(process: subprocess.Popen[bytes]) -> None:
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass


def _terminate_process_group(process: subprocess.Popen[bytes]) -> None:
    _kill_process_group(process)
    try:
        process.wait(timeout=1)
    except subprocess.TimeoutExpired:
        pass


def _collect_version_output(
    process: subprocess.Popen[bytes],
) -> tuple[bytes, bytes]:
    streams = {
        process.stdout: bytearray(),
        process.stderr: bytearray(),
    }
    selector = selectors.DefaultSelector()
    deadline = time.monotonic() + VERSION_TIMEOUT_SECONDS
    try:
        for stream in streams:
            if stream is None:
                raise BinaryProvenanceError(
                    "Greenlit release binary output pipes are unavailable"
                )
            os.set_blocking(stream.fileno(), False)
            selector.register(stream, selectors.EVENT_READ)
        while selector.get_map():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise BinaryProvenanceError(
                    "Greenlit release binary --version exceeded "
                    f"{VERSION_TIMEOUT_SECONDS} seconds"
                )
            events = selector.select(remaining)
            if not events:
                continue
            for key, _ in events:
                stream = key.fileobj
                retained = streams[stream]
                allowance = MAX_VERSION_OUTPUT_BYTES + 1 - sum(
                    len(value) for value in streams.values()
                )
                try:
                    chunk = os.read(stream.fileno(), max(1, allowance))
                except BlockingIOError:
                    continue
                if not chunk:
                    selector.unregister(stream)
                    continue
                retained.extend(chunk)
                if sum(len(value) for value in streams.values()) > (
                    MAX_VERSION_OUTPUT_BYTES
                ):
                    raise BinaryProvenanceError(
                        "Greenlit release binary --version output exceeds "
                        f"{MAX_VERSION_OUTPUT_BYTES} bytes"
                    )
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise subprocess.TimeoutExpired(process.args, VERSION_TIMEOUT_SECONDS)
        process.wait(timeout=remaining)
    except subprocess.TimeoutExpired as error:
        raise BinaryProvenanceError(
            "Greenlit release binary --version exceeded "
            f"{VERSION_TIMEOUT_SECONDS} seconds"
        ) from error
    finally:
        selector.close()
    stdout = streams[process.stdout]
    stderr = streams[process.stderr]
    return bytes(stdout), bytes(stderr)


def _identify_version(
    descriptor: int, binary: Path, trusted_source_commit: str
) -> None:
    command = f"/proc/self/fd/{descriptor}"
    try:
        establish_process_boundary()
    except ProcessContainmentError as error:
        raise BinaryProvenanceError(str(error)) from error
    try:
        process = subprocess.Popen(
            [command, "--version"],
            cwd="/",
            env=_minimal_environment(),
            pass_fds=(descriptor,),
            start_new_session=True,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
    except OSError as error:
        raise BinaryProvenanceError(
            f"cannot identify Greenlit release binary {binary}: {error}"
        ) from error
    survivors = False
    try:
        stdout, stderr = _collect_version_output(process)
    except BinaryProvenanceError:
        _terminate_process_group(process)
        raise
    finally:
        try:
            survivors = terminate_descendants(process.pid)
        except ProcessContainmentError as error:
            raise BinaryProvenanceError(str(error)) from error
        if process.stdout is not None:
            process.stdout.close()
        if process.stderr is not None:
            process.stderr.close()
    if survivors:
        raise BinaryProvenanceError(
            "Greenlit release binary --version left a descendant process"
        )
    if (
        process.returncode != 0
        or stderr
    ):
        raise BinaryProvenanceError(
            "Greenlit release binary --version did not produce its exact identity"
        )
    try:
        output = stdout.decode("utf-8", errors="strict")
    except UnicodeDecodeError as error:
        raise BinaryProvenanceError(
            "Greenlit release binary --version output is not UTF-8"
        ) from error
    match = VERSION.fullmatch(output)
    if match is None or match.group(1) != trusted_source_commit:
        raise BinaryProvenanceError(
            "Greenlit release binary version does not bind the trusted source commit"
        )


def _require_unchanged(
    descriptor: int,
    binary: Path,
    opened: os.stat_result,
    expected_digest: str,
) -> None:
    try:
        current = os.fstat(descriptor)
        named = binary.lstat()
    except OSError as error:
        raise BinaryProvenanceError(
            f"cannot revalidate Greenlit release binary {binary}: {error}"
        ) from error
    if (
        not stat.S_ISREG(named.st_mode)
        or named.st_mode & 0o111 == 0
        or _identity(current) != _identity(opened)
        or _identity(named) != _identity(opened)
        or _descriptor_sha256(descriptor, binary) != expected_digest
    ):
        raise BinaryProvenanceError(
            f"Greenlit release binary changed during validation: {binary}"
        )


def validate_release_binary(
    binary: Path, expected_sha256: str, trusted_source_commit: str
) -> None:
    """Pin, identify, and hash one exact external release executable."""
    if not isinstance(binary, Path):
        raise BinaryProvenanceError("Greenlit release binary must be a filesystem path")
    if not isinstance(expected_sha256, str) or SHA256.fullmatch(expected_sha256) is None:
        raise BinaryProvenanceError(
            "Greenlit release binary evidence must be a lowercase SHA-256"
        )
    if (
        not isinstance(trusted_source_commit, str)
        or COMMIT.fullmatch(trusted_source_commit) is None
    ):
        raise BinaryProvenanceError(
            "trusted release-binary source commit must be a full lowercase SHA"
        )
    descriptor, opened = _open_executable_nofollow(binary)
    try:
        digest = _descriptor_sha256(descriptor, binary)
        if digest != expected_sha256:
            raise BinaryProvenanceError(
                "Greenlit release binary digest does not match captured evidence: "
                f"{binary}"
            )
        _identify_version(descriptor, binary, trusted_source_commit)
        _require_unchanged(descriptor, binary, opened, digest)
    finally:
        os.close(descriptor)
