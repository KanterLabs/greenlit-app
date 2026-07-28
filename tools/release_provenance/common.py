"""Shared strict I/O, schema constants, and static binary checks."""

from __future__ import annotations

import hashlib
import json
import os
import re
import stat
from pathlib import Path
from typing import BinaryIO


SCHEMA = "greenlit.release-provenance.v2"
MANIFEST_NAME = "RELEASE-PROVENANCE.json"
BINARY_BASENAME = "litci"
APP_VERSION = "0.1.0"
SOURCE_PATTERN = re.compile(r"[0-9a-f]{40}\Z")
DIGEST_PATTERN = re.compile(r"[0-9a-f]{64}\Z")
MAX_MANIFEST_BYTES = 128 * 1024
MAX_VCS_INFO_BYTES = 16 * 1024
MAX_CRATE_BYTES = 64 * 1024 * 1024
MAX_CRATE_MEMBER_BYTES = 16 * 1024 * 1024
MAX_CRATE_EXPANDED_BYTES = 128 * 1024 * 1024
MAX_TAR_MEMBERS = 100_000

CRATES = {
    "greenlit-actions-0.1.0.crate": "crates/greenlit-actions",
    "greenlit-app-0.1.0.crate": "crates/greenlit-app",
    "greenlit-engine-0.1.0.crate": "crates/greenlit-engine",
    "greenlit-expr-0.1.0.crate": "crates/greenlit-expr",
    "greenlit-metrics-0.1.0.crate": "crates/greenlit-metrics",
    "greenlit-runtime-0.1.0.crate": "crates/greenlit-runtime",
    "greenlit-store-0.1.0.crate": "crates/greenlit-store",
    "greenlit-workflow-0.1.0.crate": "crates/greenlit-workflow",
}

PARITY_FILES = (
    "captures/shell-only-seed-github-actions.json",
    "captures/shell-only-seed-greenlit-release.json",
    "captures/shell-only-seed-oracle.json",
    "seed-github-actions.json",
    "seed-greenlit-release.json",
    "seed-oracle.json",
)


class ProvenanceError(Exception):
    """A release candidate or sealed bundle violated its contract."""


class DuplicateKeyError(ValueError):
    """A JSON object repeated a key."""


def duplicate_safe_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    """Build one JSON object while rejecting repeated keys."""

    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise DuplicateKeyError(f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def decode_json(data: bytes, label: str) -> object:
    """Decode strict UTF-8 JSON without nonstandard numeric constants."""

    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ProvenanceError(f"{label} is not valid UTF-8: {error}") from error
    try:
        return json.loads(
            text,
            object_pairs_hook=duplicate_safe_object,
            parse_constant=lambda value: (_ for _ in ()).throw(
                ValueError(f"non-standard JSON constant {value!r}")
            ),
        )
    except (json.JSONDecodeError, DuplicateKeyError, ValueError) as error:
        raise ProvenanceError(f"{label} is not strict JSON: {error}") from error


def canonical_json(document: dict[str, object]) -> bytes:
    """Encode the one canonical provenance representation."""

    return (
        json.dumps(
            document,
            ensure_ascii=True,
            separators=(",", ":"),
            sort_keys=True,
        )
        + "\n"
    ).encode("ascii")


def require_object(value: object, label: str) -> dict[str, object]:
    """Require a JSON object without accepting mapping subclasses."""

    if type(value) is not dict:
        raise ProvenanceError(f"{label} must be a JSON object")
    return value


def require_exact_keys(
    value: dict[str, object],
    expected: set[str],
    label: str,
) -> None:
    """Require exactly one closed set of JSON object keys."""

    actual = set(value)
    missing = sorted(expected - actual)
    unknown = sorted(actual - expected)
    if missing or unknown:
        details = []
        if missing:
            details.append(f"missing {missing}")
        if unknown:
            details.append(f"unknown {unknown}")
        raise ProvenanceError(f"{label} has invalid keys: {', '.join(details)}")


def require_string(value: object, label: str) -> str:
    """Require one JSON string."""

    if type(value) is not str:
        raise ProvenanceError(f"{label} must be a JSON string")
    return value


def require_digest(value: object, label: str) -> str:
    """Require one lowercase SHA-256 spelling."""

    digest = require_string(value, label)
    if DIGEST_PATTERN.fullmatch(digest) is None:
        raise ProvenanceError(
            f"{label} must be exactly 64 lowercase hexadecimal characters"
        )
    return digest


def validate_source(source: str, label: str) -> None:
    """Require one full lowercase SHA-1 commit identity."""

    if SOURCE_PATTERN.fullmatch(source) is None:
        raise ProvenanceError(
            f"{label} must be exactly 40 lowercase hexadecimal characters"
        )


def validated_directory(path: Path, label: str) -> Path:
    """Resolve and require one real directory without a final symlink."""

    absolute = Path(os.path.abspath(os.fspath(path)))
    try:
        metadata = os.lstat(absolute)
        resolved = absolute.resolve(strict=True)
    except OSError as error:
        raise ProvenanceError(f"could not inspect {label} {absolute}: {error}") from error
    if resolved != absolute or stat.S_ISLNK(metadata.st_mode):
        raise ProvenanceError(f"{label} must not contain a symbolic link: {absolute}")
    if not stat.S_ISDIR(metadata.st_mode):
        raise ProvenanceError(f"{label} is not a directory: {absolute}")
    return absolute


def open_regular(path: Path, label: str) -> BinaryIO:
    """Open one final regular file without following its final component."""

    flags = os.O_RDONLY
    if hasattr(os, "O_CLOEXEC"):
        flags |= os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise ProvenanceError(f"could not open {label}: {error}") from error
    try:
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode):
            raise ProvenanceError(f"{label} is not a regular file")
        return os.fdopen(descriptor, "rb", closefd=True)
    except Exception:
        os.close(descriptor)
        raise


