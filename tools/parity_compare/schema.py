"""Exact ParityObservationV1 shape and semantic validation."""

from __future__ import annotations

from pathlib import PurePosixPath
from typing import Any, Callable

from . import ContractError
from .lifecycle import (
    LIFECYCLE_KINDS,
    validate_lifecycle_record,
    validate_lifecycle_semantics,
    validate_reported_duration,
)
from .seed import validate_seed_contract
from .values import (
    CASE_ID,
    COMMIT,
    MODE,
    REPOSITORY,
    SHA256,
    load_json_document,
    require_array,
    require_conclusion,
    require_duration,
    require_identifier,
    require_integer,
    require_object,
    require_string,
    require_timestamp,
    validate_identity_records,
)


SCHEMA_VERSION = "ParityObservationV1"
PRODUCER_ROLES = frozenset({"oracle", "github-actions", "greenlit-release"})
ProvenanceValidator = Callable[[dict[str, Any]], None]


def _value_record(value: Any, path: str) -> str:
    record = require_object(value, path, {"id", "value"})
    return require_identifier(record["id"], f"{path}.id")


def _step(value: Any, path: str) -> str:
    record = require_object(
        value,
        path,
        {"id", "name", "outcome", "conclusion", "duration_ms", "outputs"},
    )
    identity = require_identifier(record["id"], f"{path}.id")
    require_string(record["name"], f"{path}.name")
    outcome = require_conclusion(record["outcome"], f"{path}.outcome")
    conclusion = require_conclusion(record["conclusion"], f"{path}.conclusion")
    if outcome != conclusion and (outcome, conclusion) != ("failure", "success"):
        raise ContractError(
            f"{path}: outcome and conclusion differ outside continue-on-error semantics"
        )
    require_duration(record["duration_ms"], f"{path}.duration_ms")
    outputs = require_array(record["outputs"], f"{path}.outputs")
    validate_identity_records(outputs, f"{path}.outputs", _value_record, sorted_ids=True)
    return identity


def _job(value: Any, path: str) -> str:
    record = require_object(
        value,
        path,
        {"id", "name", "conclusion", "duration_ms", "outputs", "steps"},
    )
    identity = require_identifier(record["id"], f"{path}.id")
    require_string(record["name"], f"{path}.name")
    require_conclusion(record["conclusion"], f"{path}.conclusion")
    require_duration(record["duration_ms"], f"{path}.duration_ms")
    outputs = require_array(record["outputs"], f"{path}.outputs")
    validate_identity_records(outputs, f"{path}.outputs", _value_record, sorted_ids=True)
    steps = require_array(record["steps"], f"{path}.steps")
    if not steps:
        raise ContractError(f"{path}.steps: missing step observation")
    validate_identity_records(steps, f"{path}.steps", _step, sorted_ids=False)
    return identity


def _stable_relative_path(value: Any, path: str) -> str:
    text = require_string(value, path)
    parts = PurePosixPath(text).parts
    if (
        text == "."
        or text.startswith("/")
        or "\\" in text
        or "//" in text
        or any(ord(character) < 32 or ord(character) == 127 for character in text)
        or PurePosixPath(text).as_posix() != text
        or any(part in {"", ".", ".."} for part in parts)
    ):
        raise ContractError(f"{path}: expected stable relative logical path")
    return text


def _filesystem_probe(value: Any, path: str) -> str:
    record = require_object(
        value, path, {"id", "logical_path", "kind", "exists", "mode", "sha256"}
    )
    identity = require_identifier(record["id"], f"{path}.id")
    _stable_relative_path(record["logical_path"], f"{path}.logical_path")
    kind = require_string(record["kind"], f"{path}.kind")
    if kind not in {"file", "directory", "symlink", "absent"}:
        raise ContractError(f"{path}.kind: unknown filesystem probe kind {kind!r}")
    exists, mode, digest = record["exists"], record["mode"], record["sha256"]
    if not isinstance(exists, bool):
        raise ContractError(f"{path}.exists: expected boolean")
    if mode is not None and (not isinstance(mode, str) or MODE.fullmatch(mode) is None):
        raise ContractError(f"{path}.mode: expected null or four-digit octal mode")
    if digest is not None and (
        not isinstance(digest, str) or SHA256.fullmatch(digest) is None
    ):
        raise ContractError(f"{path}.sha256: expected null or lowercase SHA-256")
    if not exists and (kind != "absent" or mode is not None or digest is not None):
        raise ContractError(
            f"{path}: absent probe requires kind 'absent' and null evidence"
        )
    if exists and (kind == "absent" or mode is None):
        raise ContractError(f"{path}: existing probe requires a kind and observed mode")
    if exists and kind in {"file", "symlink"} and digest is None:
        raise ContractError(f"{path}.sha256: existing {kind} requires SHA-256 evidence")
    if exists and kind == "directory" and digest is not None:
        raise ContractError(f"{path}.sha256: directory probe must use null sha256")
    return identity


