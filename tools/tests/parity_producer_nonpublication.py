"""No-publication scenarios owned by the parity producer behavior gate."""

from __future__ import annotations

import json
import os
import subprocess
import sys
from collections.abc import Callable
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from parity_producer.capture import Production, publish
from parity_producer.common import ProducerError
from parity_producer.github_evidence import project_github_evidence
from parity_producer_executables import write_recorded_gh_executable
from parity_producer_fixture import (
    REPOSITORY_ID,
    RUN_ID,
    expected_gh_calls,
    github_endpoints,
)


NON_CERTIFYING_MESSAGE = (
    "non-certifying producer self-test passed: no canonical files published\n"
)
TOKEN_DESCRIPTOR = "GREENLIT_GITHUB_PRODUCER_CREDENTIAL_FD"
RunCommand = Callable[..., subprocess.CompletedProcess[str]]
ToolCommand = Callable[..., list[str]]


def run_nonpublication_canaries(
    *,
    tool: Path,
    test_root: Path,
    checkout: Path,
    output_root: Path,
    source_commit: str,
    production: list[str],
    raw_command: list[str],
    inputs: dict[str, Path],
    run: RunCommand,
    tool_command: ToolCommand,
) -> None:
    """Exercise every raw, substituted, and direct publication boundary."""
    github_paths = (
        output_root / "seed-github-actions.json",
        output_root / "captures/shell-only-seed-github-actions.json",
    )
    before_entries = _output_entries(output_root)
    _require_non_certifying_success(run(raw_command), "raw evidence")
    if (
        any(path.exists() for path in github_paths)
        or _output_entries(output_root) != before_entries
    ):
        raise RuntimeError("non-certifying raw evidence published canonical files")

    failed = run(raw_command[:-1], success=False)
    if "raw GitHub evidence is restricted" not in failed.stderr:
        raise RuntimeError("canonical GitHub producer accepted raw evidence")

    self_test_only = tool_command(
        tool,
        "github-actions",
        *production,
        "--run-id",
        str(RUN_ID),
        "--self-test-raw-evidence",
    )
    failed = run(self_test_only, success=False)
    if "requires a complete raw evidence bundle or custom gh executable" not in (
        failed.stderr
    ):
        raise RuntimeError(
            "GitHub self-test flag reached live acquisition without a boundary"
        )
    partial_raw = [
        *self_test_only,
        "--run-json",
        str(inputs["run"]),
    ]
    failed = run(partial_raw, success=False)
    if "requires a complete raw evidence bundle or custom gh executable" not in (
        failed.stderr
    ):
        raise RuntimeError("partial raw GitHub evidence escaped fail-closed routing")

    protected = {
        path: f"protected non-certifying boundary: {path.name}\n".encode("utf-8")
        for path in github_paths
    }
    for path, raw_bytes in protected.items():
        path.write_bytes(raw_bytes)
        path.chmod(0o600)
    before_entries = _output_entries(output_root)
    _require_non_certifying_success(run(raw_command), "raw evidence overwrite")
    _require_unchanged(protected, "raw evidence")
    _require_no_new_entries(output_root, before_entries, "raw evidence")
    _require_direct_projection_nonpublishable(
        checkout=checkout,
        output_root=output_root,
        source_commit=source_commit,
        inputs=inputs,
        protected=protected,
    )

    endpoints = github_endpoints()
    fake_gh = test_root / "recorded-gh"
    fake_calls = test_root / "recorded-gh-calls.ndjson"
    write_recorded_gh_executable(
        fake_gh,
        record=fake_calls,
        endpoints=endpoints,
        github_inputs=inputs,
    )
    substituted_without_flag = tool_command(
        tool,
        "github-actions",
        *production,
        "--run-id",
        str(RUN_ID),
        "--self-test-gh-executable",
        str(fake_gh),
    )
    failed = run(substituted_without_flag, success=False)
    if "requires the self-test flag" not in failed.stderr:
        raise RuntimeError("substituted gh executable escaped fail-closed routing")

    fake_github = tool_command(
        tool,
        "github-actions",
        *production,
        "--run-id",
        str(RUN_ID),
        "--self-test-raw-evidence",
        "--self-test-gh-executable",
        str(fake_gh),
    )
    _require_non_certifying_success(
        _run_with_token(fake_github, run),
        "recorded fake-gh evidence",
    )
    _require_exact_gh_calls(fake_calls, fake_gh, source_commit)
    _require_unchanged(protected, "recorded fake-gh evidence")
    _require_no_new_entries(
        output_root,
        before_entries,
        "recorded fake-gh evidence",
    )


