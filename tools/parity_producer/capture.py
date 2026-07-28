"""Canonical sanitized live-capture publication."""

from __future__ import annotations

import copy
import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from parity_compare import ContractError
from parity_compare.capture import validate_capture
from parity_compare.schema import validate_observation
from parity_producer.common import (
    AUTHORITATIVE_REPOSITORY,
    CAPTURE_VERSION,
    CASE_ID,
    COMMIT,
    ProducerError,
    require_object,
    require_string,
    sha256_bytes,
)
from parity_producer.capture_git import verify_source_blob
from parity_producer.live_root import validate_live_roots
from parity_producer.secure_output import write_bytes_beneath


ROLES = {"oracle", "github-actions", "greenlit-release"}
METHODS = {
    "oracle": "direct-oracle",
    "github-actions": "github-api-logs",
    "greenlit-release": "retained-evidence",
}


@dataclass(frozen=True)
class Production:
    """One unsealed observation and the sanitized authority fields it consumed."""

    observation: dict[str, Any]
    authority: dict[str, Any]


def publish(
    production: Production,
    checkout: Path,
    output_root: Path,
    trusted_repository: str,
    trusted_source_commit: str,
) -> tuple[Path, Path]:
    """Write the role's live capture and observation, binding them by digest."""
    _validate_trusted_inputs(trusted_repository, trusted_source_commit)
    checkout, output_root = validate_live_roots(
        checkout, output_root, trusted_source_commit
    )
    observation = copy.deepcopy(production.observation)
    producer = require_object(observation.get("producer"), "producer")
    source = require_object(observation.get("source"), "source")
    if (
        producer.get("repository") != trusted_repository
        or source.get("repository") != trusted_repository
        or source.get("commit") != trusted_source_commit
    ):
        raise ProducerError("produced observation differs from trusted source identity")
    workflow_bytes = verify_source_blob(checkout, trusted_source_commit, source)
    role = _role(producer.get("role"))
    method = METHODS[role]
    producer["capture_method"] = method
    producer.pop("capture_sha256", None)
    capture = _capture_document(observation, production.authority, method)
    _validate_oracle_workflow_authority(role, production.authority, workflow_bytes)
    capture_path = output_root / "captures" / f"{CASE_ID}-{role}.json"
    capture_bytes = _capture_bytes(capture)
    capture_sha256 = sha256_bytes(capture_bytes)
    producer["capture_sha256"] = capture_sha256
    try:
        validate_observation(observation)
        validate_capture(
            capture_bytes,
            capture_sha256,
            observation,
            role,
            trusted_repository,
            trusted_source_commit,
            workflow_bytes,
        )
    except ContractError as error:
        raise ProducerError(f"produced capture violates its authority contract: {error}") from error
    write_bytes_beneath(
        output_root,
        capture_path.relative_to(output_root),
        capture_bytes,
        create_parent_leaf=True,
        created_directory_mode=0o700,
    )
    observation_path = output_root / f"seed-{role}.json"
    observation_bytes = (
        json.dumps(observation, indent=2, ensure_ascii=False, allow_nan=False) + "\n"
    ).encode("utf-8")
    write_bytes_beneath(
        output_root,
        observation_path.relative_to(output_root),
        observation_bytes,
        create_parent_leaf=False,
    )
    return capture_path, observation_path


def _capture_document(
    observation: dict[str, Any],
    authority: dict[str, Any],
    method: str,
) -> dict[str, Any]:
    role = _role(observation["producer"]["role"])
    return {
        "schema_version": CAPTURE_VERSION,
        "case_id": CASE_ID,
        "role": role,
        "capture_method": method,
        "authority": authority,
        "observation": observation,
    }


def authority_envelope(
    observation: dict[str, Any],
    role: str,
    role_authority: dict[str, Any],
) -> dict[str, Any]:
    """Create the common sanitized binding around role-specific raw fields."""
    source = observation["source"]
    run = observation["run"]
    return {
        "common": {
            "repository": source["repository"],
            "commit": source["commit"],
            "workflow_sha256": source["workflow_sha256"],
            "run_id": run["id"],
        },
        "markers": {
            "contexts": observation["contexts"],
            "seed_value": observation["jobs"][0]["steps"][0]["outputs"][0]["value"],
            "temporary_directory": run["temporary_directory"],
            "filesystem_probes": observation["filesystem_probes"],
        },
        role: role_authority,
        "semantic_sha256": _semantic_sha256(observation),
    }


def _semantic_sha256(observation: dict[str, Any]) -> str:
    semantic = {
        key: value
        for key, value in observation.items()
        if key not in {"producer"}
    }
    raw = json.dumps(
        semantic,
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
        allow_nan=False,
    ).encode("utf-8")
    return sha256_bytes(raw)


def _capture_bytes(value: dict[str, Any]) -> bytes:
    return (
        json.dumps(
            value,
            sort_keys=True,
            separators=(",", ":"),
            ensure_ascii=False,
            allow_nan=False,
        )
        + "\n"
    ).encode("utf-8")


def _role(value: Any) -> str:
    role = require_string(value, "producer role")
    if role not in ROLES:
        raise ProducerError(f"unknown parity producer role {role!r}")
    return role


def _validate_trusted_inputs(repository: str, source_commit: str) -> None:
    if repository != AUTHORITATIVE_REPOSITORY:
        raise ProducerError(
            f"repository identity must be {AUTHORITATIVE_REPOSITORY!r}"
        )
    if COMMIT.fullmatch(source_commit) is None:
        raise ProducerError("trusted source commit must be full lowercase SHA")


def _validate_oracle_workflow_authority(
    role: str,
    authority: dict[str, Any],
    workflow_bytes: bytes,
) -> None:
    if role != "oracle":
        return
    from parity_producer.oracle_workflow import EXPRESSION, extract_run_blocks

    role_authority = require_object(
        authority.get("oracle"), "oracle capture authority"
    )
    blocks = extract_run_blocks(workflow_bytes)
    expected_blocks = [sha256_bytes(block.encode("utf-8")) for block in blocks]
    rendered = blocks[1].replace(EXPRESSION, "greenlit")
    expected_markers = [
        {"job": "shell", "step": "emit"},
        {"job": "shell", "step": "verify"},
    ]
    if (
        role_authority.get("run_block_sha256") != expected_blocks
        or role_authority.get("rendered_verify_sha256")
        != sha256_bytes(rendered.encode("utf-8"))
        or role_authority.get("command_output_sha256")
        != sha256_bytes(b"seed_value=greenlit\n")
        or role_authority.get("step_exit_codes") != [0, 0]
        or role_authority.get("log_marker_identities") != expected_markers
        or role_authority.get("process_umask") != "0022"
    ):
        raise ProducerError(
            "oracle capture authority is not recomputable from committed run blocks"
        )
