"""Exact, config-independent Git source certification."""

from __future__ import annotations

import os
import stat
import tempfile
from pathlib import Path

from .contract import COMMIT
from .errors import GateError
from .process import run_command
from .source_worktree import parse_index, parse_tree, verify_worktree


GIT = "/usr/bin/git"
MAX_CONTROL_BYTES = 16 * 1024 * 1024
GIT_OPTIONS = (
    "--no-pager",
    "--no-optional-locks",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.untrackedCache=false",
    "-c",
    "core.preloadIndex=false",
    "-c",
    "core.ignoreStat=false",
    "-c",
    "core.fileMode=true",
    "-c",
    "core.symlinks=true",
    "-c",
    "core.attributesFile=/dev/null",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.sparseCheckout=false",
    "-c",
    "core.sparseCheckoutCone=false",
)


def _read_regular(path: Path, source: str, limit: int) -> bytes:
    flags = os.O_RDONLY | os.O_CLOEXEC | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
        try:
            before = os.fstat(descriptor)
            if not stat.S_ISREG(before.st_mode) or before.st_size > limit:
                raise GateError(f"{source} is not a bounded regular file")
            chunks: list[bytes] = []
            remaining = limit + 1
            while remaining > 0:
                chunk = os.read(descriptor, min(64 * 1024, remaining))
                if not chunk:
                    break
                chunks.append(chunk)
                remaining -= len(chunk)
            after = os.fstat(descriptor)
        finally:
            os.close(descriptor)
    except OSError as error:
        raise GateError(f"cannot read {source}: {error}") from error
    value = b"".join(chunks)
    if len(value) > limit or (
        before.st_dev,
        before.st_ino,
        before.st_size,
        before.st_mtime_ns,
        before.st_ctime_ns,
    ) != (
        after.st_dev,
        after.st_ino,
        after.st_size,
        after.st_mtime_ns,
        after.st_ctime_ns,
    ):
        raise GateError(f"{source} changed while it was read")
    return value


def _commit_text(raw: bytes, source: str) -> str:
    try:
        value = raw.decode("ascii", errors="strict")
    except UnicodeDecodeError as error:
        raise GateError(f"{source} is not ASCII") from error
    if COMMIT.fullmatch(value) is None:
        raise GateError(f"{source} is not a full lowercase commit")
    return value


def _head_commit(git_directory: Path) -> str:
    raw_head = _read_regular(git_directory / "HEAD", "Git HEAD", 4096)
    if raw_head.endswith(b"\n"):
        raw_head = raw_head[:-1]
    if b"\n" in raw_head or b"\r" in raw_head:
        raise GateError("Git HEAD is not one canonical line")
    if len(raw_head) == 40:
        return _commit_text(raw_head, "Git HEAD")
    prefix = b"ref: "
    if not raw_head.startswith(prefix):
        raise GateError("Git HEAD is neither a commit nor a symbolic ref")
    reference = raw_head[len(prefix) :]
    if (
        not reference.startswith(b"refs/")
        or b"\0" in reference
        or b"\n" in reference
        or any(part in {b"", b".", b".."} for part in reference.split(b"/"))
    ):
        raise GateError("Git HEAD contains an unsafe symbolic ref")
    loose = git_directory / os.fsdecode(reference)
    if loose.exists():
        raw_commit = _read_regular(loose, "Git HEAD ref", 4096)
        if raw_commit.endswith(b"\n"):
            raw_commit = raw_commit[:-1]
        if b"\n" in raw_commit or b"\r" in raw_commit:
            raise GateError("Git HEAD ref is not one canonical line")
        return _commit_text(raw_commit, "Git HEAD ref")
    packed = _read_regular(
        git_directory / "packed-refs",
        "Git packed refs",
        MAX_CONTROL_BYTES,
    )
    matches: list[bytes] = []
    for line in packed.splitlines():
        if not line or line.startswith((b"#", b"^")):
            continue
        fields = line.split(b" ", 1)
        if len(fields) == 2 and fields[1] == reference:
            matches.append(fields[0])
    if len(matches) != 1:
        raise GateError("Git HEAD symbolic ref does not resolve exactly once")
    return _commit_text(matches[0], "Git packed HEAD ref")


def _git_environment(
    scratch_git: Path,
    repository: Path,
    object_directory: Path,
) -> dict[str, str]:
    return {
        "PATH": "/usr/bin:/bin",
        "LANG": "C.UTF-8",
        "LC_ALL": "C.UTF-8",
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_SYSTEM": "/dev/null",
        "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_CONFIG_COUNT": "0",
        "GIT_NO_REPLACE_OBJECTS": "1",
        "GIT_TERMINAL_PROMPT": "0",
        "GIT_OPTIONAL_LOCKS": "0",
        "GIT_LITERAL_PATHSPECS": "1",
        "GIT_DIR": os.fspath(scratch_git),
        "GIT_WORK_TREE": os.fspath(repository),
        "GIT_ALTERNATE_OBJECT_DIRECTORIES": os.fspath(object_directory),
    }