def read_bounded(path: Path, label: str, limit: int) -> bytes:
    """Read at most one fixed number of bytes from a regular file."""

    with open_regular(path, label) as handle:
        data = handle.read(limit + 1)
    if len(data) > limit:
        raise ProvenanceError(f"{label} exceeds the {limit}-byte limit")
    return data


def hash_stream(handle: BinaryIO) -> str:
    """Hash a stream from its current position through EOF."""

    digest = hashlib.sha256()
    while chunk := handle.read(1024 * 1024):
        digest.update(chunk)
    return digest.hexdigest()


def hash_regular(path: Path, label: str) -> str:
    """Hash one no-follow regular file."""

    with open_regular(path, label) as handle:
        before = os.fstat(handle.fileno())
        digest = hash_stream(handle)
        after = os.fstat(handle.fileno())
    fields = ("st_dev", "st_ino", "st_size", "st_mtime_ns", "st_ctime_ns")
    if any(getattr(before, field) != getattr(after, field) for field in fields):
        raise ProvenanceError(f"{label} changed while it was hashed")
    return digest


def require_mode(path: Path, mode: int, label: str, *, directory: bool) -> os.stat_result:
    """Require one exact no-follow type and permission mode."""

    try:
        metadata = os.lstat(path)
    except OSError as error:
        raise ProvenanceError(f"could not inspect {label}: {error}") from error
    expected_type = stat.S_ISDIR if directory else stat.S_ISREG
    if not expected_type(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode):
        kind = "directory" if directory else "regular file"
        raise ProvenanceError(f"{label} is not a real {kind}")
    actual = stat.S_IMODE(metadata.st_mode)
    if actual != mode:
        raise ProvenanceError(
            f"{label} mode is {actual:04o}, expected exactly {mode:04o}"
        )
    return metadata


def inspect_elf(path: Path) -> dict[str, object]:
    """Require a Linux x86-64 ELF executable without running it."""

    with open_regular(path, "release binary") as handle:
        data = handle.read(64)
    if len(data) < 64 or data[:4] != b"\x7fELF":
        raise ProvenanceError("release binary is not an ELF file")
    if data[4:7] != bytes((2, 1, 1)):
        raise ProvenanceError(
            "release binary must be ELF64, little-endian, version 1"
        )
    elf_type = int.from_bytes(data[16:18], "little")
    machine = int.from_bytes(data[18:20], "little")
    version = int.from_bytes(data[20:24], "little")
    if elf_type not in {2, 3} or machine != 62 or version != 1:
        raise ProvenanceError(
            "release binary must be an x86-64 executable/shared-object ELF"
        )
    return {
        "class": "ELF64",
        "data": "little-endian",
        "machine": "x86_64",
    }


def expected_version(source: str) -> str:
    """Return the exact embedded CLI version output without a newline."""

    return f"litci {APP_VERSION} ({source})"
