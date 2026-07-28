#!/usr/bin/env python3
"""Public release-check finalizer mismatch canary."""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
from pathlib import Path

from release_check_credential_bundle_canary_cases import (
    SHA256,
    CanaryError,
    _directory,
    _environment,
    _evidence,
    _file,
    _invoke,
    _prepared,
)


def _git(repository: Path, arguments: list[str], *, output: bool = False) -> bytes:
    result = subprocess.run(
        ["/usr/bin/git", *arguments],
        cwd=repository,
        env=_environment(),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=20,
        check=False,
    )
    if result.returncode != 0 or (not output and (result.stdout or result.stderr)):
        raise CanaryError("could not establish finalizer canary source")
    return result.stdout


def _repository(root: Path) -> tuple[Path, str]:
    repository = root / "repository"
    tools = repository / "tools"
    _directory(tools, 0o755)
    source = Path(__file__).resolve().parent
    for name in (
        "release-check",
        "release_check_credential_bundle.py",
        "release_check_credential_bundle_extract.py",
        "release_check_credential_bundle_inputs.py",
        "release_check_credential_bundle_io.py",
    ):
        shutil.copy2(source / name, tools / name)
    _git(repository, ["init", "-q"])
    _git(repository, ["add", "--all"])
    _git(
        repository,
        [
            "-c",
            "user.name=Greenlit canary",
            "-c",
            "user.email=greenlit-canary@example.invalid",
            "commit",
            "-q",
            "-m",
            "canary",
        ],
    )
    commit = _git(
        repository,
        ["rev-parse", "--verify", "HEAD"],
        output=True,
    ).decode("ascii").strip()
    if len(commit) != 40:
        raise CanaryError("finalizer canary source identity is malformed")
    return repository, commit


def _pack(
    root: Path,
    kind: str,
    input_root: Path,
    output: Path,
    source: str,
    binary: Path | None = None,
) -> str:
    arguments = [
        f"pack-{kind}",
        "--input-root",
        str(input_root),
        "--output",
        str(output),
        "--expected-source",
        source,
    ]
    if binary is not None:
        arguments.extend(("--greenlit-binary", str(binary)))
    digest = _invoke(root, arguments, accepted=True).decode("ascii").strip()
    if SHA256.fullmatch(digest) is None:
        raise CanaryError("finalizer fixture did not emit one bundle digest")
    return digest


def _fixtures(root: Path, source: str) -> tuple[tuple[Path, str], ...]:
    prepared = root / "prepared"
    _directory(prepared, 0o700)
    _prepared(prepared)
    prepared_tar = root / "prepared.tar"
    prepared_digest = _pack(
        root, "prepared", prepared, prepared_tar, source
    )
    local = root / "local"
    _directory(local, 0o700)
    _evidence(local, ("oracle", "greenlit-release"))
    local_binary = root / "local-litci"
    _file(local_binary, b"\x7fELFlocal-different", 0o755)
    local_tar = root / "local.tar"
    local_digest = _pack(
        root, "local", local, local_tar, source, local_binary
    )
    github = root / "github"
    _directory(github, 0o700)
    _evidence(github, ("github-actions",))
    github_tar = root / "github.tar"
    github_digest = _pack(root, "github", github, github_tar, source)
    return (
        (prepared_tar, prepared_digest),
        (local_tar, local_digest),
        (github_tar, github_digest),
    )


def run_finalizer_mismatch() -> None:
    """Require the public finalizer to reject a cross-job binary mismatch."""

    previous_umask = os.umask(0o077)
    try:
        with tempfile.TemporaryDirectory(prefix="greenlit-finalizer-canary.") as raw:
            root = Path(raw)
            root.chmod(0o700)
            repository, commit = _repository(root)
            prepared, local, github = _fixtures(root, commit)
            runner_temp = root / "runner-temp"
            _directory(runner_temp, 0o700)
            environment = _environment()
            environment.update(
                {
                    "GREENLIT_BUILD_COMMIT": commit,
                    "GREENLIT_PREPARED_BUNDLE": str(prepared[0]),
                    "GREENLIT_PREPARED_SHA256": prepared[1],
                    "GREENLIT_LOCAL_BUNDLE": str(local[0]),
                    "GREENLIT_LOCAL_SHA256": local[1],
                    "GREENLIT_GITHUB_BUNDLE": str(github[0]),
                    "GREENLIT_GITHUB_SHA256": github[1],
                    "RUNNER_TEMP": str(runner_temp),
                }
            )
            result = subprocess.run(
                ["/usr/bin/bash", str(repository / "tools/release-check"), "finalize"],
                cwd=repository,
                env=environment,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=30,
                check=False,
            )
            if (
                result.returncode == 0
                or result.stdout
                or b"local parity binary differs" not in result.stderr
                or (repository / "target").exists()
                or any(runner_temp.iterdir())
            ):
                raise CanaryError(
                    "public release finalizer did not reject and clean a binary mismatch"
                )
    finally:
        os.umask(previous_umask)
