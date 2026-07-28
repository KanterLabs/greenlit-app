"""Git-backed capture sealing for comparator command-boundary self-tests."""

from __future__ import annotations

import copy
import hashlib
import json
import shutil
import subprocess
from pathlib import Path
from typing import Any, Callable

from .selftest_data import (
    ROLES,
    WORKFLOW_BYTES,
    WORKFLOW_PATH,
    observation_triple,
    release_binary_bytes,
)


Documents = list[dict[str, Any]]
RawNumbers = dict[int, dict[str, str]]
CaptureMutation = Callable[[dict[str, Any]], None]


def _git(repository: Path, *arguments: str) -> str:
    completed = subprocess.run(
        ["git", "-C", str(repository), *arguments],
        check=True,
        capture_output=True,
        text=True,
    )
    return completed.stdout.strip()


def _git_bytes(repository: Path, *arguments: str) -> bytes:
    completed = subprocess.run(
        ["git", "-C", str(repository), *arguments],
        check=True,
        capture_output=True,
    )
    return completed.stdout


def _exact_json(value: Any, raw_numbers: dict[str, str]) -> bytes:
    text = json.dumps(
        value,
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
        allow_nan=False,
    )
    for sentinel, literal in raw_numbers.items():
        text = text.replace(json.dumps(sentinel, ensure_ascii=False), literal)
    return (text + "\n").encode()


def _seed_value(job: dict[str, Any]) -> Any:
    try:
        return job["steps"][0]["outputs"][0]["value"]
    except (IndexError, KeyError, TypeError):
        return None


def _run_blocks(workflow: bytes) -> tuple[str, str]:
    lines = workflow.decode("utf-8").splitlines(keepends=True)
    blocks: list[str] = []
    for step_id in ("emit", "verify"):
        header = f"      - id: {step_id}\n"
        starts = [index for index, line in enumerate(lines) if line == header]
        if len(starts) != 1:
            raise ValueError(f"self-test workflow lacks exact {step_id!r} step")
        start = starts[0]
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
            raise ValueError(f"self-test workflow lacks {step_id!r} run block")
        block_start = run_lines[0] + 1
        block_lines: list[str] = []
        for line in lines[block_start:end]:
            if line.strip() and not line.startswith("          "):
                break
            block_lines.append(line[10:] if line.startswith("          ") else "\n")
        blocks.append("".join(block_lines))
    return blocks[0], blocks[1]


def _capture_document(
    observation: dict[str, Any],
    role: str,
    source_commit: str,
    workflow: bytes,
    raw_numbers: dict[str, str],
) -> dict[str, Any]:
    embedded = copy.deepcopy(observation)
    embedded["producer"].pop("capture_sha256", None)
    job = embedded["jobs"][0]
    identities = [
        {"job": job["id"], "step": step["id"]} for step in job["steps"]
    ]
    source, producer = embedded["source"], embedded["producer"]
    if role == "oracle":
        run_blocks = _run_blocks(workflow)
        rendered_verify = run_blocks[1].replace(
            "${{ steps.emit.outputs.seed_value }}", "greenlit"
        )
        bash = shutil.which("bash")
        if bash is None:
            raise ValueError("self-test requires bash for oracle authority")
        role_authority: dict[str, Any] = {
            "source_commit": source_commit,
            "workflow_blob_sha256": source["workflow_sha256"],
            "run_block_sha256": [
                hashlib.sha256(block.encode("utf-8")).hexdigest()
                for block in run_blocks
            ],
            "rendered_verify_sha256": hashlib.sha256(
                rendered_verify.encode("utf-8")
            ).hexdigest(),
            "bash_path": str(Path(bash).resolve()),
            "process_umask": "0022",
            "command_output_sha256": hashlib.sha256(
                b"seed_value=greenlit\n"
            ).hexdigest(),
            "step_exit_codes": [0, 0],
            "log_marker_identities": identities,
        }
    elif role == "github-actions":
        role_authority = {
            "event": "push",
            "head_sha": source_commit,
            "workflow_sha256": source["workflow_sha256"],
            "run_attempt": producer["run_attempt"],
            "run_url": producer["run_url"],
            "job_name": job["name"],
            "job_conclusion": job["conclusion"],
            "step_records": job["steps"],
            "lifecycle_records": embedded["lifecycle"],
            "log_marker_identities": identities,
        }
    else:
        role_authority = {
            "event": "push",
            "source_commit": source_commit,
            "build_source_commit": source_commit,
            "binary_sha256": producer["binary_sha256"],
            "frozen_workflow_sha256": source["workflow_sha256"],
            "result_conclusion": "passed",
            "result_compatibility": "degraded",
            "result_assurance": "none",
            "journal_lifecycle": embedded["lifecycle"],
            "requested_runner": producer["runner"],
            "resolved_runner": "ubuntu-24.04",
            "reported_durations": {
                "run_elapsed_ms": embedded["run"]["duration_ms"],
                "job_duration_ms": job["duration_ms"],
                "step_duration_ms": [
                    step["duration_ms"] for step in job["steps"]
                ],
            },
        }
    semantic = {key: value for key, value in embedded.items() if key != "producer"}
    semantic_raw = _exact_json(semantic, raw_numbers).rstrip(b"\n")
    authority = {
        "common": {
            "repository": source["repository"],
            "commit": source_commit,
            "workflow_sha256": source["workflow_sha256"],
            "run_id": embedded["run"]["id"],
        },
        "markers": {
            "contexts": embedded["contexts"],
            "seed_value": _seed_value(job),
            "temporary_directory": embedded["run"]["temporary_directory"],
            "filesystem_probes": embedded["filesystem_probes"],
        },
        role: role_authority,
        "semantic_sha256": hashlib.sha256(semantic_raw).hexdigest(),
    }
    return {
        "schema_version": "ParityCaptureV1",
        "case_id": embedded["case_id"],
        "role": role,
        "capture_method": producer["capture_method"],
        "authority": authority,
        "observation": embedded,
    }


