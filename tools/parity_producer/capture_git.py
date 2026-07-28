"""Exact-HEAD workflow authority for canonical live parity."""

from __future__ import annotations

import hashlib
from pathlib import Path
from typing import Any

from parity_producer.bounded_process import run_bounded
from parity_producer.common import (
    GIT_EXECUTABLE,
    GIT_FIXED_OPTIONS,
    MAX_GIT_OUTPUT_BYTES,
    WORKFLOW_PATH,
    ProducerError,
    git_environment,
)


def verify_source_blob(
    checkout: Path,
    trusted_source_commit: str,
    source: dict[str, Any],
) -> bytes:
    """Bind an observation's workflow digest to the exact trusted HEAD."""
    if _git(checkout, "rev-parse", "HEAD").strip() != trusted_source_commit.encode(
        "ascii"
    ):
        raise ProducerError("live parity source commit differs from HEAD")
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
        raise ProducerError("live parity authority is unavailable from Git")
    return result.stdout
