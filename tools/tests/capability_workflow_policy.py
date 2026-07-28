"""Checker-owned exact identities for capability-governing workflows."""

from __future__ import annotations

import hashlib
import json
import os
import stat
from pathlib import Path
from typing import Any

from cargo_test_manifest import GateError


REQUIRED_ROUTE_POLICY_SHA256 = (
    "e690c670c6c0ba7e14e5ef20c83c80fd140f614fdee7ba9dfc9275fd35831e8d"
)
# These are explicit reviewed authority baselines. There is intentionally no
# auto-update path: any workflow byte change requires a code-reviewed rebind.
REQUIRED_WORKFLOW_POLICY_SHA256 = {
    ".github/workflows/ci.yml": (
        "46ed4a6aeb81c1122a5f706aed932c4eb147dd02a0bd288b906c7dcdcad6aa32"
    ),
    ".github/workflows/release.yml": (
        "0bf17d0a1f11ce9f7f646045068a286571f1e24e3819ff94c13b0f63f693a101"
    ),
}
MAX_WORKFLOW_BYTES = 1024 * 1024


def validate_route_policy(routes: list[dict[str, Any]]) -> None:
    """Require the schema-validated route inventory's reviewed command identity."""

    try:
        canonical = json.dumps(
            routes,
            allow_nan=False,
            ensure_ascii=True,
            separators=(",", ":"),
            sort_keys=True,
        ).encode("utf-8")
    except (TypeError, ValueError) as error:
        raise GateError(
            f"workflow routes cannot be canonicalized: {error}"
        ) from error
    route_policy = b"greenlit-capability-routes-v1\0" + canonical
    if hashlib.sha256(route_policy).hexdigest() != REQUIRED_ROUTE_POLICY_SHA256:
        raise GateError(
            "workflow routes differ from the checker-owned command policy"
        )


def _read_workflow(path: Path) -> tuple[bytes, list[str]]:
    flags = os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise GateError(f"could not open workflow {path}: {error}") from error
    try:
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode):
            raise GateError(f"{path}: workflow must be a regular non-symlink file")
        if metadata.st_size > MAX_WORKFLOW_BYTES:
            raise GateError(f"{path}: workflow exceeds the fixed byte limit")
        chunks: list[bytes] = []
        retained = 0
        while retained <= MAX_WORKFLOW_BYTES:
            chunk = os.read(
                descriptor,
                min(64 * 1024, MAX_WORKFLOW_BYTES + 1 - retained),
            )
            if not chunk:
                break
            chunks.append(chunk)
            retained += len(chunk)
        raw = b"".join(chunks)
        if len(raw) > MAX_WORKFLOW_BYTES:
            raise GateError(f"{path}: workflow exceeds the fixed byte limit")
    except OSError as error:
        raise GateError(f"could not read workflow {path}: {error}") from error
    finally:
        os.close(descriptor)
    try:
        return raw, raw.decode("utf-8").splitlines()
    except UnicodeError as error:
        raise GateError(f"{path}: workflow must be UTF-8: {error}") from error


def load_governed_workflows(
    root: Path,
    governed_workflows: set[str],
) -> dict[str, list[str]]:
    """Read each governed workflow once and bind its complete raw-byte identity."""

    if set(REQUIRED_WORKFLOW_POLICY_SHA256) != governed_workflows:
        raise GateError(
            "checker-owned workflow policies differ from the route inventory"
        )
    result: dict[str, list[str]] = {}
    for relative in sorted(governed_workflows):
        path = root / relative
        raw, lines = _read_workflow(path)
        if (
            hashlib.sha256(raw).hexdigest()
            != REQUIRED_WORKFLOW_POLICY_SHA256[relative]
        ):
            raise GateError(f"{path}: workflow differs from checker-owned policy")
        result[relative] = lines
    return result