def _require_direct_projection_nonpublishable(
    *,
    checkout: Path,
    output_root: Path,
    source_commit: str,
    inputs: dict[str, Path],
    protected: dict[Path, bytes],
) -> None:
    projection = project_github_evidence(
        repository=REPOSITORY_ID,
        requested_run_id=RUN_ID,
        run=json.loads(inputs["run"].read_text(encoding="utf-8")),
        jobs_response=json.loads(inputs["jobs"].read_text(encoding="utf-8")),
        content_response=json.loads(inputs["content"].read_text(encoding="utf-8")),
        job_log=inputs["log"].read_bytes(),
        trusted_source_commit=source_commit,
    )
    candidates = (
        ("neutral raw projector", projection),
        (
            "unsealed raw production",
            Production(
                observation=projection.observation,
                authority=projection.authority,
                _certifying_witness=None,
            ),
        ),
        (
            "caller-forged certifying witness",
            Production(
                observation=projection.observation,
                authority=projection.authority,
                _certifying_witness=object(),
            ),
        ),
    )
    expected_entries = _output_entries(output_root)
    for source, production in candidates:
        try:
            publish(
                production,
                checkout=checkout,
                output_root=output_root,
                trusted_repository=REPOSITORY_ID,
                trusted_source_commit=source_commit,
            )
        except ProducerError as error:
            if "cannot publish canonical files" not in str(error):
                raise RuntimeError(
                    f"{source} failed with the wrong publication boundary"
                ) from error
        else:
            raise RuntimeError(f"{source} published canonical files")
        _require_unchanged(protected, source)
        _require_no_new_entries(output_root, expected_entries, source)


def _require_non_certifying_success(
    result: subprocess.CompletedProcess[str],
    source: str,
) -> None:
    if result.stdout != NON_CERTIFYING_MESSAGE or result.stderr:
        raise RuntimeError(
            f"{source} did not report the exact non-certifying disposition"
        )


def _require_unchanged(protected: dict[Path, bytes], source: str) -> None:
    if any(path.read_bytes() != raw for path, raw in protected.items()):
        raise RuntimeError(f"{source} replaced prior canonical evidence")


def _output_entries(output_root: Path) -> set[Path]:
    return {
        path.relative_to(output_root)
        for path in output_root.rglob("*")
    }


def _require_no_new_entries(
    output_root: Path,
    expected: set[Path],
    source: str,
) -> None:
    if _output_entries(output_root) != expected:
        raise RuntimeError(f"{source} created or removed parity output entries")


def _require_exact_gh_calls(
    record: Path,
    executable: Path,
    source_commit: str,
) -> None:
    observed = [
        json.loads(line)
        for line in record.read_text(encoding="utf-8").splitlines()
    ]
    if observed != expected_gh_calls(executable, source_commit):
        raise RuntimeError(
            "recorded fake-gh acquisition changed its exact API requests"
        )


def _run_with_token(
    command: list[str],
    run: RunCommand,
) -> subprocess.CompletedProcess[str]:
    read_descriptor, write_descriptor = os.pipe()
    try:
        os.write(write_descriptor, b"producer-boundary-test-token")
    finally:
        os.close(write_descriptor)
    try:
        return run(
            command,
            environment_overrides={TOKEN_DESCRIPTOR: str(read_descriptor)},
            pass_fds=(read_descriptor,),
        )
    finally:
        os.close(read_descriptor)
