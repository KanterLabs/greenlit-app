"""Release-binary execution and retained-evidence projection."""

from __future__ import annotations

import os
import re
import stat
from pathlib import Path

from parity_producer.bounded_process import run_bounded
from parity_producer.common import (
    AUTHORITATIVE_REPOSITORY,
    COMMIT,
    GIT_EXECUTABLE,
    GIT_FIXED_OPTIONS,
    MAX_GIT_OUTPUT_BYTES,
    WORKFLOW_PATH,
    ProducerError,
    git_environment,
    read_regular_file,
    sha256_bytes,
)
from parity_producer.capture import Production
from parity_producer.host_environment import (
    minimal_environment,
    trusted_system_tools,
)
from parity_producer.local_binary import PinnedReleaseBinary
from parity_producer.retained import project_local_evidence


RUN_TIMEOUT_SECONDS = 15 * 60
RUN_STDOUT_LIMIT_BYTES = 8 * 1024 * 1024
RUN_STDERR_LIMIT_BYTES = 8 * 1024 * 1024


def produce_local(
    binary: Path,
    repository: Path,
    home: Path,
    repository_id: str,
    trusted_source_commit: str,
) -> Production:
    """Run the exact seed through a release binary and project its evidence."""
    binary = _safe_absolute(binary, "release binary path")
    repository = _safe_absolute(repository, "release checkout")
    home = _safe_absolute(home, "local parity HOME")
    if _git(repository, "rev-parse", "--show-toplevel") != str(repository):
        raise ProducerError("local parity checkout must be the exact Git worktree root")
    if _within(binary, repository) or _within(home, repository):
        raise ProducerError("release target and isolated HOME must be outside the checkout")
    if _within(binary, home) or _within(home, binary.parent.parent):
        raise ProducerError("release target and isolated HOME must be disjoint")
    commit = _git(repository, "rev-parse", "HEAD")
    if COMMIT.fullmatch(commit) is None:
        raise ProducerError("repository HEAD is not a full lowercase Git commit")
    if commit != trusted_source_commit:
        raise ProducerError("release checkout HEAD differs from trusted source commit")
    _require_ordinary_index(repository)
    if _git(
        repository,
        "status",
        "--porcelain=v1",
        "--untracked-files=all",
        "--ignored=matching",
    ):
        raise ProducerError(
            "local parity requires a pristine tracked-only checkout with no ignored files"
        )
    if repository_id != AUTHORITATIVE_REPOSITORY:
        raise ProducerError(
            f"repository identity must be {AUTHORITATIVE_REPOSITORY!r}"
        )
    if COMMIT.fullmatch(trusted_source_commit) is None:
        raise ProducerError("trusted source commit must be full lowercase SHA")
    workflow = repository / WORKFLOW_PATH
    workflow_bytes = _read_workflow(workflow)
    if workflow_bytes != _git_bytes(
        repository, "cat-file", "blob", f"{commit}:{WORKFLOW_PATH}"
    ):
        raise ProducerError("working seed workflow differs from the trusted Git blob")
    _prepare_home(home)
    runtime_tools = trusted_system_tools(("bash", "git"))
    environment = minimal_environment(
        home=home,
        executables=list(runtime_tools.values()),
        extra={"GIT_TERMINAL_PROMPT": "0", "GCM_INTERACTIVE": "Never"},
    )

    with PinnedReleaseBinary.open(binary) as release:
        release.validate_version(commit, environment)
        _require_empty_home(home)
        before = _run_directories(home)
        command = [
            release.command,
            "run",
            "--allow-degraded",
            "--no-daemon",
            "--no-input",
            "--format",
            "jsonl",
            "--log-mode",
            "full",
            "--color",
            "never",
            "--event",
            "push",
            "--job",
            "shell",
            "--workflow",
            WORKFLOW_PATH,
        ]
        result = run_bounded(
            command,
            label="release-built local seed",
            cwd=repository,
            environment=environment,
            pass_fds=(release.descriptor,),
            timeout_seconds=RUN_TIMEOUT_SECONDS,
            stdout_limit=RUN_STDOUT_LIMIT_BYTES,
            stderr_limit=RUN_STDERR_LIMIT_BYTES,
        )
        if result.returncode != 0:
            diagnostic = _bounded_diagnostic(result.stderr or result.stdout)
            raise ProducerError(
                "release-built local seed did not pass"
                + (f": {diagnostic}" if diagnostic else "")
            )
        release.verify_unchanged()
        binary_sha256 = release.digest

    after = _run_directories(home)
    created = sorted(after - before)
    if len(created) != 1:
        raise ProducerError(
            "release-built local seed must create exactly one retained run "
            f"(observed {len(created)})"
        )
    run_directory = home / ".litci" / "runs" / created[0]
    if _git(
        repository,
        "status",
        "--porcelain=v1",
        "--untracked-files=all",
        "--ignored=matching",
    ):
        raise ProducerError("release-built seed modified its pristine source checkout")
    _require_ordinary_index(repository)
    return project_local_evidence(
        run_directory=run_directory,
        binary_sha256=binary_sha256,
        repository_name=repository_id,
        expected_commit=commit,
        expected_workflow_sha256=sha256_bytes(workflow_bytes),
    )


