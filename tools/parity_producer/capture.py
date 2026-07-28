"""Canonical sanitized capture publication and replay."""

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
    SHA256,
    ProducerError,
    load_json_bytes,
    require_fields,
    require_object,
    require_string,
    sha256_bytes,
)
from parity_producer.capture_git import committed_regular_bytes, verify_source_blob
from parity_producer.live_root import validate_live_roots
from parity_producer.secure_output import write_bytes_beneath


CAPTURE_DIRECTORY = Path("fixtures/stabilization/parity/captures")
OBSERVATION_DIRECTORY = Path("fixtures/stabilization/parity")
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
    """Write the role's fixed capture and observation, binding them by digest."""
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


def verify(
    checkout: Path,
    role_value: str,
    trusted_repository: str,
    trusted_source_commit: str,
) -> dict[str, Any]:
    """Replay a tracked fixed capture and require byte-exact observation equality."""
    checkout = checkout.resolve()
    _validate_trusted_inputs(trusted_repository, trusted_source_commit)
    role = _role(role_value)
    capture_path = _fixed_capture_path(checkout, role)
    observation_path = _fixed_observation_path(checkout, role)
    capture_bytes = committed_regular_bytes(
        checkout, capture_path, f"{role} parity capture"
    )
    observation_bytes = committed_regular_bytes(
        checkout, observation_path, f"{role} parity observation"
    )
    capture = require_object(
        load_json_bytes(capture_bytes, f"{role} parity capture"),
        f"{role} parity capture",
    )
    replayed = replay_capture(capture, sha256_bytes(capture_bytes), role)
    source = require_object(replayed.get("source"), "replayed source")
    producer = require_object(replayed.get("producer"), "replayed producer")
    if (
        source.get("repository") != trusted_repository
        or producer.get("repository") != trusted_repository
        or source.get("commit") != trusted_source_commit
    ):
        raise ProducerError("replayed capture differs from trusted source identity")
    workflow_bytes = verify_source_blob(checkout, trusted_source_commit, source)
    _validate_oracle_workflow_authority(role, capture["authority"], workflow_bytes)
    expected = require_object(
        load_json_bytes(observation_bytes, f"{role} parity observation"),
        f"{role} parity observation",
    )
    try:
        validate_observation(expected)
        validate_capture(
            capture_bytes,
            sha256_bytes(capture_bytes),
            expected,
            role,
            trusted_repository,
            trusted_source_commit,
            workflow_bytes,
        )
    except ContractError as error:
        raise ProducerError(f"{role} capture authority is invalid: {error}") from error
    if replayed != expected:
        raise ProducerError(
            f"{role} observation drifted from its committed canonical capture"
        )
    return replayed


def replay_capture(
    capture: dict[str, Any],
    capture_sha256: str,
    expected_role: str,
) -> dict[str, Any]:
    """Regenerate one observation solely from a sanitized canonical capture."""
    require_fields(
        capture,
        "parity capture",
        {
            "schema_version",
            "case_id",
            "role",
            "capture_method",
            "authority",
            "observation",
        },
        allow_extra=False,
    )
    if capture["schema_version"] != CAPTURE_VERSION or capture["case_id"] != CASE_ID:
        raise ProducerError("parity capture has an unknown schema or case identity")
    role = _role(capture["role"])
    if role != expected_role or capture["capture_method"] != METHODS[role]:
        raise ProducerError("parity capture role or method does not match its fixed path")
    authority = require_object(capture["authority"], "capture authority")
    if not authority:
        raise ProducerError("parity capture authority is empty")
    observation = copy.deepcopy(
        require_object(capture["observation"], "captured observation")
    )
    producer = require_object(observation.get("producer"), "captured producer")
    if producer.get("role") != role:
        raise ProducerError("captured producer role does not match capture role")
    producer["capture_method"] = capture["capture_method"]
    if SHA256.fullmatch(capture_sha256) is None:
        raise ProducerError("capture digest is malformed")
    producer["capture_sha256"] = capture_sha256
    _validate_authority_binding(role, authority, observation)
    return observation


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


def _validate_authority_binding(
    role: str,
    authority: dict[str, Any],
    observation: dict[str, Any],
) -> None:
    producer = require_object(observation["producer"], "captured producer")
    source = require_object(observation["source"], "captured source")
    run = require_object(observation["run"], "captured run")
    common = require_object(authority.get("common"), "capture common authority")
    if (
        common.get("repository") != source.get("repository")
        or common.get("commit") != source.get("commit")
        or common.get("workflow_sha256") != source.get("workflow_sha256")
        or common.get("run_id") != run.get("id")
    ):
        raise ProducerError("capture authority does not bind source and run identities")
    markers = require_object(authority.get("markers"), "capture marker authority")
    contexts = observation.get("contexts")
    probe = observation.get("filesystem_probes")
    if markers.get("contexts") != contexts or markers.get("filesystem_probes") != probe:
        raise ProducerError("capture marker authority does not bind observed probes")
    if markers.get("seed_value") != "greenlit":
        raise ProducerError("capture marker authority does not bind the seed output")
    if authority.get("semantic_sha256") != _semantic_sha256(observation):
        raise ProducerError("capture semantic authority digest does not match observation")
    role_authority = require_object(authority.get(role), f"{role} capture authority")
    if role == "greenlit-release" and (
        role_authority.get("event") != "push"
        or role_authority.get("binary_sha256") != producer.get("binary_sha256")
        or role_authority.get("result_conclusion") != "passed"
        or role_authority.get("source_commit") != source.get("commit")
        or role_authority.get("build_source_commit") != source.get("commit")
    ):
        raise ProducerError(
            "Greenlit release capture authority does not bind release result identity"
        )
    if role == "github-actions" and (
        role_authority.get("event") != "push"
        or role_authority.get("head_sha") != source.get("commit")
        or role_authority.get("workflow_sha256") != source.get("workflow_sha256")
        or role_authority.get("run_attempt") != producer.get("run_attempt")
        or role_authority.get("run_url") != producer.get("run_url")
    ):
        raise ProducerError("GitHub capture authority does not bind API run identity")
    if role == "oracle":
        block_digests = role_authority.get("run_block_sha256")
        if (
            not isinstance(block_digests, list)
            or len(block_digests) != 2
            or any(SHA256.fullmatch(value) is None for value in block_digests)
        ):
            raise ProducerError("oracle capture lacks two workflow run-block bindings")
        if (
            role_authority.get("source_commit") != source.get("commit")
            or role_authority.get("workflow_blob_sha256")
            != source.get("workflow_sha256")
        ):
            raise ProducerError("oracle capture is not bound to trusted workflow source")


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


def _fixed_capture_path(checkout: Path, role: str) -> Path:
    return checkout / CAPTURE_DIRECTORY / f"{CASE_ID}-{role}.json"


def _fixed_observation_path(checkout: Path, role: str) -> Path:
    return checkout / OBSERVATION_DIRECTORY / f"seed-{role}.json"


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
