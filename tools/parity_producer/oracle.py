"""Independent direct-shell oracle bound to the committed seed workflow."""

from __future__ import annotations

import datetime as dt
import os
import stat
import subprocess
import tempfile
import time
import uuid
from pathlib import Path
from typing import Any

from parity_producer.bounded_process import run_bounded
from parity_producer.capture import Production, authority_envelope
from parity_producer.common import (
    AUTHORITATIVE_REPOSITORY,
    COMMIT,
    GIT_EXECUTABLE,
    GIT_FIXED_OPTIONS,
    MAX_GIT_OUTPUT_BYTES,
    RUNNER,
    WORKFLOW_PATH,
    ProducerError,
    format_timestamp,
    git_environment,
    sha256_bytes,
)
from parity_producer.contract import (
    EXPECTED_JOB_NAME,
    EXPECTED_STEPS,
    assemble_observation,
)
from parity_producer.host_environment import (
    minimal_environment,
    trusted_system_tools,
)
from parity_producer.markers import parse_markers
from parity_producer.oracle_workflow import (
    EXPRESSION,
    EXPECTED_RUN_BLOCKS,
    extract_run_blocks,
)


RUN_TIMEOUT_SECONDS = 60
_ORACLE_CERTIFYING_WITNESS = object()


def produce_oracle(
    checkout: Path,
    repository_id: str,
    trusted_source_commit: str,
) -> Production:
    """Execute the two exact committed run blocks without Greenlit or GitHub."""
    checkout = checkout.resolve()
    if repository_id != AUTHORITATIVE_REPOSITORY:
        raise ProducerError(
            f"repository identity must be {AUTHORITATIVE_REPOSITORY!r}"
        )
    if COMMIT.fullmatch(trusted_source_commit) is None:
        raise ProducerError("trusted source commit must be full lowercase SHA")
    _require_ancestor(checkout, trusted_source_commit)
    workflow_bytes = _git_blob(checkout, trusted_source_commit, WORKFLOW_PATH)
    run_blocks = extract_run_blocks(workflow_bytes)
    if run_blocks != EXPECTED_RUN_BLOCKS:
        raise ProducerError(
            "committed parity workflow run blocks differ from the fixed host oracle"
        )
    tools = trusted_system_tools(("bash", "cat", "cut", "sha256sum", "stat"))
    bash = tools["bash"]

    with tempfile.TemporaryDirectory(prefix="greenlit-parity-oracle-") as directory:
        root = Path(directory)
        workspace = root / "workspace"
        runner_temp = root / "runner-temp"
        oracle_home = root / "home"
        workspace.mkdir(mode=0o700)
        runner_temp.mkdir(mode=0o700)
        oracle_home.mkdir(mode=0o700)
        github_output = root / "github-output"
        environment = _oracle_environment(
            runner_temp, github_output, oracle_home, list(tools.values())
        )

        run_id = f"oracle-{uuid.uuid4().hex}"
        run_started_wall = _now()
        run_started_tick = time.monotonic_ns()
        job_started_wall = run_started_wall
        job_started_tick = run_started_tick

        emit_started_wall = _now()
        emit_started_tick = time.monotonic_ns()
        emit = _run_block(bash, run_blocks[0], workspace, environment, "emit")
        emit_completed_wall = _now()
        emit_duration = _elapsed_ms(emit_started_tick)
        seed_value = _read_command_output(github_output)

        rendered_verify = _render_verify(run_blocks[1], seed_value)
        verify_started_wall = _now()
        verify_started_tick = time.monotonic_ns()
        verify = _run_block(
            bash, rendered_verify, workspace, environment, "verify"
        )
        verify_completed_wall = _now()
        verify_duration = _elapsed_ms(verify_started_tick)

        job_completed_wall = _now()
        job_duration = _elapsed_ms(job_started_tick)
        run_completed_wall = job_completed_wall
        run_duration = _elapsed_ms(run_started_tick)
        markers = parse_markers(
            [*emit.stdout.splitlines(), *verify.stdout.splitlines()],
            "direct oracle output",
        )
        _validate_oracle_files(workspace, runner_temp, markers)

        projection = {
            "run_started_at": format_timestamp(run_started_wall),
            "run_completed_at": format_timestamp(run_completed_wall),
            "run_duration_ms": run_duration,
            "run_conclusion": "success",
            "job_name": EXPECTED_JOB_NAME,
            "job_conclusion": "success",
            "job_duration_ms": job_duration,
            "steps": [
                {
                    "id": EXPECTED_STEPS[0][0],
                    "name": EXPECTED_STEPS[0][1],
                    "outcome": "success",
                    "conclusion": "success",
                    "duration_ms": emit_duration,
                },
                {
                    "id": EXPECTED_STEPS[1][0],
                    "name": EXPECTED_STEPS[1][1],
                    "outcome": "success",
                    "conclusion": "success",
                    "duration_ms": verify_duration,
                },
            ],
            "lifecycle": _lifecycle(
                run_started_wall,
                job_started_wall,
                emit_started_wall,
                emit_completed_wall,
                verify_started_wall,
                verify_completed_wall,
                job_completed_wall,
                run_completed_wall,
            ),
            "log_lines": [],
        }
        producer = {
            "role": "oracle",
            "repository": repository_id,
            "runner": RUNNER,
            "run_id": run_id,
            "run_attempt": 1,
            "run_url": None,
            "binary_sha256": None,
        }
        source = {
            "repository": repository_id,
            "commit": trusted_source_commit,
            "workflow_path": WORKFLOW_PATH,
            "workflow_sha256": sha256_bytes(workflow_bytes),
        }
        observation = assemble_observation(producer, source, projection, markers)
        authority = authority_envelope(
            observation,
            "oracle",
            {
                "source_commit": trusted_source_commit,
                "workflow_blob_sha256": source["workflow_sha256"],
                "run_block_sha256": [
                    sha256_bytes(block.encode("utf-8")) for block in run_blocks
                ],
                "rendered_verify_sha256": sha256_bytes(
                    rendered_verify.encode("utf-8")
                ),
                "bash_path": bash,
                "process_umask": "0022",
                "command_output_sha256": sha256_bytes(github_output.read_bytes()),
                "step_exit_codes": [emit.returncode, verify.returncode],
                "log_marker_identities": [
                    {"job": job, "step": step}
                    for job, step in markers.identities
                ],
            },
        )
        return Production(
            observation=observation,
            authority=authority,
            _certifying_witness=_ORACLE_CERTIFYING_WITNESS,
        )