def _write_control_file(path: Path, value: bytes) -> None:
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC
    descriptor = os.open(path, flags, 0o600)
    try:
        offset = 0
        while offset < len(value):
            offset += os.write(descriptor, value[offset:])
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def _git(
    scratch_git: Path,
    repository: Path,
    object_directory: Path,
    *arguments: str,
) -> bytes:
    result = run_command(
        [GIT, *GIT_OPTIONS, *arguments],
        cwd=repository,
        timeout=30,
        environment=_git_environment(scratch_git, repository, object_directory),
        capture_stdout=True,
        stdout_limit=MAX_CONTROL_BYTES,
    )
    return result.stdout


def exact_source(path: Path) -> tuple[Path, str]:
    """Verify index, working bytes, modes, and HEAD without repository config."""

    try:
        repository = path.absolute()
        if repository.resolve(strict=True) != repository:
            raise GateError("repository root contains a symlink component")
        metadata = repository.lstat()
    except OSError as error:
        raise GateError(f"cannot inspect repository root: {error}") from error
    if not stat.S_ISDIR(metadata.st_mode):
        raise GateError("repository root is not a real directory")
    git_directory = repository / ".git"
    try:
        git_metadata = git_directory.lstat()
    except OSError as error:
        raise GateError(f"repository has no direct .git directory: {error}") from error
    if not stat.S_ISDIR(git_metadata.st_mode) or stat.S_ISLNK(git_metadata.st_mode):
        raise GateError("linked worktrees and .git files are not accepted")
    if (git_directory / "commondir").exists():
        raise GateError("linked worktrees are not accepted")
    object_directory = git_directory / "objects"
    try:
        object_metadata = object_directory.lstat()
    except OSError as error:
        raise GateError(f"cannot inspect Git object directory: {error}") from error
    if not stat.S_ISDIR(object_metadata.st_mode) or stat.S_ISLNK(object_metadata.st_mode):
        raise GateError("Git object directory must be a direct real directory")
    for alternate_name in ("alternates", "http-alternates"):
        if (object_directory / "info" / alternate_name).exists():
            raise GateError("Git object alternates are not accepted for live parity")
    source_commit = _head_commit(git_directory)
    supplied = os.environ.get("GREENLIT_BUILD_COMMIT")
    if supplied != source_commit:
        raise GateError(
            "GREENLIT_BUILD_COMMIT must equal the exact live parity source HEAD"
        )
    index = _read_regular(git_directory / "index", "Git index", MAX_CONTROL_BYTES)
    with tempfile.TemporaryDirectory(prefix="greenlit-live-parity-git.") as raw:
        scratch_git = Path(raw) / "git"
        scratch_git.mkdir(mode=0o700)
        (scratch_git / "objects").mkdir(mode=0o700)
        (scratch_git / "refs").mkdir(mode=0o700)
        _write_control_file(scratch_git / "HEAD", source_commit.encode("ascii") + b"\n")
        _write_control_file(
            scratch_git / "config",
            (
                b"[core]\n"
                b"\trepositoryformatversion = 0\n"
                b"\tbare = false\n"
                b"\tfilemode = true\n"
                b"\tsymlinks = true\n"
                b"\tfsmonitor = false\n"
                b"\tuntrackedCache = false\n"
                b"\thooksPath = /dev/null\n"
            ),
        )
        _write_control_file(scratch_git / "index", index)
        object_type = _git(
            scratch_git,
            repository,
            object_directory,
            "cat-file",
            "-t",
            source_commit,
        ).strip()
        if object_type != b"commit":
            raise GateError("repository HEAD does not name a commit object")
        tree = parse_tree(
            _git(
                scratch_git,
                repository,
                object_directory,
                "ls-tree",
                "-rz",
                "--full-tree",
                source_commit,
            )
        )
        indexed = parse_index(
            _git(
                scratch_git,
                repository,
                object_directory,
                "ls-files",
                "--stage",
                "-v",
                "-z",
            )
        )
    if indexed != tree:
        raise GateError("Git index content or modes differ from exact HEAD")
    verify_worktree(repository, tree)
    if _head_commit(git_directory) != source_commit:
        raise GateError("Git HEAD changed during live parity source verification")
    if _read_regular(git_directory / "index", "Git index", MAX_CONTROL_BYTES) != index:
        raise GateError("Git index changed during live parity source verification")
    return repository, source_commit