def _resource_finding(value: Any, path: str) -> str:
    record = require_object(value, path, {"id", "category", "outcome", "detail"})
    identity = require_identifier(record["id"], f"{path}.id")
    require_identifier(record["category"], f"{path}.category")
    require_string(record["outcome"], f"{path}.outcome")
    if not isinstance(record["detail"], str):
        raise ContractError(f"{path}.detail: expected string")
    return identity


def _validate_probe_topology(probes: list[dict[str, Any]]) -> None:
    by_path: dict[str, tuple[int, dict[str, Any]]] = {}
    for index, probe in enumerate(probes):
        logical_path = probe["logical_path"]
        if logical_path in by_path:
            raise ContractError(
                f"$.filesystem_probes[{index}].logical_path: duplicate logical path"
            )
        by_path[logical_path] = (index, probe)
    for index, probe in enumerate(probes):
        if not probe["exists"]:
            continue
        for parent in PurePosixPath(probe["logical_path"]).parents:
            parent_record = by_path.get(parent.as_posix())
            if parent_record is None:
                continue
            _, ancestor = parent_record
            if not ancestor["exists"] or ancestor["kind"] != "directory":
                raise ContractError(
                    f"$.filesystem_probes[{index}]: existing child has a "
                    "non-directory or absent observed ancestor"
                )


def _dynamic_port(value: Any, path: str) -> str:
    record = require_object(
        value, path, {"id", "container_port", "host_port", "protocol"}
    )
    identity = require_identifier(record["id"], f"{path}.id")
    require_integer(record["container_port"], f"{path}.container_port", 1, 65535)
    require_integer(record["host_port"], f"{path}.host_port", 1, 65535)
    if record["protocol"] not in {"tcp", "udp"}:
        raise ContractError(f"{path}.protocol: expected 'tcp' or 'udp'")
    return identity


def _source(value: Any) -> dict[str, Any]:
    source = require_object(
        value,
        "$.source",
        {"repository", "commit", "workflow_path", "workflow_sha256"},
    )
    repository = require_string(source["repository"], "$.source.repository")
    if REPOSITORY.fullmatch(repository) is None:
        raise ContractError("$.source.repository: expected canonical owner/repository")
    commit = require_string(source["commit"], "$.source.commit")
    if COMMIT.fullmatch(commit) is None:
        raise ContractError("$.source.commit: expected full lowercase Git commit")
    workflow_path = _stable_relative_path(
        source["workflow_path"], "$.source.workflow_path"
    )
    parts = PurePosixPath(workflow_path).parts
    if (
        len(parts) != 3
        or parts[:2] != (".github", "workflows")
        or PurePosixPath(workflow_path).suffix not in {".yml", ".yaml"}
        or PurePosixPath(workflow_path).name in {".yml", ".yaml"}
    ):
        raise ContractError(
            "$.source.workflow_path: expected one .github/workflows YAML file"
        )
    digest = require_string(source["workflow_sha256"], "$.source.workflow_sha256")
    if SHA256.fullmatch(digest) is None:
        raise ContractError("$.source.workflow_sha256: expected lowercase SHA-256")
    return source


