"""Linux descendant containment for release-binary identification."""

from __future__ import annotations

import ctypes
import os
import signal
import time
from pathlib import Path


PR_SET_CHILD_SUBREAPER = 36


class ProcessContainmentError(ValueError):
    """The comparator cannot contain release-binary descendants."""


def _process_table() -> dict[int, tuple[int, int]]:
    result: dict[int, tuple[int, int]] = {}
    try:
        entries = os.scandir("/proc")
    except OSError as error:
        raise ProcessContainmentError(
            f"cannot inspect release-binary descendants: {error}"
        ) from error
    with entries:
        for entry in entries:
            if not entry.name.isdecimal():
                continue
            try:
                raw = Path(entry.path, "stat").read_bytes()
                fields = raw[raw.rfind(b")") + 2 :].split()
                result[int(entry.name)] = (int(fields[1]), int(fields[2]))
            except (IndexError, OSError, ValueError):
                continue
    return result


def _descendants(parent: int) -> dict[int, tuple[int, int]]:
    table = _process_table()
    known = {parent}
    result: dict[int, tuple[int, int]] = {}
    changed = True
    while changed:
        changed = False
        for process_id, identity in table.items():
            if process_id not in known and identity[0] in known:
                known.add(process_id)
                result[process_id] = identity
                changed = True
    return result


def establish_process_boundary() -> None:
    """Become a subreaper and require exclusive ownership of descendants."""

    try:
        libc = ctypes.CDLL(None, use_errno=True)
        result = libc.prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0)
    except (AttributeError, OSError) as error:
        raise ProcessContainmentError(
            f"cannot establish release-binary containment: {error}"
        ) from error
    if result != 0:
        raise ProcessContainmentError(
            "cannot establish release-binary containment: "
            f"errno {ctypes.get_errno()}"
        )
    if _descendants(os.getpid()):
        raise ProcessContainmentError(
            "release-binary process boundary already has descendants"
        )


def terminate_descendants(process_id: int) -> bool:
    """Terminate and reap every descendant, returning whether any survived."""

    found = False
    deadline = time.monotonic() + 1.0
    while True:
        descendants = _descendants(os.getpid())
        if not descendants:
            break
        found = True
        groups = {group for _, group in descendants.values()}
        groups.add(process_id)
        for group in groups:
            try:
                os.killpg(group, signal.SIGKILL)
            except (PermissionError, ProcessLookupError):
                pass
        for descendant in descendants:
            try:
                os.kill(descendant, signal.SIGKILL)
            except ProcessLookupError:
                pass
        while True:
            try:
                reaped, _ = os.waitpid(-1, os.WNOHANG)
            except ChildProcessError:
                break
            if reaped == 0:
                break
        if time.monotonic() >= deadline:
            break
        time.sleep(0.01)
    return found


__all__ = [
    "ProcessContainmentError",
    "establish_process_boundary",
    "terminate_descendants",
]
