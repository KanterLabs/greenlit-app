"""Authority-ordered evidence production and atomic comparison."""

from __future__ import annotations

import os
import stat
import tempfile
from pathlib import Path

from .contract import REPOSITORY_ID
from .errors import GateError
from .filesystem import BinaryBinding, OutputRoot, RoleEvidenceBinding
from .github import (
    GitHubCredential,
    RunIdentity,
    certify_run,
    credential_free_environment,
    PRODUCER_TOKEN_DESCRIPTOR,
)
from .process import run_command
from .source import exact_source


PYTHON = "/usr/bin/python3"
PYTHON_OPTIONS = ("-E", "-s", "-B")


def _verify_source(repository: Path, source_commit: str, stage: str) -> None:
    observed_repository, observed_commit = exact_source(repository)
    if observed_repository != repository or observed_commit != source_commit:
        raise GateError(f"live parity source identity changed at {stage}")


def _bound_call(
    output: OutputRoot,
    command: list[str],
    *,
    repository: Path,
    timeout: float,
    stage: str,
    source_commit: str,
    binary: BinaryBinding | None = None,
    evidence: tuple[RoleEvidenceBinding, ...] = (),
    environment: dict[str, str] | None = None,
    pass_fds: tuple[int, ...] = (),
) -> None:
    _verify_source(repository, source_commit, f"before {stage}")
    for binding in evidence:
        binding.verify(f"before {stage}")
    if binary is not None:
        binary.verify(f"before {stage}")
    output.verify(f"before {stage}")
    process_error: GateError | None = None
    try:
        run_command(
            command,
            cwd=repository,
            timeout=timeout,
            environment=environment,
            pass_fds=pass_fds,
        )
    except GateError as error:
        process_error = error
    boundary_error: GateError | None = None
    try:
        output.verify(f"after {stage}")
    except GateError as binding_error:
        boundary_error = binding_error
    if binary is not None:
        try:
            binary.verify(f"after {stage}")
        except GateError as binary_error:
            if boundary_error is None:
                boundary_error = binary_error
    for binding in evidence:
        try:
            binding.verify(f"after {stage}")
        except GateError as evidence_error:
            if boundary_error is None:
                boundary_error = evidence_error
    try:
        _verify_source(repository, source_commit, f"after {stage}")
    except GateError as source_error:
        if boundary_error is None:
            boundary_error = source_error
    if boundary_error is not None:
        if process_error is not None:
            raise boundary_error from process_error
        raise boundary_error
    if process_error is not None:
        raise process_error


def _common(repository: Path, output: OutputRoot, source_commit: str) -> list[str]:
    return [
        "--checkout",
        str(repository),
        "--repository-id",
        REPOSITORY_ID,
        "--source-commit",
        source_commit,
        "--output-root",
        str(output.path),
    ]


def produce_local(
    repository: Path,
    binary: BinaryBinding,
    output: OutputRoot,
    source_commit: str,
) -> None:
    """Produce only credential-free oracle and release-Greenlit evidence."""

    producer = repository / "tools" / "collect-parity-observation"
    common = _common(repository, output, source_commit)
    _bound_call(
        output,
        [PYTHON, *PYTHON_OPTIONS, str(producer), "oracle", *common],
        repository=repository,
        timeout=15 * 60,
        stage="oracle producer",
        source_commit=source_commit,
        environment=credential_free_environment(),
    )
    output.require_layout(("oracle",), "after oracle producer")
    oracle_evidence = output.bind_role("oracle", "after oracle producer")
    with tempfile.TemporaryDirectory(
        prefix="greenlit-live-parity-home.",
        dir=output.path.parent,
    ) as raw_home:
        home = Path(raw_home)
        home.chmod(0o700)
        metadata = home.lstat()
        if (
            not stat.S_ISDIR(metadata.st_mode)
            or stat.S_IMODE(metadata.st_mode) != 0o700
        ):
            raise GateError("Greenlit parity HOME is not a private directory")
        _bound_call(
            output,
            [
                PYTHON,
                *PYTHON_OPTIONS,
                str(producer),
                "greenlit-release",
                *common,
                "--binary",
                str(binary.path),
                "--home",
                str(home),
            ],
            repository=repository,
            timeout=20 * 60,
            stage="Greenlit producer",
            source_commit=source_commit,
            binary=binary,
            environment=credential_free_environment(),
            evidence=(oracle_evidence,),
        )
        output.require_layout(
            ("oracle", "greenlit-release"),
            "after Greenlit producer",
        )
        greenlit_evidence = output.bind_role(
            "greenlit-release",
            "after Greenlit producer",
        )
        oracle_evidence.verify("after credential-free local production")
        greenlit_evidence.verify("after credential-free local production")


def produce_github(
    repository: Path,
    output: OutputRoot,
    source_commit: str,
    run: RunIdentity,
    credential: GitHubCredential,
) -> None:
    """Produce only credentialed GitHub evidence; never execute Greenlit."""

    producer = repository / "tools" / "collect-parity-observation"
    descriptor = credential.open_descriptor()
    environment = credential_free_environment()
    environment[PRODUCER_TOKEN_DESCRIPTOR] = str(descriptor)
    try:
        _bound_call(
            output,
            [
                PYTHON,
                *PYTHON_OPTIONS,
                str(producer),
                "github-actions",
                *_common(repository, output, source_commit),
                "--run-id",
                str(run.run_id),
            ],
            repository=repository,
            timeout=10 * 60,
            stage="GitHub producer",
            source_commit=source_commit,
            environment=environment,
            pass_fds=(descriptor,),
        )
    finally:
        try:
            os.close(descriptor)
        except OSError:
            pass
    output.require_layout(("github-actions",), "after GitHub producer")
    evidence = output.bind_role("github-actions", "after GitHub producer")
    evidence.verify("before same-attempt GitHub recheck")
    rechecked = certify_run(credential, source_commit, run.run_id)
    if rechecked != run:
        raise GateError(
            "canonical GitHub parity run or attempt changed during evidence collection"
        )
    output.verify("after same-attempt GitHub recheck")
    evidence.verify("after same-attempt GitHub recheck")


def compare_evidence(
    repository: Path,
    binary: BinaryBinding,
    output: OutputRoot,
    source_commit: str,
) -> None:
    """Compare one already merged, credential-free three-role evidence set."""

    roles = ("oracle", "greenlit-release", "github-actions")
    output.require_layout(roles, "before atomic comparator")
    evidence = tuple(output.bind_role(role, "before atomic comparator") for role in roles)
    comparator = repository / "tools" / "compare-parity"
    _bound_call(
        output,
        [
            PYTHON,
            *PYTHON_OPTIONS,
            str(comparator),
            "--repository-root",
            str(repository),
            "--repository-id",
            REPOSITORY_ID,
            "--source-commit",
            source_commit,
            "--greenlit-binary",
            str(binary.path),
            "--capture-root",
            str(output.path),
            "--exceptions",
            str(repository / "docs" / "PARITY-EXCEPTIONS.md"),
            str(output.path / "seed-oracle.json"),
            str(output.path / "seed-github-actions.json"),
            str(output.path / "seed-greenlit-release.json"),
        ],
        repository=repository,
        timeout=5 * 60,
        stage="atomic comparator",
        source_commit=source_commit,
        binary=binary,
        environment=credential_free_environment(),
        evidence=evidence,
    )
    output.require_layout(roles, "after atomic comparator")