def _producer(
    value: Any, source: dict[str, Any], run: dict[str, Any]
) -> dict[str, Any]:
    producer = require_object(
        value,
        "$.producer",
        {
            "role",
            "repository",
            "runner",
            "run_id",
            "run_attempt",
            "run_url",
            "binary_sha256",
            "capture_method",
            "capture_sha256",
        },
    )
    role = require_string(producer["role"], "$.producer.role")
    if role not in PRODUCER_ROLES:
        raise ContractError(f"$.producer.role: unknown producer role {role!r}")
    if producer["repository"] != source["repository"]:
        raise ContractError("$.producer.repository: must equal $.source.repository")
    require_identifier(producer["runner"], "$.producer.runner")
    if require_identifier(producer["run_id"], "$.producer.run_id") != run["id"]:
        raise ContractError("$.producer.run_id: must equal $.run.id")
    attempt = require_integer(producer["run_attempt"], "$.producer.run_attempt", 1)
    capture = require_string(producer["capture_sha256"], "$.producer.capture_sha256")
    if SHA256.fullmatch(capture) is None:
        raise ContractError("$.producer.capture_sha256: expected raw-capture SHA-256")
    method = require_string(producer["capture_method"], "$.producer.capture_method")
    run_url, binary = producer["run_url"], producer["binary_sha256"]
    if role == "github-actions":
        expected = (
            f"https://github.com/{source['repository']}/actions/runs/{run['id']}"
        )
        if method != "github-api-logs" or run_url != expected or binary is not None:
            raise ContractError("$.producer: invalid github-actions producer evidence")
    elif role == "oracle":
        if method != "direct-oracle" or run_url is not None or binary is not None:
            raise ContractError("$.producer: invalid oracle producer evidence")
        if attempt != 1:
            raise ContractError("$.producer.run_attempt: oracle producer must use 1")
    else:
        if method != "retained-evidence" or run_url is not None:
            raise ContractError("$.producer: invalid greenlit-release producer evidence")
        if attempt != 1:
            raise ContractError(
                "$.producer.run_attempt: greenlit-release producer must use 1"
            )
        if not isinstance(binary, str) or SHA256.fullmatch(binary) is None:
            raise ContractError(
                "$.producer.binary_sha256: greenlit-release requires binary SHA-256"
            )
    return producer


def validate_observation(
    value: Any, provenance_validator: ProvenanceValidator | None = None
) -> dict[str, Any]:
    """Validate one observation, then apply optional immutable-provenance checks."""
    root = require_object(
        value,
        "$",
        {
            "schema_version",
            "case_id",
            "producer",
            "source",
            "run",
            "contexts",
            "outputs",
            "jobs",
            "lifecycle",
            "filesystem_probes",
            "resource_security_findings",
            "dynamic_ports",
        },
    )
    if root["schema_version"] != SCHEMA_VERSION:
        raise ContractError(f"$.schema_version: expected {SCHEMA_VERSION!r}")
    case_id = require_string(root["case_id"], "$.case_id")
    if CASE_ID.fullmatch(case_id) is None:
        raise ContractError(f"$.case_id: invalid case identity {case_id!r}")
    source = _source(root["source"])
    run = require_object(
        root["run"],
        "$.run",
        {
            "id",
            "started_at",
            "completed_at",
            "duration_ms",
            "conclusion",
            "temporary_directory",
        },
    )
    require_identifier(run["id"], "$.run.id")
    started = require_timestamp(run["started_at"], "$.run.started_at")
    completed = require_timestamp(run["completed_at"], "$.run.completed_at")
    if completed < started:
        raise ContractError("$.run.completed_at: run completed before it started")
    validate_reported_duration(
        run["duration_ms"], started, completed, "$.run.duration_ms"
    )
    require_conclusion(run["conclusion"], "$.run.conclusion")
    temporary = require_string(run["temporary_directory"], "$.run.temporary_directory")
    if not temporary.startswith("/"):
        raise ContractError("$.run.temporary_directory: expected absolute path")
    _producer(root["producer"], source, run)
    collections = (
        ("contexts", _value_record, True),
        ("outputs", _value_record, True),
        ("jobs", _job, True),
        ("lifecycle", validate_lifecycle_record, False),
        ("filesystem_probes", _filesystem_probe, True),
        ("resource_security_findings", _resource_finding, True),
        ("dynamic_ports", _dynamic_port, True),
    )
    for name, validator, sorted_ids in collections:
        records = require_array(root[name], f"$.{name}")
        if name in {"jobs", "lifecycle"} and not records:
            raise ContractError(f"$.{name}: missing observation")
        validate_identity_records(
            records, f"$.{name}", validator, sorted_ids=sorted_ids
        )
    for index, event in enumerate(root["lifecycle"]):
        if event["sequence"] != index + 1:
            raise ContractError(
                f"$.lifecycle[{index}].sequence: expected {index + 1}"
            )
    _validate_probe_topology(root["filesystem_probes"])
    validate_lifecycle_semantics(root, started, completed)
    validate_seed_contract(root)
    if provenance_validator is not None:
        provenance_validator(root)
    return root


__all__ = [
    "ContractError",
    "LIFECYCLE_KINDS",
    "PRODUCER_ROLES",
    "ProvenanceValidator",
    "SCHEMA_VERSION",
    "load_json_document",
    "validate_observation",
]
