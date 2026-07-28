"""Immutable source and producer-evidence validation for parity observations."""

from __future__ import annotations

import hashlib
import re
from pathlib import Path, PurePosixPath
from typing import Any

from . import ContractError
from .binary import validate_release_binary
from .capture import validate_capture
from .live_capture import LiveCaptureRootIdentity, read_live_capture
from .repository import RepositoryIdentity, git_output
from .values import JsonInteger
from .workflow import validate_seed_workflow


COMMIT = re.compile(r"^[0-9a-f]{40}$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")
REPOSITORY_ID = re.compile(
    r"^[A-Za-z0-9](?:[A-Za-z0-9_.-]*[A-Za-z0-9])?/"
    r"[A-Za-z0-9](?:[A-Za-z0-9_.-]*[A-Za-z0-9])?$"
)
IDENTIFIER = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._:/-]*$")
CASE_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
ROLES = ("oracle", "github-actions", "greenlit-release")


class ProvenanceError(ContractError):
    """An immutable provenance contract violation."""


def _object(value: Any, path: str, fields: set[str]) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ProvenanceError(f"{path}: expected object")
    unknown = sorted(set(value) - fields)
    if unknown:
        raise ProvenanceError(f"{path}.{unknown[0]}: unknown field")
    missing = sorted(fields - set(value))
    if missing:
        raise ProvenanceError(f"{path}.{missing[0]}: missing field")
    return value


def _string(value: Any, path: str) -> str:
    if not isinstance(value, str) or not value:
        raise ProvenanceError(f"{path}: expected non-empty string")
    return value


def _identifier(value: Any, path: str) -> str:
    text = _string(value, path)
    if IDENTIFIER.fullmatch(text) is None:
        raise ProvenanceError(f"{path}: invalid identifier {text!r}")
    return text


def _positive_integer(value: Any, path: str) -> int | JsonInteger:
    if (
        isinstance(value, bool)
        or not isinstance(value, (int, JsonInteger))
        or value < 1
    ):
        raise ProvenanceError(f"{path}: expected positive integer")
    return value


def committed_blob(
    repository: RepositoryIdentity,
    revision: str,
    relative_path: str,
    field_path: str,
) -> bytes:
    """Read one committed regular, non-symlink file from a Git tree."""
    listing = git_output(
        repository,
        "ls-tree",
        "-z",
        "--full-tree",
        revision,
        "--",
        relative_path,
    )
    entries = [entry for entry in listing.split(b"\0") if entry]
    if len(entries) != 1 or b"\t" not in entries[0]:
        raise ProvenanceError(f"{field_path}: committed regular file is missing")
    metadata, raw_path = entries[0].split(b"\t", 1)
    fields = metadata.split()
    try:
        listed_path = raw_path.decode("utf-8", errors="strict")
        object_id = fields[2].decode("ascii", errors="strict")
    except (IndexError, UnicodeDecodeError) as error:
        raise ProvenanceError(
            f"{field_path}: malformed committed-file evidence"
        ) from error
    if (
        listed_path != relative_path
        or len(fields) != 3
        or fields[0] not in {b"100644", b"100755"}
        or fields[1] != b"blob"
        or re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", object_id) is None
    ):
        raise ProvenanceError(
            f"{field_path}: expected committed regular non-symlink file"
        )
    return git_output(repository, "cat-file", "blob", object_id)


def _validate_workflow_path(value: Any) -> str:
    workflow_path = _string(value, "$.source.workflow_path")
    posix_path = PurePosixPath(workflow_path)
    if (
        posix_path.is_absolute()
        or "\\" in workflow_path
        or str(posix_path) != workflow_path
        or any(ord(character) < 32 for character in workflow_path)
        or len(posix_path.parts) != 3
        or posix_path.parts[:2] != (".github", "workflows")
        or posix_path.suffix not in {".yml", ".yaml"}
        or posix_path.name in {".yml", ".yaml"}
    ):
        raise ProvenanceError(
            "$.source.workflow_path: expected one direct "
            ".github/workflows/*.yml or *.yaml path"
        )
    return workflow_path


def validate_source(
    value: Any,
    repository: RepositoryIdentity,
    repository_id: str,
    source_commit: str,
) -> dict[str, Any]:
    """Validate source identity against trusted CLI inputs and committed bytes."""
    source = _object(
        value,
        "$.source",
        {"repository", "commit", "workflow_path", "workflow_sha256"},
    )
    if REPOSITORY_ID.fullmatch(repository_id) is None:
        raise ProvenanceError(
            "trusted repository ID must be canonical owner/repository"
        )
    observed_repository = _string(source["repository"], "$.source.repository")
    if observed_repository != repository_id:
        raise ProvenanceError(
            "$.source.repository: does not match the trusted repository ID"
        )
    if COMMIT.fullmatch(source_commit) is None:
        raise ProvenanceError(
            "trusted source commit must be a full lowercase 40-character commit"
        )
    observed_commit = _string(source["commit"], "$.source.commit")
    if observed_commit != source_commit:
        raise ProvenanceError(
            "$.source.commit: does not match the trusted source commit"
        )
    if repository.head != source_commit:
        raise ProvenanceError(
            "$.source.commit: trusted source commit must equal the bound "
            "repository HEAD"
        )
    workflow_path = _validate_workflow_path(source["workflow_path"])
    workflow_sha256 = _string(
        source["workflow_sha256"], "$.source.workflow_sha256"
    )
    if SHA256.fullmatch(workflow_sha256) is None:
        raise ProvenanceError(
            "$.source.workflow_sha256: expected lowercase SHA-256"
        )
    workflow = committed_blob(
        repository, source_commit, workflow_path, "$.source.workflow_path"
    )
    if hashlib.sha256(workflow).hexdigest() != workflow_sha256:
        raise ProvenanceError(
            "$.source.workflow_sha256: does not match the committed workflow"
        )
    return source


