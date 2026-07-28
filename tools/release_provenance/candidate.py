"""Exact candidate/parity closure, manifest creation, and static verification."""

from __future__ import annotations

import os
import stat
import tempfile
from pathlib import Path

from .common import (
    BINARY_BASENAME,
    CRATES,
    MANIFEST_NAME,
    PARITY_FILES,
    SCHEMA,
    ProvenanceError,
    canonical_json,
    expected_version,
    hash_regular,
    inspect_elf,
    open_regular,
    require_mode,
)
from .crate_archive import inspect_crate
from .manifest import difference, load_manifest
from .package_source import rebuilt_package_digests
from .repository import verify_repository
from .version_execution import run_trusted_version


def _exact_entries(
    directory: Path,
    label: str,
    expected: dict[str, tuple[str, int]],
) -> None:
    try:
        entries = {entry.name: entry for entry in os.scandir(directory)}
    except OSError as error:
        raise ProvenanceError(f"could not enumerate {label}: {error}") from error
    if set(entries) != set(expected):
        raise ProvenanceError(f"{label} does not have its exact required closure")
    for name, (kind, mode) in expected.items():
        try:
            metadata = entries[name].stat(follow_symlinks=False)
        except OSError as error:
            raise ProvenanceError(f"could not inspect {label}/{name}: {error}") from error
        if stat.S_ISLNK(metadata.st_mode):
            raise ProvenanceError(f"{label}/{name} is a symbolic link")
        actual_kind = (
            "directory"
            if stat.S_ISDIR(metadata.st_mode)
            else "file"
            if stat.S_ISREG(metadata.st_mode)
            else "special"
        )
        if actual_kind != kind or stat.S_IMODE(metadata.st_mode) != mode:
            raise ProvenanceError(
                f"{label}/{name} must be a mode-{mode:04o} {kind}"
            )


def scan_candidate(candidate: Path, *, include_manifest: bool) -> None:
    """Require the candidate's exact directories, files, types, and modes."""

    require_mode(candidate, 0o700, "candidate root", directory=True)
    root_files = (
        {MANIFEST_NAME: ("file", 0o644)} if include_manifest else {}
    )
    _exact_entries(
        candidate,
        "candidate root",
        {"target": ("directory", 0o755), **root_files},
    )
    _exact_entries(
        candidate / "target",
        "candidate target",
        {
            "package": ("directory", 0o755),
            "release": ("directory", 0o755),
        },
    )
    _exact_entries(
        candidate / "target/release",
        "candidate release directory",
        {BINARY_BASENAME: ("file", 0o755)},
    )
    _exact_entries(
        candidate / "target/package",
        "candidate package directory",
        {basename: ("file", 0o644) for basename in CRATES},
    )


def scan_parity(parity: Path) -> None:
    """Require the exact six-file private parity evidence closure."""

    require_mode(parity, 0o700, "parity evidence root", directory=True)
    _exact_entries(
        parity,
        "parity evidence root",
        {
            "captures": ("directory", 0o700),
            "seed-github-actions.json": ("file", 0o600),
            "seed-greenlit-release.json": ("file", 0o600),
            "seed-oracle.json": ("file", 0o600),
        },
    )
    _exact_entries(
        parity / "captures",
        "parity capture directory",
        {
            "shell-only-seed-github-actions.json": ("file", 0o600),
            "shell-only-seed-greenlit-release.json": ("file", 0o600),
            "shell-only-seed-oracle.json": ("file", 0o600),
        },
    )


def _run_version(path: Path, expected: bytes) -> None:
    """Execute only the trusted build-job binary with bounded descendants/output."""

    run_trusted_version(path, expected)