def _run_block(
    bash: str,
    block: str,
    workspace: Path,
    environment: dict[str, str],
    step_id: str,
) -> subprocess.CompletedProcess[str]:
    try:
        result = subprocess.run(
            [bash, "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", block],
            cwd=workspace,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=RUN_TIMEOUT_SECONDS,
            check=False,
            preexec_fn=_oracle_child_setup,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ProducerError(f"direct oracle step {step_id!r} could not execute: {error}") from error
    if result.returncode != 0:
        raise ProducerError(
            f"direct oracle step {step_id!r} failed with exit {result.returncode}"
        )
    return result


def _read_command_output(path: Path) -> str:
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError as error:
        raise ProducerError(f"direct oracle could not read GITHUB_OUTPUT: {error}") from error
    if lines != ["seed_value=greenlit"]:
        raise ProducerError("direct oracle did not receive exact seed command output")
    return "greenlit"


def _render_verify(block: str, seed_value: str) -> str:
    if block.count(EXPRESSION) != 2 or seed_value != "greenlit":
        raise ProducerError("verify run block has an ambiguous step-output expression")
    return block.replace(EXPRESSION, seed_value)


def _validate_oracle_files(
    workspace: Path,
    runner_temp: Path,
    markers: Any,
) -> None:
    if Path(markers.temporary_directory) != runner_temp:
        raise ProducerError("direct oracle temporary-directory marker was not explicit")
    probe = workspace / "parity-seed.txt"
    try:
        metadata = probe.lstat()
        raw = probe.read_bytes()
    except OSError as error:
        raise ProducerError(f"direct oracle probe cannot be inspected: {error}") from error
    if (
        not stat.S_ISREG(metadata.st_mode)
        or f"0{stat.S_IMODE(metadata.st_mode):03o}" != markers.probe_mode
        or sha256_bytes(raw) != markers.probe_sha256
    ):
        raise ProducerError("direct oracle filesystem marker differs from actual probe")


def _oracle_environment(
    runner_temp: Path,
    github_output: Path,
    home: Path,
    executables: list[str],
) -> dict[str, str]:
    return minimal_environment(
        home=home,
        executables=executables,
        extra={
            "GITHUB_JOB": "shell",
            "GITHUB_WORKFLOW": "Parity seed",
            "RUNNER_ARCH": "X64",
            "RUNNER_OS": "Linux",
            "RUNNER_TEMP": str(runner_temp),
            "GITHUB_OUTPUT": str(github_output),
        },
    )


def _oracle_child_setup() -> None:
    os.umask(0o022)


def _lifecycle(*timestamps: dt.datetime) -> list[dict[str, Any]]:
    identities = (
        ("run-started", "run_started", None, None),
        ("job-shell-started", "job_started", "shell", None),
        ("step-emit-started", "step_started", "shell", "emit"),
        ("step-emit-completed", "step_completed", "shell", "emit"),
        ("step-verify-started", "step_started", "shell", "verify"),
        ("step-verify-completed", "step_completed", "shell", "verify"),
        ("job-shell-completed", "job_completed", "shell", None),
        ("run-completed", "run_completed", None, None),
    )
    return [
        {
            "id": identity,
            "sequence": index,
            "kind": kind,
            "timestamp": format_timestamp(timestamp),
            "job_id": job_id,
            "step_id": step_id,
        }
        for index, ((identity, kind, job_id, step_id), timestamp) in enumerate(
            zip(identities, timestamps, strict=True),
            1,
        )
    ]


def _require_ancestor(checkout: Path, commit: str) -> None:
    _git(checkout, "cat-file", "-e", f"{commit}^{{commit}}")
    try:
        result = subprocess.run(
            [
                GIT_EXECUTABLE,
                *GIT_FIXED_OPTIONS,
                "-C",
                str(checkout),
                "merge-base",
                "--is-ancestor",
                commit,
                "HEAD",
            ],
            env=git_environment(),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=30,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ProducerError(f"cannot verify trusted source ancestry: {error}") from error
    if result.returncode != 0:
        raise ProducerError("trusted source commit is not reachable from capture checkout")


def _git_blob(checkout: Path, commit: str, path: str) -> bytes:
    return _git(checkout, "cat-file", "blob", f"{commit}:{path}")


def _git(checkout: Path, *arguments: str) -> bytes:
    result = run_bounded(
        [
            GIT_EXECUTABLE,
            *GIT_FIXED_OPTIONS,
            "-C",
            str(checkout),
            *arguments,
        ],
        label="trusted oracle Git read",
        environment=git_environment(),
        timeout_seconds=30,
        stdout_limit=MAX_GIT_OUTPUT_BYTES,
        stderr_limit=64 * 1024,
    )
    if result.returncode != 0:
        raise ProducerError("trusted workflow source is unavailable from Git")
    return result.stdout


def _now() -> dt.datetime:
    return dt.datetime.now(tz=dt.timezone.utc)


def _elapsed_ms(start: int) -> int:
    return max(0, round((time.monotonic_ns() - start) / 1_000_000))
