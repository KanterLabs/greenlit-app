"""Committed-file authority for canonical parity replay."""

from __future__ import annotations

import hashlib
import subprocess
from pathlib import Path
from typing import Any

from parity_producer.bounded_process import run_bounded
from parity_producer.common import (
    MAX_JSON_BYTES,
    GIT_EXECUTABLE,
    GIT_FIXED_OPTIONS,
    MAX_GIT_OUTPUT_BYTES,
    WORKFLOW_PATH,
    ProducerError,
    git_environment,
)
from parity_producer.secure_output import read_bytes_beneath


def committed_regular_bytes(checkout: Path, path: Path, source: str) -> bytes:
    """Return a HEAD blob after requiring identical regular worktree bytes."""
    try:
        relative_path = path.relative_to(checkout)
        relative = relative_path.as_posix()
    except ValueError as error:
        raise ProducerError(f"cannot inspect {source} at {path}: {error}") from error
    listing = _git(
        checkout,
        "ls-tree",
        "-z",
        "--full-tree",
        "HEAD",
        "--",
        relative,
    )
    entries = [entry for entry in listing.split(b"\0") if entry]
    if len(entries) != 1 or b"\t" not in entries[0]:
        raise ProducerError(f"{source} is not committed at HEAD: {relative}")
    metadata_fields, raw_path = entries[0].split(b"\t", 1)
    fields = metadata_fields.split()
    try:
        listed_path = raw_path.decode("utf-8", errors="strict")
        object_id = fields[2].decode("ascii", errors="strict")
    except (IndexError, UnicodeDecodeError) as error:
        raise ProducerError(f"{source} has malformed Git tree metadata") from error
    if (
        listed_path != relative
        or len(fields) != 3
        or fields[0] not in {b"100644", b"100755"}
        or fields[1] != b"blob"
    ):
        raise ProducerError(f"{source} is not a committed regular file: {relative}")
    committed = _git(checkout, "cat-file", "blob", object_id)
    working = read_bytes_beneath(checkout, relative_path, MAX_JSON_BYTES, source)
    if working != committed:
        raise ProducerError(f"{source} differs from committed HEAD bytes: {relative}")
    return committed


def verify_source_blob(
    checkout: Path,
    trusted_source_commit: str,
    source: dict[str, Any],
) -> bytes:
    """Bind an observation's workflow digest to the trusted reachable commit."""
    try:
        ancestor = subprocess.run(
            [
                GIT_EXECUTABLE,
                *GIT_FIXED_OPTIONS,
                "-C",
                str(checkout),
                "merge-base",
                "--is-ancestor",
                trusted_source_commit,
                "HEAD",
            ],
            env=git_environment(),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=30,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ProducerError(f"cannot verify capture source commit: {error}") from error
    if ancestor.returncode != 0:
        raise ProducerError("capture source commit is not reachable from HEAD")
    workflow = _git(
        checkout,
        "cat-file",
        "blob",
        f"{trusted_source_commit}:{WORKFLOW_PATH}",
    )
    if hashlib.sha256(workflow).hexdigest() != source.get("workflow_sha256"):
        raise ProducerError("capture workflow identity differs from trusted source blob")
    return workflow


def _git(checkout: Path, *arguments: str) -> bytes:
    result = run_bounded(
        [
            GIT_EXECUTABLE,
            *GIT_FIXED_OPTIONS,
            "-C",
            str(checkout),
            *arguments,
        ],
        label="committed parity Git read",
        environment=git_environment(),
        timeout_seconds=30,
        stdout_limit=MAX_GIT_OUTPUT_BYTES,
        stderr_limit=64 * 1024,
    )
    if result.returncode != 0:
        raise ProducerError("committed parity authority is unavailable from Git")
    return result.stdout