def seal_captures(
    repository: Path,
    source_commit: str,
    documents: Documents,
    output_root: Path,
    raw_numbers: RawNumbers | None = None,
) -> Documents:
    """Write private live captures replaying the supplied documents."""

    result = copy.deepcopy(documents)
    roles = [document.get("producer", {}).get("role") for document in result]
    if sorted(roles) != sorted(ROLES):
        raise ValueError("self-test triple must contain each producer role exactly once")
    output_root.mkdir(mode=0o700)
    capture_root = output_root / "captures"
    capture_root.mkdir(mode=0o700)
    workflow = _git_bytes(
        repository,
        "cat-file",
        "blob",
        f"{source_commit}:{WORKFLOW_PATH}",
    )
    for index, document in enumerate(result):
        role = document["producer"]["role"]
        replacements = (raw_numbers or {}).get(index, {})
        capture = _capture_document(
            document, role, source_commit, workflow, replacements
        )
        payload = _exact_json(capture, replacements)
        capture_path = capture_root / f"{document['case_id']}-{role}.json"
        capture_path.write_bytes(payload)
        capture_path.chmod(0o600)
        document["producer"]["capture_sha256"] = hashlib.sha256(payload).hexdigest()
    return result


def initialize_repository(
    repository: Path,
) -> tuple[str, dict[tuple[str, str], str], Path]:
    """Create immutable source and initial strict captures in a temporary Git repo."""

    repository.mkdir()
    _git(repository, "init", "--initial-branch=main")
    _git(repository, "config", "user.name", "Parity Self Test")
    _git(repository, "config", "user.email", "parity-self-test@example.invalid")
    workflow = repository / WORKFLOW_PATH
    workflow.parent.mkdir(parents=True)
    workflow.write_bytes(WORKFLOW_BYTES)
    _git(repository, "add", WORKFLOW_PATH)
    _git(repository, "commit", "-m", "test: add immutable parity workflow")
    source_commit = _git(repository, "rev-parse", "HEAD")
    binary = repository.parent / f"{repository.name}-target" / "release" / "litci"
    binary.parent.mkdir(parents=True)
    binary.write_bytes(release_binary_bytes(source_commit))
    binary.chmod(0o755)
    placeholders = {
        (case_id, role): "0" * 64
        for case_id in ("contract-case", "shell-only-seed")
        for role in ROLES
    }
    initial_root = repository.parent / f"{repository.name}-initial-live"
    sealed = seal_captures(
        repository,
        source_commit,
        observation_triple(source_commit, placeholders),
        initial_root,
    )
    second_root = repository.parent / f"{repository.name}-initial-seed-live"
    sealed += seal_captures(
        repository,
        source_commit,
        observation_triple(source_commit, placeholders, "shell-only-seed"),
        second_root,
    )
    digests = {
        (document["case_id"], document["producer"]["role"]):
        document["producer"]["capture_sha256"]
        for document in sealed
    }
    return source_commit, digests, binary


def binary_canary_paths(
    temporary: Path, binary: Path
) -> tuple[Path, Path, Path, Path]:
    """Create missing, changed, symlink, and non-executable binary canaries."""

    missing = temporary / "missing-release-litci"
    drifted = temporary / "drifted-release-litci"
    drifted.write_bytes(binary.read_bytes() + b"# mutated\n")
    drifted.chmod(0o755)
    symlink = temporary / "symlink-release-litci"
    symlink.symlink_to(binary)
    non_executable = temporary / "non-executable-litci"
    non_executable.write_bytes(binary.read_bytes())
    non_executable.chmod(0o600)
    return missing, drifted, symlink, non_executable


def rewrite_capture(
    capture_root: Path,
    documents: Documents,
    index: int,
    mutation: CaptureMutation,
) -> None:
    """Rewrite and commit one hostile capture, rebinding its observation digest."""

    document = documents[index]
    role = document["producer"]["role"]
    path = (
        capture_root
        / "captures"
        / f"{document['case_id']}-{role}.json"
    )
    capture = json.loads(path.read_text(encoding="utf-8"))
    mutation(capture)
    payload = _exact_json(capture, {})
    path.write_bytes(payload)
    path.chmod(0o600)
    document["producer"]["capture_sha256"] = hashlib.sha256(payload).hexdigest()


def install_empty_commit_replacement(repository: Path, source_commit: str) -> None:
    """Install a hostile replacement commit for Git provenance canaries."""

    tree = subprocess.run(
        ["git", "-C", str(repository), "mktree"],
        input=b"",
        check=True,
        capture_output=True,
    ).stdout.decode("ascii").strip()
    replacement = subprocess.run(
        ["git", "-C", str(repository), "commit-tree", tree],
        input=b"hostile replacement\n",
        check=True,
        capture_output=True,
    ).stdout.decode("ascii").strip()
    _git(repository, "replace", source_commit, replacement)


def remove_replacement(repository: Path, source_commit: str) -> None:
    """Remove the hostile replacement installed by a provenance canary."""

    _git(repository, "replace", "-d", source_commit)


__all__ = [
    "binary_canary_paths",
    "initialize_repository",
    "install_empty_commit_replacement",
    "remove_replacement",
    "rewrite_capture",
    "seal_captures",
]
