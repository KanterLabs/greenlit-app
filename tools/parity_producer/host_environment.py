"""Minimal host-tool discovery and process environments for parity."""

from __future__ import annotations

import os
import pwd
import stat
from pathlib import Path

from parity_producer.common import ProducerError


SYSTEM_TOOL_DIRECTORIES = (Path("/usr/bin"), Path("/bin"), Path("/usr/local/bin"))


def trusted_system_tools(names: tuple[str, ...]) -> dict[str, str]:
    """Resolve required standard tools only from fixed system directories."""
    return {
        name: _trusted_executable(
            name, tuple(directory / name for directory in SYSTEM_TOOL_DIRECTORIES)
        )
        for name in names
    }


def minimal_path(executables: list[str]) -> str:
    """Build a deterministic PATH from already validated executable parents."""
    directories: list[str] = []
    for executable in executables:
        parent = str(Path(executable).parent)
        if parent not in directories:
            directories.append(parent)
    for directory in SYSTEM_TOOL_DIRECTORIES:
        text = str(directory)
        if text not in directories:
            directories.append(text)
    return os.pathsep.join(directories)


def minimal_environment(
    *,
    home: Path,
    executables: list[str],
    extra: dict[str, str] | None = None,
) -> dict[str, str]:
    """Create a credential-free environment without inherited execution hooks."""
    try:
        identity = pwd.getpwuid(os.getuid())
    except KeyError as error:
        raise ProducerError("cannot identify the parity execution account") from error
    environment = {
        "HOME": str(home),
        "LANG": "C",
        "LC_ALL": "C",
        "LOGNAME": identity.pw_name,
        "PATH": minimal_path(executables),
        "TZ": "UTC",
        "USER": identity.pw_name,
    }
    environment.update(extra or {})
    return environment


def _trusted_executable(name: str, candidates: tuple[Path, ...]) -> str:
    for candidate in candidates:
        try:
            resolved = candidate.resolve(strict=True)
            metadata = resolved.stat()
        except OSError:
            continue
        if (
            stat.S_ISREG(metadata.st_mode)
            and metadata.st_uid == 0
            and metadata.st_mode & 0o111
            and stat.S_IMODE(metadata.st_mode) & 0o022 == 0
        ):
            return str(candidate)
    raise ProducerError(f"parity execution lacks trusted executable {name!r}")
