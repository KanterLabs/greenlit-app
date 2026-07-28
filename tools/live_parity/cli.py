"""Public CLI router for isolated live-parity production and comparison."""

from __future__ import annotations

import argparse
import hashlib
import os
import sys
from pathlib import Path

from .contract import POSITIVE_INTEGER, REPOSITORY_ID
from .errors import GateError
from .filesystem import BinaryBinding, OutputRoot
from .github import (
    TOKEN_DESCRIPTOR,
    GitHubCredential,
    RunIdentity,
    certify_run,
    credential_free_environment,
    discover_run,
)
from .orchestration import compare_evidence, produce_github, produce_local
from .process import run_command
from .source import exact_source


CANARY_TOKEN = "greenlit-live-production-credential-canary"


def _positive_run_id(value: str, source: str) -> int:
    if POSITIVE_INTEGER.fullmatch(value) is None:
        raise GateError(f"{source} must be a positive decimal GitHub Actions run id")
    return int(value, 10)


def _common(parser: argparse.ArgumentParser, *, binary: bool) -> None:
    parser.add_argument("--repository-root", type=Path, required=True)
    if binary:
        parser.add_argument("--greenlit-binary", type=Path, required=True)
    parser.add_argument("--output-root", type=Path, required=True)


def _parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(
        description="produce and compare isolated same-commit parity evidence"
    )
    commands = result.add_subparsers(dest="command", required=True)
    local = commands.add_parser("local", help="produce tokenless oracle/Greenlit evidence")
    _common(local, binary=True)
    github = commands.add_parser(
        "github", help="produce credential-only GitHub evidence without Greenlit"
    )
    _common(github, binary=False)

    def run_id(value: str) -> int:
        try:
            return _positive_run_id(value, "--run-id")
        except GateError as error:
            raise argparse.ArgumentTypeError(str(error)) from error

    github.add_argument("--run-id", type=run_id)
    compare = commands.add_parser(
        "compare", help="compare a merged credential-free evidence set"
    )
    _common(compare, binary=True)
    canary = commands.add_parser("credential-canary", help=argparse.SUPPRESS)
    canary.add_argument("--repository-root", type=Path, required=True)
    canary.add_argument("--expected-launcher-sha256", required=True)
    return result


def _selected_run(
    credential: GitHubCredential,
    argument: int | None,
    source_commit: str,
) -> RunIdentity:
    environment_value = os.environ.get("PARITY_GITHUB_RUN_ID")
    environment_id = (
        _positive_run_id(environment_value, "PARITY_GITHUB_RUN_ID")
        if environment_value is not None
        else None
    )
    if argument is not None and environment_id is not None and argument != environment_id:
        raise GateError("--run-id and PARITY_GITHUB_RUN_ID disagree")
    explicit = argument if argument is not None else environment_id
    if explicit is not None:
        return certify_run(credential, source_commit, explicit)
    if os.environ.get("GITHUB_ACTIONS") != "true":
        raise GateError(
            "live parity outside GitHub Actions requires --run-id or "
            "PARITY_GITHUB_RUN_ID"
        )
    if os.environ.get("GITHUB_REPOSITORY") != REPOSITORY_ID:
        raise GateError(f"GITHUB_REPOSITORY must be exactly {REPOSITORY_ID}")
    if os.environ.get("GITHUB_SHA") != source_commit:
        raise GateError("GITHUB_SHA differs from the exact live parity source")
    return discover_run(credential, source_commit)


def _require_credential_free(command: str) -> None:
    forbidden = (
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
        TOKEN_DESCRIPTOR,
    )
    if any(key in os.environ for key in forbidden):
        raise GateError(f"{command} live parity mode must be credential-free")