def _prepare_home(home: Path) -> None:
    try:
        home.mkdir(mode=0o700, parents=False, exist_ok=True)
        metadata = home.lstat()
    except OSError as error:
        raise ProducerError(f"cannot prepare isolated local parity HOME {home}: {error}") from error
    if (
        not stat.S_ISDIR(metadata.st_mode)
        or stat.S_IMODE(metadata.st_mode) != 0o700
        or metadata.st_uid != os.geteuid()
    ):
        raise ProducerError(
            "local parity HOME must be a real current-user mode-0700 directory"
        )
    _require_empty_home(home)


def _require_empty_home(home: Path) -> None:
    try:
        entries = list(home.iterdir())
    except OSError as error:
        raise ProducerError(f"cannot inspect isolated local parity HOME: {error}") from error
    if entries:
        raise ProducerError("local parity HOME must be empty before execution")


def _run_directories(home: Path) -> set[str]:
    state = home / ".litci"
    runs = home / ".litci" / "runs"
    try:
        state_metadata = state.lstat()
    except FileNotFoundError:
        return set()
    except OSError as error:
        raise ProducerError(f"cannot inspect retained state in {state}: {error}") from error
    _require_private_directory(state, state_metadata)
    try:
        metadata = runs.lstat()
    except FileNotFoundError:
        return set()
    except OSError as error:
        raise ProducerError(f"cannot inspect retained runs in {runs}: {error}") from error
    _require_private_directory(runs, metadata)
    try:
        entries = list(runs.iterdir())
    except OSError as error:
        raise ProducerError(f"cannot enumerate retained runs in {runs}: {error}") from error
    result: set[str] = set()
    for entry in entries:
        try:
            entry_metadata = entry.lstat()
        except OSError as error:
            raise ProducerError(f"cannot inspect retained run {entry}: {error}") from error
        _require_private_directory(entry, entry_metadata)
        result.add(entry.name)
    return result


def _require_private_directory(path: Path, metadata: os.stat_result) -> None:
    if (
        not stat.S_ISDIR(metadata.st_mode)
        or stat.S_IMODE(metadata.st_mode) != 0o700
        or metadata.st_uid != os.geteuid()
    ):
        raise ProducerError(
            f"retained evidence directory must be current-user mode 0700: {path}"
        )


def _require_ordinary_index(repository: Path) -> None:
    entries = [
        entry
        for entry in _git(repository, "ls-files", "-v", "-z").split("\0")
        if entry
    ]
    if any(not entry.startswith("H ") for entry in entries):
        raise ProducerError(
            "local parity forbids skip-worktree, assume-unchanged, "
            "or nonordinary index entries"
        )


def _git(repository: Path, *arguments: str) -> str:
    result = run_bounded(
        [
            GIT_EXECUTABLE,
            *GIT_FIXED_OPTIONS,
            "-C",
            str(repository),
            *arguments,
        ],
        label="local parity Git inspection",
        environment=git_environment(),
        timeout_seconds=30,
        stdout_limit=MAX_GIT_OUTPUT_BYTES,
        stderr_limit=64 * 1024,
    )
    if result.returncode != 0:
        diagnostic = _bounded_diagnostic(result.stderr)
        raise ProducerError(
            "could not inspect parity checkout with git"
            + (f": {diagnostic}" if diagnostic else "")
        )
    try:
        return result.stdout.decode("utf-8").strip()
    except UnicodeDecodeError as error:
        raise ProducerError("git returned non-UTF-8 checkout metadata") from error


def _git_bytes(repository: Path, *arguments: str) -> bytes:
    result = run_bounded(
        [
            GIT_EXECUTABLE,
            *GIT_FIXED_OPTIONS,
            "-C",
            str(repository),
            *arguments,
        ],
        label="trusted workflow Git read",
        environment=git_environment(),
        timeout_seconds=30,
        stdout_limit=MAX_GIT_OUTPUT_BYTES,
        stderr_limit=64 * 1024,
    )
    if result.returncode != 0:
        raise ProducerError("trusted workflow blob is unavailable from Git")
    return result.stdout


def _read_workflow(path: Path) -> bytes:
    raw = read_regular_file(path, "parity seed workflow", 1024 * 1024)
    if not raw:
        raise ProducerError("parity seed workflow is empty")
    return raw


def _safe_absolute(path: Path, source: str) -> Path:
    absolute = Path(os.path.abspath(path))
    current = Path(absolute.anchor)
    for part in absolute.parts[1:]:
        current /= part
        try:
            metadata = current.lstat()
        except FileNotFoundError:
            break
        except OSError as error:
            raise ProducerError(f"cannot inspect {source} component {current}: {error}") from error
        if stat.S_ISLNK(metadata.st_mode):
            raise ProducerError(f"{source} contains symlink component {current}")
    return absolute


def _within(path: Path, directory: Path) -> bool:
    return path == directory or directory in path.parents


def _bounded_diagnostic(raw: bytes) -> str:
    text = raw[:4096].decode("utf-8", errors="replace")
    text = re.sub(r"(?i)(token|secret|password|credential)=[^\s]+", r"\1=[redacted]", text)
    return " ".join(text.split())