def _binary(path: Path, expected_source: str, *, execute: bool) -> dict[str, object]:
    require_mode(path, 0o755, "release binary", directory=False)
    elf = inspect_elf(path)
    marker = f"0.1.0 ({expected_source})".encode("ascii")
    found_marker = False
    carry = b""
    with open_regular(path, "release binary") as handle:
        while chunk := handle.read(1024 * 1024):
            combined = carry + chunk
            if marker in combined:
                found_marker = True
            carry = combined[-max(0, len(marker) - 1) :]
    if not found_marker:
        raise ProvenanceError(
            "release binary does not contain its exact embedded source identity"
        )
    digest_before = hash_regular(path, "release binary")
    if execute:
        _run_version(path, f"{expected_version(expected_source)}\n".encode("ascii"))
    digest_after = hash_regular(path, "release binary")
    if digest_before != digest_after:
        raise ProvenanceError("release binary changed during provenance inspection")
    return {
        "basename": BINARY_BASENAME,
        "elf": elf,
        "sha256": digest_before,
        "version_output": expected_version(expected_source),
    }


def observe(
    repository: Path,
    candidate: Path,
    parity: Path,
    expected_source: str,
    *,
    execute_binary: bool,
) -> dict[str, object]:
    """Observe one exact source-derived candidate and parity evidence set."""

    canonical = rebuilt_package_digests(repository)
    crates = {
        basename: inspect_crate(
            candidate / "target/package" / basename,
            basename,
            repository,
            expected_source,
            path_in_vcs,
            canonical[basename],
        )
        for basename, path_in_vcs in sorted(CRATES.items())
    }
    parity_hashes = {
        relative: hash_regular(
            parity / relative,
            f"parity evidence {relative}",
        )
        for relative in PARITY_FILES
    }
    return {
        "binary": _binary(
            candidate / "target/release/litci",
            expected_source,
            execute=execute_binary,
        ),
        "crates": crates,
        "parity": parity_hashes,
        "schema": SCHEMA,
        "source_commit": expected_source,
    }


def verify(
    repository: Path,
    candidate: Path,
    parity: Path,
    expected_source: str,
) -> None:
    """Statically verify candidate bytes; never execute candidate content."""

    verify_repository(repository, expected_source)
    scan_candidate(candidate, include_manifest=True)
    scan_parity(parity)
    document = load_manifest(candidate, expected_source)
    observed = observe(
        repository,
        candidate,
        parity,
        expected_source,
        execute_binary=False,
    )
    mismatch = difference(document, observed)
    if mismatch is not None:
        raise ProvenanceError(
            f"release provenance does not match candidate at {mismatch}"
        )


def _fsync_directory(directory: Path) -> None:
    flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0)
    descriptor = os.open(directory, flags)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def _write_manifest(path: Path, data: bytes) -> tuple[int, int]:
    if path.exists() or path.is_symlink():
        raise ProvenanceError(f"refusing to overwrite existing manifest {path}")
    descriptor, raw = tempfile.mkstemp(
        prefix=".release-provenance.",
        suffix=".tmp",
        dir=path.parent,
    )
    temporary = Path(raw)
    try:
        os.fchmod(descriptor, 0o644)
        with os.fdopen(descriptor, "wb", closefd=True) as handle:
            descriptor = -1
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        metadata = os.lstat(temporary)
        identity = (metadata.st_dev, metadata.st_ino)
        os.link(temporary, path, follow_symlinks=False)
        temporary.unlink()
        _fsync_directory(path.parent)
        return identity
    except OSError as error:
        raise ProvenanceError(f"could not atomically create {path}: {error}") from error
    finally:
        if descriptor >= 0:
            os.close(descriptor)
        if temporary.exists() or temporary.is_symlink():
            temporary.unlink()


def _remove_same(path: Path, identity: tuple[int, int]) -> None:
    try:
        metadata = os.lstat(path)
        if (metadata.st_dev, metadata.st_ino) == identity:
            path.unlink()
            _fsync_directory(path.parent)
    except OSError:
        return


def create(
    repository: Path,
    candidate: Path,
    parity: Path,
    expected_source: str,
) -> None:
    """Create provenance only for a trusted locally built, executed candidate."""

    verify_repository(repository, expected_source)
    scan_candidate(candidate, include_manifest=False)
    scan_parity(parity)
    document = observe(
        repository,
        candidate,
        parity,
        expected_source,
        execute_binary=True,
    )
    path = candidate / MANIFEST_NAME
    identity = _write_manifest(path, canonical_json(document))
    try:
        verify(repository, candidate, parity, expected_source)
    except ProvenanceError:
        _remove_same(path, identity)
        raise
