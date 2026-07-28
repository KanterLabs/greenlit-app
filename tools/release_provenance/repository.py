"""Exact, config-independent source and Cargo package projection checks."""

from __future__ import annotations

import hashlib
import os
import stat
import subprocess
from pathlib import Path

from .common import ProvenanceError, validate_source


GIT = Path("/usr/bin/git")
MAX_GIT_OUTPUT = 32 * 1024 * 1024


def _git_environment() -> dict[str, str]:
    """Return a minimal Git environment without ambient config or redirects."""

    return {
        "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_NO_REPLACE_OBJECTS": "1",
        "GIT_TERMINAL_PROMPT": "0",
        "HOME": "/nonexistent",
        "LANG": "C",
        "LC_ALL": "C",
        "PATH": "/usr/bin:/bin",
    }


def _git(repository: Path, arguments: list[str], *, timeout: int = 30) -> bytes:
    """Run one bounded, literal Git command against the trusted worktree."""

    if not GIT.is_file():
        raise ProvenanceError(f"trusted Git executable is unavailable: {GIT}")
    command = [
        os.fspath(GIT),
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.untrackedCache=false",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.excludesFile=/dev/null",
        "-c",
        "core.attributesFile=/dev/null",
        "-c",
        f"core.worktree={repository}",
        "-c",
        "core.bare=false",
        "-C",
        os.fspath(repository),
        *arguments,
    ]
    try:
        result = subprocess.run(
            command,
            env=_git_environment(),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=timeout,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ProvenanceError(f"could not inspect repository identity: {error}") from error
    if len(result.stdout) > MAX_GIT_OUTPUT or len(result.stderr) > MAX_GIT_OUTPUT:
        raise ProvenanceError("Git repository inspection exceeded its output limit")
    if result.returncode != 0:
        diagnostic = result.stderr[:4096].decode("utf-8", errors="replace").strip()
        raise ProvenanceError(
            f"git {' '.join(arguments)} failed"
            + (f": {diagnostic}" if diagnostic else "")
        )
    return result.stdout


def _text(repository: Path, arguments: list[str]) -> str:
    try:
        return _git(repository, arguments).decode("ascii").strip()
    except UnicodeDecodeError as error:
        raise ProvenanceError("Git returned a non-ASCII source identity") from error


def _records(data: bytes, label: str) -> list[str]:
    """Decode one NUL-delimited Git record stream."""

    if data and not data.endswith(b"\0"):
        raise ProvenanceError(f"{label} is not NUL terminated")
    try:
        return [
            record.decode("utf-8", errors="strict")
            for record in data.split(b"\0")
            if record
        ]
    except UnicodeDecodeError as error:
        raise ProvenanceError(f"{label} contains a non-UTF-8 path") from error


def _tree(repository: Path) -> dict[str, tuple[str, str]]:
    result: dict[str, tuple[str, str]] = {}
    for record in _records(
        _git(repository, ["ls-tree", "-r", "-z", "--full-tree", "HEAD"]),
        "Git tree",
    ):
        try:
            metadata, path = record.split("\t", 1)
            mode, kind, object_id = metadata.split(" ", 2)
        except ValueError as error:
            raise ProvenanceError("Git tree contains a malformed record") from error
        if kind != "blob" or mode not in {"100644", "100755", "120000"}:
            raise ProvenanceError(
                f"release source contains unsupported tracked entry {path!r}"
            )
        if path in result:
            raise ProvenanceError(f"Git tree repeats tracked path {path!r}")
        result[path] = (mode, object_id)
    if not result:
        raise ProvenanceError("release source commit has no tracked files")
    return result


def _index(repository: Path) -> dict[str, tuple[str, str]]:
    result: dict[str, tuple[str, str]] = {}
    for record in _records(
        _git(repository, ["ls-files", "--stage", "-z"]),
        "Git index",
    ):
        try:
            metadata, path = record.split("\t", 1)
            mode, object_id, stage = metadata.split(" ", 2)
        except ValueError as error:
            raise ProvenanceError("Git index contains a malformed record") from error
        if stage != "0" or path in result:
            raise ProvenanceError(f"Git index has an unmerged or repeated path {path!r}")
        result[path] = (mode, object_id)
    return result


def _require_normal_flags(repository: Path, expected_paths: set[str]) -> None:
    seen: set[str] = set()
    for record in _records(
        _git(repository, ["ls-files", "-v", "-z"]),
        "Git tracked flags",
    ):
        if len(record) < 3 or record[1] != " ":
            raise ProvenanceError("Git tracked flags contain a malformed record")
        tag, path = record[0], record[2:]
        if tag != "H":
            raise ProvenanceError(
                f"tracked path {path!r} has skip-worktree or assume-unchanged state"
            )
        if path in seen:
            raise ProvenanceError(f"Git tracked flags repeat path {path!r}")
        seen.add(path)
    if seen != expected_paths:
        raise ProvenanceError("Git tracked-flag inventory differs from HEAD")


def _tracked_path(repository: Path, relative: str) -> tuple[Path, os.stat_result]:
    current = repository
    parts = Path(relative).parts
    if not parts or any(part in {"", ".", ".."} for part in parts):
        raise ProvenanceError(f"Git contains unsafe tracked path {relative!r}")
    for part in parts[:-1]:
        current /= part
        try:
            metadata = os.lstat(current)
        except OSError as error:
            raise ProvenanceError(
                f"could not inspect tracked parent {relative!r}: {error}"
            ) from error
        if not stat.S_ISDIR(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode):
            raise ProvenanceError(
                f"tracked path {relative!r} crosses a non-directory or symlink"
            )
    path = current / parts[-1]
    try:
        return path, os.lstat(path)
    except OSError as error:
        raise ProvenanceError(f"tracked path {relative!r} is absent: {error}") from error


def _working_blob(repository: Path, relative: str, mode: str) -> str:
    path, metadata = _tracked_path(repository, relative)
    if mode == "120000":
        if not stat.S_ISLNK(metadata.st_mode):
            raise ProvenanceError(f"tracked symlink {relative!r} changed type")
        data = os.fsencode(os.readlink(path))
        digest = hashlib.sha1()
        digest.update(f"blob {len(data)}\0".encode("ascii"))
        digest.update(data)
        return digest.hexdigest()
    if not stat.S_ISREG(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode):
        raise ProvenanceError(f"tracked file {relative!r} changed type")
    executable = bool(metadata.st_mode & 0o111)
    if executable != (mode == "100755"):
        raise ProvenanceError(f"tracked file mode differs from HEAD: {relative!r}")
    flags = os.O_RDONLY
    if hasattr(os, "O_CLOEXEC"):
        flags |= os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise ProvenanceError(f"could not open tracked file {relative!r}: {error}") from error
    try:
        opened = os.fstat(descriptor)
        if (
            not stat.S_ISREG(opened.st_mode)
            or (opened.st_dev, opened.st_ino)
            != (metadata.st_dev, metadata.st_ino)
        ):
            raise ProvenanceError(f"tracked file raced during open: {relative!r}")
        digest = hashlib.sha1()
        digest.update(f"blob {opened.st_size}\0".encode("ascii"))
        while chunk := os.read(descriptor, 1024 * 1024):
            digest.update(chunk)
        final = os.fstat(descriptor)
        if (
            opened.st_dev,
            opened.st_ino,
            opened.st_size,
            opened.st_mtime_ns,
            opened.st_ctime_ns,
        ) != (
            final.st_dev,
            final.st_ino,
            final.st_size,
            final.st_mtime_ns,
            final.st_ctime_ns,
        ):
            raise ProvenanceError(f"tracked file changed while read: {relative!r}")
        return digest.hexdigest()
    finally:
        os.close(descriptor)


def _require_allowed_untracked(repository: Path) -> None:
    records = _records(
        _git(
            repository,
            [
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--ignored=matching",
            ],
        ),
        "Git status",
    )
    for record in records:
        if len(record) < 4 or record[2] != " ":
            raise ProvenanceError("Git status contains a malformed record")
        status, path = record[:2], record[3:]
        if status not in {"??", "!!"}:
            raise ProvenanceError(f"release source has tracked change {record!r}")
        normalized = path[:-1] if path.endswith("/") else path
        if normalized != "target" and not normalized.startswith("target/"):
            raise ProvenanceError(
                f"release source contains untracked or ignored path {path!r}"
            )


def verify_repository(repository: Path, expected_source: str) -> None:
    """Require exact HEAD, index, working bytes, and an allowlisted output tree."""

    validate_source(expected_source, "--expected-source")
    git_entry = repository / ".git"
    try:
        metadata = os.lstat(git_entry)
    except OSError as error:
        raise ProvenanceError(f"repository has no inspectable .git directory: {error}") from error
    if not stat.S_ISDIR(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode):
        raise ProvenanceError("linked worktrees and .git files are not release sources")
    top = Path(_text(repository, ["rev-parse", "--show-toplevel"])).resolve()
    if top != repository:
        raise ProvenanceError("repository root is not the exact Git worktree root")
    if _text(repository, ["rev-parse", "--show-object-format"]) != "sha1":
        raise ProvenanceError("release source repository must use SHA-1 object identities")
    resolved = _text(
        repository,
        ["rev-parse", "--verify", f"{expected_source}^{{commit}}"],
    )
    head = _text(repository, ["rev-parse", "--verify", "HEAD^{commit}"])
    if resolved != expected_source or head != expected_source:
        raise ProvenanceError(
            f"repository HEAD/source is not expected commit {expected_source}"
        )
    if _text(repository, ["for-each-ref", "--format=%(refname)", "refs/replace"]):
        raise ProvenanceError("release source repository contains replacement refs")
    tree = _tree(repository)
    if _index(repository) != tree:
        raise ProvenanceError("release source index differs from exact HEAD")
    _require_normal_flags(repository, set(tree))
    for relative, (mode, object_id) in tree.items():
        if _working_blob(repository, relative, mode) != object_id:
            raise ProvenanceError(
                f"tracked working bytes differ from HEAD: {relative!r}"
            )
    _require_allowed_untracked(repository)
