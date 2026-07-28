"""Public-command canaries for parity producer process-output bounds."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from pathlib import Path

from parity_producer_executables import (
    write_gh_executable,
    write_overflow_executable,
    write_success_escape_executable,
)


TOKEN_DESCRIPTOR = "GREENLIT_GITHUB_PRODUCER_CREDENTIAL_FD"


def run_bounds_canaries(
    *,
    tool: Path,
    test_root: Path,
    checkout: Path,
    output_root: Path,
    trusted: list[str],
    source_commit: str,
    github_inputs: dict[str, Path],
    run_id: int,
    job_id: int,
) -> None:
    """Require bounded rejection and descendant cleanup for both producers."""
    protected = (
        output_root / "seed-github-actions.json",
        output_root / "captures/shell-only-seed-github-actions.json",
    )
    before = {path: path.read_bytes() for path in protected}
    _local_success_escape(
        tool=tool,
        test_root=test_root,
        output_root=output_root,
        trusted=trusted,
        source_commit=source_commit,
    )
    _local_overflow(
        tool=tool,
        test_root=test_root,
        output_root=output_root,
        trusted=trusted,
        source_commit=source_commit,
    )
    _github_job_log_overflow(
        tool=tool,
        test_root=test_root,
        output_root=output_root,
        trusted=trusted,
        github_inputs=github_inputs,
        run_id=run_id,
        job_id=job_id,
    )
    if any(path.read_bytes() != before[path] for path in protected):
        raise RuntimeError("failed bounded producer changed prior parity evidence")
    if checkout.joinpath(".litci").exists():
        raise RuntimeError("bounded producer canary wrote into the source checkout")


def _local_success_escape(
    *,
    tool: Path,
    test_root: Path,
    output_root: Path,
    trusted: list[str],
    source_commit: str,
) -> None:
    target = test_root / "escaped-success-target/release"
    target.mkdir(parents=True)
    binary = target / "litci"
    home = test_root / "escaped-success-home"
    home.mkdir(mode=0o700)
    sentinel = test_root / "successful-descendant-survived"
    child_identity = test_root / "successful-descendant.identity"
    write_success_escape_executable(
        binary,
        version_commit=source_commit,
        sentinel=sentinel,
        child_identity=child_identity,
    )
    result = _run(
        [
            sys.executable,
            "-B",
            str(tool),
            "greenlit-release",
            *trusted,
            "--output-root",
            str(output_root),
            "--binary",
            str(binary),
            "--home",
            str(home),
        ],
        success=False,
    )
    if "left a surviving descendant process" not in result.stderr:
        raise RuntimeError(
            "release producer accepted a successful command with an "
            "escaped-session descendant: " + result.stderr
        )
    _require_descendant_gone(
        sentinel,
        child_identity,
        "successful release producer command",
    )


def _local_overflow(
    *,
    tool: Path,
    test_root: Path,
    output_root: Path,
    trusted: list[str],
    source_commit: str,
) -> None:
    target = test_root / "hostile-target/release"
    target.mkdir(parents=True)
    binary = target / "litci"
    home = test_root / "hostile-home"
    home.mkdir(mode=0o700)
    sentinel = test_root / "local-descendant-survived"
    child_identity = test_root / "local-descendant.identity"
    write_overflow_executable(
        binary,
        version_commit=source_commit,
        sentinel=sentinel,
        child_identity=child_identity,
    )
    result = _run(
        [
            sys.executable,
            "-B",
            str(tool),
            "greenlit-release",
            *trusted,
            "--output-root",
            str(output_root),
            "--binary",
            str(binary),
            "--home",
            str(home),
        ],
        success=False,
    )
    if "stdout exceeds the 8388608-byte safety limit" not in result.stderr:
        raise RuntimeError(
            "release producer did not reject bounded stdout: " + result.stderr
        )
    _require_descendant_gone(
        sentinel,
        child_identity,
        "failed release producer command",
    )


def _github_job_log_overflow(
    *,
    tool: Path,
    test_root: Path,
    output_root: Path,
    trusted: list[str],
    github_inputs: dict[str, Path],
    run_id: int,
    job_id: int,
) -> None:
    executable = test_root / "hostile-gh"
    record = test_root / "hostile-gh-calls.ndjson"
    sentinel = test_root / "github-descendant-survived"
    child_identity = test_root / "github-descendant.identity"
    endpoints = {
        "run": f"repos/KanterLabs/greenlit-app/actions/runs/{run_id}",
        "jobs": (
            f"repos/KanterLabs/greenlit-app/actions/runs/{run_id}"
            "/attempts/1/jobs?per_page=100"
        ),
        "content": (
            "repos/KanterLabs/greenlit-app/contents/"
            ".github/workflows/parity-seed.yml"
        ),
        "log": f"repos/KanterLabs/greenlit-app/actions/jobs/{job_id}/logs",
    }
    write_gh_executable(
        executable,
        record=record,
        sentinel=sentinel,
        child_identity=child_identity,
        endpoints=endpoints,
        github_inputs=github_inputs,
    )
    read_descriptor, write_descriptor = os.pipe()
    try:
        os.write(write_descriptor, b"producer-boundary-test-token")
    finally:
        os.close(write_descriptor)
    try:
        result = _run(
            [
                sys.executable,
                "-B",
                str(tool),
                "github-actions",
                *trusted,
                "--output-root",
                str(output_root),
                "--run-id",
                str(run_id),
                "--self-test-raw-evidence",
                "--self-test-gh-executable",
                str(executable),
            ],
            success=False,
            environment_overrides={TOKEN_DESCRIPTOR: str(read_descriptor)},
            pass_fds=(read_descriptor,),
        )
    finally:
        os.close(read_descriptor)
    if "stdout exceeds the 8388608-byte safety limit" not in result.stderr:
        raise RuntimeError(
            "GitHub producer did not reject bounded log stdout: " + result.stderr
        )
    calls = [
        json.loads(line)
        for line in record.read_text(encoding="utf-8").splitlines()
    ]
    observed_endpoints = [
        next(argument for argument in call if argument.startswith("repos/"))
        for call in calls
    ]
    if observed_endpoints != [
        endpoints["run"],
        endpoints["jobs"],
        endpoints["content"],
        endpoints["log"],
    ]:
        raise RuntimeError("GitHub producer lost exact attempt-specific API binding")
    if any(
        call[1:4] != ["api", "--hostname", "github.com"]
        for call in calls
    ):
        raise RuntimeError("GitHub producer did not pin the official API host")
    _require_descendant_gone(
        sentinel,
        child_identity,
        "failed GitHub producer command",
    )


def _require_descendant_gone(
    sentinel: Path,
    child_identity: Path,
    label: str,
) -> None:
    identity = child_identity.read_text(encoding="ascii").split()
    if len(identity) != 2:
        raise RuntimeError(f"{label} did not record an exact process identity")
    pid = int(identity[0])
    expected_start_time = identity[1]
    deadline = time.monotonic() + 2
    while True:
        try:
            raw = Path(f"/proc/{pid}/stat").read_bytes()
        except FileNotFoundError:
            break
        fields = raw[raw.rfind(b")") + 2 :].split()
        if len(fields) <= 19:
            raise RuntimeError(f"{label} descendant identity became malformed")
        if fields[19].decode("ascii") != expected_start_time:
            break
        if time.monotonic() >= deadline:
            raise RuntimeError(
                f"{label} left exact descendant identity {pid} alive"
            )
        time.sleep(0.02)
    time.sleep(0.55)
    if sentinel.exists():
        raise RuntimeError(f"{label} left a descendant process alive")


def _run(
    command: list[str],
    *,
    success: bool,
    environment_overrides: dict[str, str] | None = None,
    pass_fds: tuple[int, ...] = (),
) -> subprocess.CompletedProcess[str]:
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.upper().endswith("_TOKEN")
        and all(
            fragment not in key.upper()
            for fragment in ("SECRET", "PASSWORD", "CREDENTIAL")
        )
    }
    environment.update(environment_overrides or {})
    result = subprocess.run(
        command,
        env=environment,
        pass_fds=pass_fds,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        timeout=30,
        check=False,
    )
    if success and result.returncode != 0:
        raise RuntimeError(
            f"command failed ({result.returncode}): {' '.join(command)}\n"
            + result.stderr
        )
    if not success and result.returncode == 0:
        raise RuntimeError(f"command unexpectedly passed: {' '.join(command)}")
    return result
