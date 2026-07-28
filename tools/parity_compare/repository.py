"""Pinned, environment-independent Git access for parity provenance."""

from __future__ import annotations

import os
import re
import shutil
import stat
import subprocess
from dataclasses import dataclass
from pathlib import Path

from . import ContractError
from .repository_worktree import WorktreeContractError, validate_exact_worktree


COMMIT = re.compile(r"^[0-9a-f]{40}$")
_PathIdentity = tuple[int, int, int]


class RepositoryError(ContractError):
    """The trusted checkout cannot provide immutable Git evidence."""


@dataclass(frozen=True)
class RepositoryIdentity:
    """One real checkout, Git directory, and HEAD captured at command start."""

    root: Path
    git_dir: Path
    head: str
    _root_identity: _PathIdentity
    _git_dir_identity: _PathIdentity


def _safe_environment() -> dict[str, str]:
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.upper().startswith("GIT_")
    }
    environment.update(
        {
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_OPTIONAL_LOCKS": "0",
            "LC_ALL": "C",
        }
    )
    return environment


def _git_executable() -> str:
    executable = shutil.which("git", path=os.defpath)
    if executable is None:
        raise RepositoryError(
            "cannot verify immutable provenance: git is unavailable"
        )
    return executable


def _run_git(
    root: Path,
    git_dir: Path,
    *arguments: str,
) -> subprocess.CompletedProcess[bytes]:
    command = [
        _git_executable(),
        "--no-replace-objects",
        "--no-optional-locks",
        "-c",
        "core.fsmonitor=false",
        f"--git-dir={git_dir}",
        f"--work-tree={root}",
        "-C",
        str(root),
        *arguments,
    ]
    try:
        return subprocess.run(
            command,
            check=False,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=_safe_environment(),
            timeout=30,
        )
    except subprocess.TimeoutExpired as error:
        raise RepositoryError(
            "cannot verify immutable provenance: git command timed out"
        ) from error
    except OSError as error:
        raise RepositoryError(
            f"cannot execute git for provenance validation: {error}"
        ) from error


def _successful_output(root: Path, git_dir: Path, *arguments: str) -> bytes:
    completed = _run_git(root, git_dir, *arguments)
    if completed.returncode != 0:
        detail = completed.stderr.decode("utf-8", errors="replace").strip()
        raise RepositoryError(
            "cannot verify immutable provenance with git: "
            f"{detail or 'git command failed'}"
        )
    return completed.stdout


def _path_identity(path: Path, label: str) -> _PathIdentity:
    try:
        metadata = path.lstat()
    except OSError as error:
        raise RepositoryError(f"{label} is not accessible: {error}") from error
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
        raise RepositoryError(f"{label} must be a real non-symlink directory")
    return metadata.st_dev, metadata.st_ino, stat.S_IFMT(metadata.st_mode)


def _resolved(path: bytes, label: str) -> Path:
    try:
        text = path.decode("utf-8", errors="strict").strip()
        if not text:
            raise ValueError("empty path")
        return Path(text).resolve(strict=True)
    except (OSError, RuntimeError, UnicodeDecodeError, ValueError) as error:
        raise RepositoryError(f"git returned an invalid {label}") from error


def _single_line(raw: bytes, label: str) -> str:
    try:
        text = raw.decode("ascii", errors="strict").strip()
    except UnicodeDecodeError as error:
        raise RepositoryError(f"git returned a non-ASCII {label}") from error
    if "\n" in text or "\r" in text or not text:
        raise RepositoryError(f"git returned an invalid {label}")
    return text


def _validate_layout(root: Path, git_dir: Path) -> str:
    top_level = _resolved(
        _successful_output(root, git_dir, "rev-parse", "--show-toplevel"),
        "repository root",
    )
    actual_git_dir = _resolved(
        _successful_output(root, git_dir, "rev-parse", "--absolute-git-dir"),
        "Git directory",
    )
    if top_level != root or actual_git_dir != git_dir:
        raise RepositoryError(
            "trusted repository root and its direct .git directory do not "
            "identify the same checkout"
        )
    inside = _single_line(
        _successful_output(root, git_dir, "rev-parse", "--is-inside-work-tree"),
        "work-tree result",
    )
    if inside != "true":
        raise RepositoryError("trusted repository is not a Git work tree")
    head = _single_line(
        _successful_output(root, git_dir, "rev-parse", "--verify", "HEAD^{commit}"),
        "HEAD commit",
    )
    if COMMIT.fullmatch(head) is None:
        raise RepositoryError(
            "trusted repository HEAD is not a full lowercase SHA-1 commit"
        )
    return head


def _validate_clean(root: Path, git_dir: Path) -> None:
    tracked = _successful_output(
        root,
        git_dir,
        "ls-files",
        "-v",
        "-z",
    )
    concealed = [
        entry
        for entry in tracked.split(b"\0")
        if entry and not entry.startswith(b"H ")
    ]
    if concealed:
        raise RepositoryError(
            "trusted repository has tracked index flags that can conceal "
            "worktree changes"
        )
    try:
        validate_exact_worktree(
            root,
            lambda *arguments: _successful_output(
                root, git_dir, *arguments
            ),
        )
    except WorktreeContractError as error:
        raise RepositoryError(str(error)) from error


def bind_repository(repository: Path) -> RepositoryIdentity:
    """Bind a real repository root, direct Git directory, and exact HEAD once."""
    supplied = repository.absolute()
    _path_identity(supplied, "trusted repository root")
    try:
        root = supplied.resolve(strict=True)
    except (OSError, RuntimeError) as error:
        raise RepositoryError(
            f"trusted repository root cannot be resolved: {error}"
        ) from error
    root_identity = _path_identity(root, "trusted repository root")
    git_dir = root / ".git"
    git_dir_identity = _path_identity(
        git_dir, "trusted repository root's direct .git directory"
    )
    head = _validate_layout(root, git_dir)
    _validate_clean(root, git_dir)
    identity = RepositoryIdentity(
        root=root,
        git_dir=git_dir,
        head=head,
        _root_identity=root_identity,
        _git_dir_identity=git_dir_identity,
    )
    assert_repository_unchanged(identity)
    return identity


def _assert_path_identity(identity: RepositoryIdentity) -> None:
    if (
        _path_identity(identity.root, "trusted repository root")
        != identity._root_identity
        or _path_identity(
            identity.git_dir, "trusted repository root's direct .git directory"
        )
        != identity._git_dir_identity
    ):
        raise RepositoryError(
            "trusted repository identity changed during parity validation"
        )


def git_output(identity: RepositoryIdentity, *arguments: str) -> bytes:
    """Run one read-only Git command against the bound checkout."""
    _assert_path_identity(identity)
    return _successful_output(identity.root, identity.git_dir, *arguments)


def assert_repository_unchanged(identity: RepositoryIdentity) -> None:
    """Require the bound checkout, Git directory, and HEAD to remain unchanged."""
    _assert_path_identity(identity)
    if _validate_layout(identity.root, identity.git_dir) != identity.head:
        raise RepositoryError(
            "trusted repository HEAD changed during parity validation"
        )
    _validate_clean(identity.root, identity.git_dir)


__all__ = [
    "RepositoryError",
    "RepositoryIdentity",
    "assert_repository_unchanged",
    "bind_repository",
    "git_output",
]