def _validate_role_contract(
    producer: dict[str, Any], source: dict[str, Any], role: str, run_id: str
) -> None:
    attempt = producer["run_attempt"]
    run_url = producer["run_url"]
    binary_sha256 = producer["binary_sha256"]
    method = producer["capture_method"]
    if role == "oracle":
        if method != "direct-oracle":
            raise ProvenanceError(
                "$.producer.capture_method: oracle requires 'direct-oracle'"
            )
        if attempt != 1:
            raise ProvenanceError("$.producer.run_attempt: oracle requires 1")
        if run_url is not None:
            raise ProvenanceError("$.producer.run_url: oracle requires null")
        if binary_sha256 is not None:
            raise ProvenanceError(
                "$.producer.binary_sha256: oracle requires null"
            )
        return
    if role == "github-actions":
        if re.fullmatch(r"[1-9][0-9]*", run_id) is None:
            raise ProvenanceError(
                "$.producer.run_id: github-actions requires a positive numeric run ID"
            )
        if method != "github-api-logs":
            raise ProvenanceError(
                "$.producer.capture_method: github-actions requires "
                "'github-api-logs'"
            )
        expected_url = (
            f"https://github.com/{source['repository']}/actions/runs/{run_id}"
        )
        if run_url != expected_url:
            raise ProvenanceError(
                "$.producer.run_url: github-actions URL must identify "
                "the exact run"
            )
        if binary_sha256 is not None:
            raise ProvenanceError(
                "$.producer.binary_sha256: github-actions requires null"
            )
        return
    if method != "retained-evidence":
        raise ProvenanceError(
            "$.producer.capture_method: greenlit-release requires "
            "'retained-evidence'"
        )
    if attempt != 1:
        raise ProvenanceError(
            "$.producer.run_attempt: greenlit-release requires 1"
        )
    if run_url is not None:
        raise ProvenanceError(
            "$.producer.run_url: greenlit-release requires null"
        )
    if not isinstance(binary_sha256, str) or SHA256.fullmatch(binary_sha256) is None:
        raise ProvenanceError(
            "$.producer.binary_sha256: greenlit-release requires a "
            "release-binary SHA-256"
        )


def validate_producer(
    value: Any,
    run: Any,
    source: dict[str, Any],
    observation: dict[str, Any],
    capture_root: LiveCaptureRootIdentity,
    repository_id: str,
    source_commit: str,
    case_id: Any,
    expected_role: str,
    workflow: bytes,
) -> dict[str, Any]:
    """Validate role-bound producer metadata and its private live capture."""
    if expected_role not in ROLES:
        raise ProvenanceError(f"unsupported expected producer role {expected_role!r}")
    if not isinstance(case_id, str) or CASE_ID.fullmatch(case_id) is None:
        raise ProvenanceError("$.case_id: invalid capture-path identity")
    run_object = _object(run, "$.run", set(run) if isinstance(run, dict) else set())
    if "id" not in run_object:
        raise ProvenanceError("$.run.id: missing field")
    producer = _object(
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
    role = _string(producer["role"], "$.producer.role")
    if role != expected_role:
        raise ProvenanceError(
            f"$.producer.role: expected position-bound role {expected_role!r}"
        )
    if producer["repository"] != source["repository"]:
        raise ProvenanceError(
            "$.producer.repository: must equal $.source.repository"
        )
    _identifier(producer["runner"], "$.producer.runner")
    run_id = _identifier(producer["run_id"], "$.producer.run_id")
    observed_run_id = _identifier(run_object["id"], "$.run.id")
    if run_id != observed_run_id:
        raise ProvenanceError("$.producer.run_id: must equal $.run.id")
    _positive_integer(producer["run_attempt"], "$.producer.run_attempt")
    capture_sha256 = producer["capture_sha256"]
    if not isinstance(capture_sha256, str) or SHA256.fullmatch(capture_sha256) is None:
        raise ProvenanceError(
            "$.producer.capture_sha256: expected lowercase raw-capture SHA-256"
        )
    capture = read_live_capture(capture_root, case_id, expected_role)
    if hashlib.sha256(capture).hexdigest() != capture_sha256:
        raise ProvenanceError(
            "$.producer.capture_sha256: does not match the private live capture"
        )
    _validate_role_contract(producer, source, role, run_id)
    validate_capture(
        capture,
        capture_sha256,
        observation,
        expected_role,
        repository_id,
        source_commit,
        workflow,
    )
    return producer


def validate_provenance(
    observation: Any,
    repository: RepositoryIdentity,
    capture_root: LiveCaptureRootIdentity,
    repository_id: str,
    source_commit: str,
    expected_role: str,
    greenlit_binary: Path,
) -> tuple[dict[str, Any], dict[str, Any]]:
    """Validate and return the observation's source and producer objects."""
    root = _object(
        observation,
        "$",
        set(observation) if isinstance(observation, dict) else set(),
    )
    for field in ("source", "producer", "run", "case_id"):
        if field not in root:
            raise ProvenanceError(f"$.{field}: missing field")
    source = validate_source(
        root["source"], repository, repository_id, source_commit
    )
    workflow = committed_blob(
        repository, source_commit, source["workflow_path"], "$.source.workflow_path"
    )
    validate_seed_workflow(root, workflow)
    producer = validate_producer(
        root["producer"],
        root["run"],
        source,
        root,
        capture_root,
        repository_id,
        source_commit,
        root["case_id"],
        expected_role,
        workflow,
    )
    if expected_role == "greenlit-release":
        validate_release_binary(
            greenlit_binary,
            producer["binary_sha256"],
            source_commit,
        )
    return source, producer
