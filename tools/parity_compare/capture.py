"""Strict replay and authority validation for private live parity captures."""

from __future__ import annotations

import hashlib
import json
import re
from decimal import Decimal, DecimalException, InvalidOperation
from typing import Any

from . import ContractError
from .capture_claims import (
    EXPECTED_MARKERS,
    CaptureClaimError,
    exact_json_equal,
    expected_oracle_claims,
    semantic_sha256,
    trusted_bash_path,
)
from .values import JsonInteger


SHA256 = re.compile(r"^[0-9a-f]{64}$")
METHODS = {
    "oracle": "direct-oracle",
    "github-actions": "github-api-logs",
    "greenlit-release": "retained-evidence",
}
MAX_CAPTURE_BYTES = 8 * 1024 * 1024


class CaptureError(ContractError):
    """A committed raw capture violates its replay or authority contract."""


def _object(value: Any, path: str, fields: set[str]) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise CaptureError(f"{path}: expected object")
    unknown = sorted(set(value) - fields)
    if unknown:
        raise CaptureError(f"{path}.{unknown[0]}: unknown field")
    missing = sorted(fields - set(value))
    if missing:
        raise CaptureError(f"{path}.{missing[0]}: missing field")
    return value


def _strict_pairs(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise CaptureError(f"raw capture: duplicate JSON key {key!r}")
        result[key] = value
    return result


def _decimal(value: str) -> Decimal:
    try:
        number = Decimal(value)
    except InvalidOperation as error:
        raise CaptureError(f"raw capture: invalid JSON number {value!r}") from error
    if not number.is_finite():
        raise CaptureError(f"raw capture: non-finite JSON number {value!r}")
    return number


def _integer(value: str) -> JsonInteger:
    try:
        return JsonInteger(value)
    except InvalidOperation as error:
        raise CaptureError(f"raw capture: invalid JSON integer {value!r}") from error


def _reject_constant(value: str) -> None:
    raise CaptureError(f"raw capture: non-JSON constant {value!r}")


def _load_capture(raw: bytes) -> dict[str, Any]:
    if len(raw) > MAX_CAPTURE_BYTES:
        raise CaptureError("raw capture exceeds the 8 MiB contract limit")
    try:
        value = json.loads(
            raw.decode("utf-8"),
            object_pairs_hook=_strict_pairs,
            parse_float=_decimal,
            parse_int=_integer,
            parse_constant=_reject_constant,
        )
    except UnicodeDecodeError as error:
        raise CaptureError("raw capture is not UTF-8 JSON") from error
    except json.JSONDecodeError as error:
        raise CaptureError(
            f"raw capture is invalid JSON at line {error.lineno}, "
            f"column {error.colno}: {error.msg}"
        ) from error
    except (ValueError, RecursionError) as error:
        raise CaptureError(f"raw capture is invalid JSON: {error}") from error
    return _object(
        value,
        "raw capture",
        {
            "schema_version",
            "case_id",
            "role",
            "capture_method",
            "authority",
            "observation",
        },
    )


def _seed_evidence(observation: dict[str, Any]) -> tuple[Any, list[dict[str, Any]], dict[str, Any]]:
    try:
        job = observation["jobs"][0]
        seed_value = job["steps"][0]["outputs"][0]["value"]
        identities = [
            {"job": job["id"], "step": step["id"]} for step in job["steps"]
        ]
    except (IndexError, KeyError, TypeError) as error:
        message = "raw capture authority cannot bind missing seed job evidence"
        raise CaptureError(message) from error
    return seed_value, identities, job


def _common_authority(
    authority: dict[str, Any], observation: dict[str, Any], repository_id: str,
    source_commit: str,
) -> None:
    source, run = observation["source"], observation["run"]
    common = _object(
        authority["common"],
        "raw capture.authority.common",
        {"repository", "commit", "workflow_sha256", "run_id"},
    )
    if not exact_json_equal(common, {
        "repository": repository_id,
        "commit": source_commit,
        "workflow_sha256": source["workflow_sha256"],
        "run_id": run["id"],
    }):
        raise CaptureError("raw capture authority does not bind trusted source/run")
    seed_value, _, _ = _seed_evidence(observation)
    markers = _object(
        authority["markers"],
        "raw capture.authority.markers",
        {
            "contexts",
            "seed_value",
            "temporary_directory",
            "filesystem_probes",
        },
    )
    if not exact_json_equal(markers, {
        "contexts": observation["contexts"],
        "seed_value": seed_value,
        "temporary_directory": run["temporary_directory"],
        "filesystem_probes": observation["filesystem_probes"],
    }):
        raise CaptureError("raw capture marker authority differs from observations")
    semantic_digest = authority["semantic_sha256"]
    try:
        expected_semantic_digest = semantic_sha256(observation)
    except CaptureClaimError as error:
        raise CaptureError(f"raw capture semantic value is invalid: {error}") from error
    if (
        not isinstance(semantic_digest, str)
        or SHA256.fullmatch(semantic_digest) is None
        or semantic_digest != expected_semantic_digest
    ):
        raise CaptureError("raw capture semantic SHA does not bind the observation")


def _oracle_authority(
    value: Any,
    observation: dict[str, Any],
    source_commit: str,
    workflow_bytes: bytes | None,
) -> None:
    source = observation["source"]
    block = _object(
        value,
        "raw capture.authority.oracle",
        {
            "source_commit",
            "workflow_blob_sha256",
            "run_block_sha256",
            "rendered_verify_sha256",
            "bash_path",
            "process_umask",
            "command_output_sha256",
            "step_exit_codes",
            "log_marker_identities",
        },
    )
    if workflow_bytes is None:
        raise CaptureError(
            "raw oracle authority requires the committed workflow bytes"
        )
    try:
        expected = expected_oracle_claims(workflow_bytes)
        expected["source_commit"] = source_commit
        expected["bash_path"] = trusted_bash_path()
    except CaptureClaimError as error:
        raise CaptureError(f"raw oracle authority cannot be derived: {error}") from error
    _, observed_markers, _ = _seed_evidence(observation)
    if (
        not exact_json_equal(block, expected)
        or source["workflow_sha256"] != expected["workflow_blob_sha256"]
        or not exact_json_equal(observed_markers, EXPECTED_MARKERS)
    ):
        raise CaptureError(
            "raw oracle authority does not bind the trusted workflow execution"
        )


def _github_authority(
    value: Any, observation: dict[str, Any], source_commit: str
) -> None:
    source, producer = observation["source"], observation["producer"]
    block = _object(
        value,
        "raw capture.authority.github-actions",
        {
            "event",
            "head_sha",
            "workflow_sha256",
            "run_attempt",
            "run_url",
            "job_name",
            "job_conclusion",
            "step_records",
            "lifecycle_records",
            "log_marker_identities",
        },
    )
    _, expected_markers, job = _seed_evidence(observation)
    if not exact_json_equal(block, {
        "event": "push",
        "head_sha": source_commit,
        "workflow_sha256": source["workflow_sha256"],
        "run_attempt": producer["run_attempt"],
        "run_url": producer["run_url"],
        "job_name": job["name"],
        "job_conclusion": job["conclusion"],
        "step_records": job["steps"],
        "lifecycle_records": observation["lifecycle"],
        "log_marker_identities": expected_markers,
    }):
        raise CaptureError(
            "raw GitHub authority does not bind API source and result evidence"
        )


def _greenlit_authority(
    value: Any, observation: dict[str, Any], source_commit: str
) -> None:
    source, producer = observation["source"], observation["producer"]
    block = _object(
        value,
        "raw capture.authority.greenlit-release",
        {
            "event",
            "binary_sha256",
            "frozen_workflow_sha256",
            "result_conclusion",
            "result_compatibility",
            "result_assurance",
            "journal_lifecycle",
            "source_commit",
            "build_source_commit",
            "requested_runner",
            "resolved_runner",
            "reported_durations",
        },
    )
    reported = _object(
        block["reported_durations"],
        "raw capture.authority.greenlit-release.reported_durations",
        {"run_elapsed_ms", "job_duration_ms", "step_duration_ms"},
    )
    step_durations = reported["step_duration_ms"]
    observed = [
        observation["run"]["duration_ms"],
        observation["jobs"][0]["duration_ms"],
        *(step["duration_ms"] for step in observation["jobs"][0]["steps"]),
    ]
    reported_values = [
        reported["run_elapsed_ms"],
        reported["job_duration_ms"],
        *(step_durations if isinstance(step_durations, list) else []),
    ]
    try:
        durations_agree = all(
            abs(value - expected) <= 1000
            for value, expected in zip(reported_values, observed)
        )
    except (DecimalException, TypeError):
        durations_agree = False
    if (
        block["event"] != "push"
        or block["binary_sha256"] != producer["binary_sha256"]
        or block["frozen_workflow_sha256"] != source["workflow_sha256"]
        or block["result_conclusion"] != "passed"
        or block["result_compatibility"] != "degraded"
        or block["result_assurance"] != "none"
        or not exact_json_equal(
            block["journal_lifecycle"], observation["lifecycle"]
        )
        or block["source_commit"] != source_commit
        or block["build_source_commit"] != source_commit
        or block["requested_runner"] != producer["runner"]
        or block["resolved_runner"] != "ubuntu-24.04"
        or len(reported_values) != len(observed)
        or any(
            isinstance(value, bool)
            or not isinstance(value, (int, JsonInteger))
            or value < 0
            for value in reported_values
        )
        or not durations_agree
    ):
        raise CaptureError(
            "raw Greenlit authority does not bind release build and retained result"
        )


def validate_capture(
    raw: bytes,
    capture_sha256: str,
    observation: dict[str, Any],
    expected_role: str,
    repository_id: str,
    source_commit: str,
    workflow_bytes: bytes | None = None,
) -> None:
    """Require exact replay and trusted source authority for a capture blob."""
    if expected_role not in METHODS:
        raise CaptureError(f"raw capture has unsupported role {expected_role!r}")
    if not isinstance(raw, bytes):
        raise CaptureError("raw capture must be exact bytes")
    if (
        not isinstance(capture_sha256, str)
        or SHA256.fullmatch(capture_sha256) is None
        or hashlib.sha256(raw).hexdigest() != capture_sha256
    ):
        raise CaptureError("raw capture bytes do not match their exact SHA-256")
    capture = _load_capture(raw)
    method = METHODS[expected_role]
    if (
        capture["schema_version"] != "ParityCaptureV1"
        or capture["case_id"] != observation["case_id"]
        or capture["role"] != expected_role
        or capture["capture_method"] != method
    ):
        raise CaptureError(
            "raw capture schema, case, role, or method does not match its fixed path"
        )
    captured = _object(
        capture["observation"], "raw capture.observation", set(observation)
    )
    replayed = dict(captured)
    captured_producer = replayed.get("producer")
    if not isinstance(captured_producer, dict):
        raise CaptureError("raw capture.observation.producer: expected object")
    producer = dict(captured_producer)
    if producer.get("role") != expected_role:
        raise CaptureError("raw capture producer role does not match capture role")
    if producer.get("capture_method") != method:
        raise CaptureError("raw capture producer contains a conflicting method")
    if "capture_sha256" in producer:
        raise CaptureError(
            "raw capture producer cannot contain its self-referential digest"
        )
    producer["capture_method"] = method
    producer["capture_sha256"] = capture_sha256
    replayed["producer"] = producer
    if not exact_json_equal(replayed, observation):
        raise CaptureError(
            "raw capture does not replay the supplied parity observation exactly"
        )
    if observation["source"]["repository"] != repository_id:
        raise CaptureError(
            "raw capture observation does not name the trusted repository"
        )
    authority = _object(
        capture["authority"],
        "raw capture.authority",
        {"common", "markers", expected_role, "semantic_sha256"},
    )
    _common_authority(authority, replayed, repository_id, source_commit)
    if expected_role == "oracle":
        _oracle_authority(
            authority[expected_role], replayed, source_commit, workflow_bytes
        )
    elif expected_role == "github-actions":
        _github_authority(authority[expected_role], replayed, source_commit)
    else:
        _greenlit_authority(authority[expected_role], replayed, source_commit)
