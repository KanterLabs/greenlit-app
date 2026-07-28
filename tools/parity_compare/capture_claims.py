"""Deterministic claims independently derived from the parity seed source."""

from __future__ import annotations

import hashlib
import json
import stat
from decimal import Decimal
from pathlib import Path
from typing import Any

from .values import JsonInteger


EXPRESSION = "${{ steps.emit.outputs.seed_value }}"
EXPECTED_STEPS = ("emit", "verify")
EXPECTED_MARKERS = [
    {"job": "shell", "step": "emit"},
    {"job": "shell", "step": "verify"},
]
SYSTEM_BASH_PATHS = (Path("/usr/bin/bash"), Path("/bin/bash"), Path("/usr/local/bin/bash"))


class CaptureClaimError(ValueError):
    """A deterministic capture claim cannot be derived or verified."""


def _sha256(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def _canonical_json(value: Any) -> str:
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, Decimal):
        if not value.is_finite():
            raise CaptureClaimError("capture semantic number is non-finite")
        return str(value)
    if isinstance(value, str):
        return json.dumps(value, ensure_ascii=False)
    if isinstance(value, list):
        return "[" + ",".join(_canonical_json(item) for item in value) + "]"
    if isinstance(value, dict):
        if any(not isinstance(key, str) for key in value):
            raise CaptureClaimError(
                "capture semantic object has a non-string key"
            )
        members = (
            f"{json.dumps(key, ensure_ascii=False)}:{_canonical_json(value[key])}"
            for key in sorted(value)
        )
        return "{" + ",".join(members) + "}"
    kind = type(value).__name__
    raise CaptureClaimError(
        f"capture semantic value has unsupported type {kind}"
    )


def semantic_sha256(observation: dict[str, Any]) -> str:
    """Hash the observation without its transport-only producer metadata."""
    semantic = {
        key: value for key, value in observation.items() if key != "producer"
    }
    try:
        raw = _canonical_json(semantic).encode("utf-8")
    except (UnicodeEncodeError, RecursionError) as error:
        raise CaptureClaimError(
            "capture semantic value is not valid bounded UTF-8"
        ) from error
    return _sha256(raw)


def exact_json_equal(left: Any, right: Any) -> bool:
    """Compare JSON values without Python's boolean/number type coercion."""

    if left is None or right is None:
        return left is None and right is None
    if isinstance(left, bool) or isinstance(right, bool):
        return isinstance(left, bool) and isinstance(right, bool) and left == right
    if isinstance(left, (int, Decimal)) or isinstance(right, (int, Decimal)):
        if not isinstance(left, (int, Decimal)) or not isinstance(
            right, (int, Decimal)
        ):
            return False
        left_integer = isinstance(left, (int, JsonInteger))
        right_integer = isinstance(right, (int, JsonInteger))
        if left_integer != right_integer:
            return False
        return Decimal(left) == Decimal(right)
    if isinstance(left, str) or isinstance(right, str):
        return isinstance(left, str) and isinstance(right, str) and left == right
    if isinstance(left, list) or isinstance(right, list):
        return (
            isinstance(left, list)
            and isinstance(right, list)
            and len(left) == len(right)
            and all(
                exact_json_equal(left_item, right_item)
                for left_item, right_item in zip(left, right, strict=True)
            )
        )
    if isinstance(left, dict) or isinstance(right, dict):
        return (
            isinstance(left, dict)
            and isinstance(right, dict)
            and set(left) == set(right)
            and all(exact_json_equal(left[key], right[key]) for key in left)
        )
    return False


def _extract_run_blocks(workflow: bytes) -> tuple[str, str]:
    try:
        lines = workflow.decode("utf-8").splitlines(keepends=True)
    except UnicodeDecodeError as error:
        raise CaptureClaimError("committed parity workflow is not UTF-8") from error
    blocks: list[str] = []
    for step_id in EXPECTED_STEPS:
        header = f"      - id: {step_id}\n"
        indices = [index for index, line in enumerate(lines) if line == header]
        if len(indices) != 1:
            raise CaptureClaimError(
                f"committed parity workflow lacks one exact {step_id!r} step"
            )
        start = indices[0]
        end = next(
            (
                index
                for index in range(start + 1, len(lines))
                if lines[index].startswith("      - id: ")
                or (
                    lines[index].strip()
                    and not lines[index].startswith("        ")
                )
            ),
            len(lines),
        )
        run_lines = [
            index
            for index in range(start + 1, end)
            if lines[index] == "        run: |\n"
        ]
        if len(run_lines) != 1:
            raise CaptureClaimError(
                f"committed parity step {step_id!r} lacks one literal run block"
            )
        block_lines: list[str] = []
        for line in lines[run_lines[0] + 1 : end]:
            if line.strip() and not line.startswith("          "):
                break
            block_lines.append(line[10:] if line.startswith("          ") else "\n")
        block = "".join(block_lines)
        if not block or not block.endswith("\n"):
            raise CaptureClaimError(
                f"committed parity step {step_id!r} run block is unterminated"
            )
        blocks.append(block)
    return blocks[0], blocks[1]


def expected_oracle_claims(workflow: bytes) -> dict[str, Any]:
    """Derive every deterministic oracle execution claim from source bytes."""
    emit, verify = _extract_run_blocks(workflow)
    if verify.count(EXPRESSION) != 2:
        raise CaptureClaimError(
            "committed parity verify block has ambiguous output expressions"
        )
    rendered = verify.replace(EXPRESSION, "greenlit")
    return {
        "workflow_blob_sha256": _sha256(workflow),
        "run_block_sha256": [
            _sha256(emit.encode("utf-8")),
            _sha256(verify.encode("utf-8")),
        ],
        "rendered_verify_sha256": _sha256(rendered.encode("utf-8")),
        "process_umask": "0022",
        "command_output_sha256": _sha256(b"seed_value=greenlit\n"),
        "step_exit_codes": [0, 0],
        "log_marker_identities": EXPECTED_MARKERS,
    }


def trusted_bash_path() -> str:
    """Return the producer's first fixed, independently verified Bash path."""
    for candidate in SYSTEM_BASH_PATHS:
        try:
            resolved = candidate.resolve(strict=True)
            metadata = resolved.stat()
        except OSError:
            continue
        if stat.S_ISREG(metadata.st_mode) and metadata.st_mode & 0o111:
            return str(candidate)
    raise CaptureClaimError("host lacks Bash at a fixed trusted system path")


__all__ = [
    "CaptureClaimError",
    "EXPECTED_MARKERS",
    "exact_json_equal",
    "expected_oracle_claims",
    "semantic_sha256",
    "trusted_bash_path",
]
