"""Exact-clean source and private live-output root validation."""

from __future__ import annotations

import os
import stat
from pathlib import Path

from parity_producer.bounded_process import run_bounded
from parity_producer.common import (
    GIT_EXECUTABLE,
    GIT_FIXED_OPTIONS,
    MAX_GIT_OUTPUT_BYTES,
    ProducerError,
    git_environment,
)


ROLES = ("oracle", "github-actions", "greenlit-release")
EXPECTED_ROOT_FILES = {f"seed-{role}.json" for role in ROLES}
EXPECTED_CAPTURE_FILES = {
    f"shell-only-seed-{role}.json" for role in ROLES
}


def validate_live_roots(
    checkout: Path,
    output_root: Path,
    source_commit: str,
) -> tuple[Path, Path]:
    """Require current exact source HEAD and an external owner-private root."""
    if not output_root.is_absolute():
        raise ProducerError("live parity output root must be an absolute path")
    checkout = checkout.resolve()
    output_root = _without_symlink_components(output_root)
    if (
        output_root == checkout
        or checkout in output_root.parents
        or output_root in checkout.parents
    ):
        raise ProducerError("live parity output root must be outside and disjoint")
    if Path(_git(checkout, "rev-parse", "--show-toplevel")).resolve() != checkout:
        raise ProducerError("parity checkout must be the exact Git worktree root")
    if _git(checkout, "rev-parse", "HEAD") != source_commit:
        raise ProducerError("parity checkout HEAD differs from trusted source commit")
    _require_ordinary_index(checkout)
    if _git(
        checkout,
        "status",
        "--porcelain=v1",
        "--untracked-files=all",
        "--ignored=matching",
    ):
        raise ProducerError(
            "live parity requires a pristine tracked-only source checkout"
        )
    _validate_private_directory(output_root, "live parity output root")
    entries = _directory_entries(output_root)
    unexpected = sorted(entries - EXPECTED_ROOT_FILES - {"captures"})
    if unexpected:
        raise ProducerError(
            f"live parity output root contains unexpected entry {unexpected[0]!r}"
        )
    for name in sorted(entries & EXPECTED_ROOT_FILES):
        _validate_private_file(output_root / name)
    captures = output_root / "captures"
    if "captures" in entries:
        _validate_private_directory(captures, "live parity captures directory")
        capture_entries = _directory_entries(captures)
        unexpected_captures = sorted(capture_entries - EXPECTED_CAPTURE_FILES)
        if unexpected_captures:
            raise ProducerError(
                "live parity captures directory contains unexpected entry "
                f"{unexpected_captures[0]!r}"
            )
        for name in sorted(capture_entries):
            _validate_private_file(captures / name)
    return checkout, output_root


def _require_ordinary_index(checkout: Path) -> None:
    entries = [
        entry
        for entry in _git(checkout, "ls-files", "-v", "-z").split("\0")
        if entry
    ]
    if any(not entry.startswith("H ") for entry in entries):
        raise ProducerError(
            "live parity forbids skip-worktree, assume-unchanged, "
            "or nonordinary index entries"
        )


def _validate_private_directory(path: Path, source: str) -> None:
    try:
        metadata = path.lstat()
    except OSError as error:
        raise ProducerError(f"cannot inspect {source} {path}: {error}") from error
    if (
        not stat.S_ISDIR(metadata.st_mode)
        or stat.S_IMODE(metadata.st_mode) != 0o700
        or metadata.st_uid != os.geteuid()
    ):
        raise ProducerError(f"{source} must be a real mode-0700 directory")


def _validate_private_file(path: Path) -> None:
    try:
        metadata = path.lstat()
    except OSError as error:
        raise ProducerError(f"cannot inspect live parity file {path}: {error}") from error
    if (
        not stat.S_ISREG(metadata.st_mode)
        or stat.S_IMODE(metadata.st_mode) != 0o600
        or metadata.st_uid != os.geteuid()
    ):
        raise ProducerError(f"live parity file must be regular mode 0600: {path}")


def _directory_entries(path: Path) -> set[str]:
    try:
        return {entry.name for entry in path.iterdir()}
    except OSError as error:
        raise ProducerError(f"cannot enumerate private parity directory {path}: {error}") from error


def _without_symlink_components(path: Path) -> Path:
    normalized = Path(os.path.abspath(path))
    current = Path(normalized.anchor)
    for part in normalized.parts[1:]:
        current /= part
        try:
            metadata = current.lstat()
        except OSError as error:
            raise ProducerError(
                f"live parity output root component is unavailable: {current}: {error}"
            ) from error
        if stat.S_ISLNK(metadata.st_mode):
            raise ProducerError(
                f"live parity output root contains symlink component {current}"
            )
    return normalized


def _git(checkout: Path, *arguments: str) -> str:
    result = run_bounded(
        [
            GIT_EXECUTABLE,
            *GIT_FIXED_OPTIONS,
            "-C",
            str(checkout),
            *arguments,
        ],
        label="live parity Git inspection",
        environment=git_environment(),
        timeout_seconds=30,
        stdout_limit=MAX_GIT_OUTPUT_BYTES,
        stderr_limit=64 * 1024,
    )
    if result.returncode != 0:
        raise ProducerError("live parity source Git identity is unavailable")
    try:
        return result.stdout.decode("utf-8", errors="strict").strip()
    except UnicodeDecodeError as error:
        raise ProducerError("live parity source Git identity is not UTF-8") from error