def _credential_canary(credential: GitHubCredential) -> None:
    if credential.token != CANARY_TOKEN:
        raise GateError("live parity credential canary bytes changed")
    forbidden = ("GH_TOKEN", "GITHUB_TOKEN", TOKEN_DESCRIPTOR)
    if any(key in os.environ for key in forbidden):
        raise GateError("live parity credential canary retained its environment")
    descriptor_prefix = f"{TOKEN_DESCRIPTOR}=".encode()
    environment = Path("/proc/self/environ").read_bytes()
    descriptor_values = [
        item[len(descriptor_prefix) :]
        for item in environment.split(b"\0")
        if item.startswith(descriptor_prefix)
    ]
    for raw in descriptor_values:
        if not raw.isdigit() or Path(f"/proc/self/fd/{raw.decode()}").exists():
            raise GateError("live parity credential canary descriptor remained open")
    with os.scandir("/proc/self/fd") as entries:
        descriptors = tuple(entry.name for entry in entries)
    for name in descriptors:
        if not name.isdecimal() or int(name, 10) <= 2:
            continue
        try:
            target = os.readlink(f"/proc/self/fd/{name}")
        except OSError:
            continue
        if target.startswith(("/usr/lib/", "/lib/")):
            continue
        raise GateError("live parity credential canary inherited an unexpected descriptor")
    if CANARY_TOKEN.encode() in environment:
        raise GateError("live parity credential canary remained in procfs")


def _canary_repository(path: Path, expected_sha256: str) -> None:
    repository = Path(os.path.abspath(os.fspath(path)))
    if not path.is_absolute() or repository != path or not repository.is_dir():
        raise GateError("credential canary repository must be normalized and absolute")
    launcher = repository / "tools" / "check-live-parity"
    if launcher.is_symlink() or not launcher.is_file():
        raise GateError("credential recipient is not one regular launcher")
    if len(expected_sha256) != 64 or any(
        character not in "0123456789abcdef" for character in expected_sha256
    ):
        raise GateError("credential canary launcher digest is malformed")
    digest = hashlib.sha256()
    with launcher.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    environment = credential_free_environment()
    commit = run_command(
        [
            "/usr/bin/git",
            "-c",
            "credential.helper=",
            "-c",
            f"safe.directory={repository}",
            "rev-parse",
            "--verify",
            "HEAD",
        ],
        cwd=repository,
        timeout=10,
        environment=environment,
        capture_stdout=True,
        stdout_limit=128,
    ).stdout.decode("ascii", errors="strict").strip()
    if digest.hexdigest() != expected_sha256 or os.environ.get(
        "GREENLIT_BUILD_COMMIT"
    ) != commit:
        raise GateError("credential recipient differs from the exact source identity")
    if Path(sys.argv[0]).resolve(strict=True) != launcher:
        raise GateError("credential recipient is not the exact tracked launcher")


def main() -> int:
    """Run exactly one isolated parity role."""

    arguments = _parser().parse_args()
    source_commit = ""
    run: RunIdentity | None = None
    try:
        credential = (
            GitHubCredential.capture()
            if arguments.command in ("github", "credential-canary")
            else None
        )
        if arguments.command == "credential-canary":
            if credential is None:
                raise GateError("credential canary was not established")
            _credential_canary(credential)
            _canary_repository(
                arguments.repository_root,
                arguments.expected_launcher_sha256,
            )
            print("live parity production credential boundary canary passed")
            return 0
        if arguments.command != "github":
            _require_credential_free(arguments.command)
        repository, source_commit = exact_source(arguments.repository_root)
        require_empty = arguments.command != "compare"
        with OutputRoot.bind(
            arguments.output_root,
            repository,
            require_empty=require_empty,
        ) as output:
            if arguments.command == "local":
                binary = BinaryBinding.bind(arguments.greenlit_binary)
                produce_local(repository, binary, output, source_commit)
            elif arguments.command == "github":
                if credential is None:
                    raise GateError("GitHub credential boundary was not established")
                run = _selected_run(credential, arguments.run_id, source_commit)
                produce_github(repository, output, source_commit, run, credential)
            else:
                binary = BinaryBinding.bind(arguments.greenlit_binary)
                compare_evidence(repository, binary, output, source_commit)
    except (GateError, OSError) as error:
        print(f"live parity gate failed: {error}", file=sys.stderr)
        return 1
    suffix = f", run {run.run_id}" if run is not None else ""
    print(
        f"live parity {arguments.command} passed: source {source_commit}{suffix}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
