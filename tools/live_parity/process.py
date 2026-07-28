"""Bounded subprocess execution with process-group teardown."""

from __future__ import annotations

import ctypes
import os
import re
import selectors
import shlex
import signal
import subprocess
import threading
import time
from dataclasses import dataclass
from pathlib import Path

from .errors import GateError


ERROR_BYTES = 16 * 1024
READ_BYTES = 64 * 1024
TERMINATION_GRACE_SECONDS = 1.0
PR_SET_CHILD_SUBREAPER = 36
_RUN_LOCK = threading.Lock()


@dataclass(frozen=True)
class CommandResult:
    """Bounded captured output from one command."""

    stdout: bytes
    stderr: bytes


def inherited_environment() -> dict[str, str]:
    """Return the producer environment without Python bytecode writes."""

    environment = os.environ.copy()
    environment["PYTHONDONTWRITEBYTECODE"] = "1"
    environment["GH_PROMPT_DISABLED"] = "1"
    return environment


def _command_text(command: list[str]) -> str:
    return shlex.join(command)


def _process_table() -> dict[int, tuple[int, int, int]]:
    """Return PID to parent/group/session identities from Linux procfs."""

    result: dict[int, tuple[int, int, int]] = {}
    try:
        entries = os.scandir("/proc")
    except OSError as error:
        raise GateError(f"cannot inspect descendant processes: {error}") from error
    with entries:
        for entry in entries:
            if not entry.name.isdecimal():
                continue
            try:
                raw = Path(entry.path, "stat").read_bytes()
                fields = raw[raw.rfind(b")") + 2 :].split()
                parent = int(fields[1])
                process_group = int(fields[2])
                observed_session = int(fields[3])
            except (OSError, ValueError, IndexError):
                continue
            result[int(entry.name)] = (parent, process_group, observed_session)
    return result


def _descendants(parent: int) -> dict[int, tuple[int, int, int]]:
    table = _process_table()
    known = {parent}
    result: dict[int, tuple[int, int, int]] = {}
    changed = True
    while changed:
        changed = False
        for process_id, identity in table.items():
            if process_id not in known and identity[0] in known:
                known.add(process_id)
                result[process_id] = identity
                changed = True
    return result


def _ensure_subreaper() -> None:
    try:
        libc = ctypes.CDLL(None, use_errno=True)
        result = libc.prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0)
    except (AttributeError, OSError) as error:
        raise GateError(f"cannot establish descendant containment: {error}") from error
    if result != 0:
        error_number = ctypes.get_errno()
        raise GateError(
            f"cannot establish descendant containment: errno {error_number}"
        )


def _signal_descendants(
    process: subprocess.Popen[bytes],
    requested_signal: signal.Signals,
) -> None:
    descendants = _descendants(os.getpid())
    groups = {identity[1] for identity in descendants.values()}
    groups.add(process.pid)
    for process_group in groups:
        try:
            os.killpg(process_group, requested_signal)
        except ProcessLookupError:
            pass
        except PermissionError:
            pass
    for process_id in descendants:
        try:
            os.kill(process_id, requested_signal)
        except ProcessLookupError:
            pass


def _terminate_group(process: subprocess.Popen[bytes]) -> None:
    """Terminate every process group in the command session."""

    _signal_descendants(process, signal.SIGTERM)
    try:
        process.wait(timeout=TERMINATION_GRACE_SECONDS)
    except subprocess.TimeoutExpired:
        pass
    deadline = time.monotonic() + TERMINATION_GRACE_SECONDS
    while _descendants(os.getpid()) and time.monotonic() < deadline:
        _signal_descendants(process, signal.SIGTERM)
        time.sleep(0.01)
    _signal_descendants(process, signal.SIGKILL)
    try:
        process.wait(timeout=TERMINATION_GRACE_SECONDS)
    except subprocess.TimeoutExpired:
        pass
    while True:
        try:
            reaped, _ = os.waitpid(-1, os.WNOHANG)
        except ChildProcessError:
            break
        if reaped == 0:
            break


def _reject_surviving_descendants(process: subprocess.Popen[bytes]) -> None:
    """Fail a completed command that left any process in its session."""

    if not _descendants(os.getpid()):
        return
    _terminate_group(process)
    raise GateError("command left a surviving descendant process")


def _redacted_detail(raw: bytes) -> str:
    detail = " ".join(raw[:4096].decode("utf-8", errors="replace").split())
    return re.sub(
        r"(?i)(token|secret|password|credential)=[^\s]+",
        r"\1=[redacted]",
        detail,
    )


def _bounded_capture(
    process: subprocess.Popen[bytes],
    *,
    timeout: float,
    stdout_limit: int,
) -> CommandResult:
    if process.stdout is None or process.stderr is None:
        raise GateError("internal live parity capture boundary is incomplete")
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ, "stdout")
    selector.register(process.stderr, selectors.EVENT_READ, "stderr")
    buffers = {"stdout": bytearray(), "stderr": bytearray()}
    deadline = time.monotonic() + timeout
    try:
        while selector.get_map():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                _terminate_group(process)
                raise GateError("command timed out and its process group was terminated")
            events = selector.select(remaining)
            if not events:
                _terminate_group(process)
                raise GateError("command timed out and its process group was terminated")
            for key, _ in events:
                chunk = os.read(key.fd, READ_BYTES)
                if not chunk:
                    selector.unregister(key.fileobj)
                    continue
                name = key.data
                buffer = buffers[name]
                limit = stdout_limit if name == "stdout" else ERROR_BYTES
                buffer.extend(chunk[: max(0, limit + 1 - len(buffer))])
                if len(buffer) > limit:
                    _terminate_group(process)
                    raise GateError(f"command {name} exceeds its safety limit")
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            _terminate_group(process)
            raise GateError("command timed out and its process group was terminated")
        try:
            process.wait(timeout=remaining)
        except subprocess.TimeoutExpired as error:
            _terminate_group(process)
            raise GateError(
                "command timed out and its process group was terminated"
            ) from error
    finally:
        selector.close()
        process.stdout.close()
        process.stderr.close()
    return CommandResult(bytes(buffers["stdout"]), bytes(buffers["stderr"]))


def _run_command_locked(
    command: list[str],
    *,
    cwd: Path,
    timeout: float,
    environment: dict[str, str] | None = None,
    capture_stdout: bool = False,
    stdout_limit: int = 0,
    pass_fds: tuple[int, ...] = (),
) -> CommandResult:
    if not command or any(not isinstance(value, str) or not value for value in command):
        raise GateError("live parity attempted to execute an invalid command")
    if len(set(pass_fds)) != len(pass_fds) or any(value < 0 for value in pass_fds):
        raise GateError("live parity attempted to pass an invalid descriptor")
    _ensure_subreaper()
    if _descendants(os.getpid()):
        raise GateError("live parity process boundary already has descendants")
    capture = capture_stdout
    try:
        process = subprocess.Popen(
            command,
            cwd=cwd,
            env=environment if environment is not None else inherited_environment(),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE if capture else None,
            stderr=subprocess.PIPE if capture else None,
            pass_fds=pass_fds,
            start_new_session=True,
        )
    except OSError as error:
        raise GateError(f"could not execute {_command_text(command)}: {error}") from error
    if capture:
        try:
            result = _bounded_capture(
                process,
                timeout=timeout,
                stdout_limit=stdout_limit,
            )
        except GateError as error:
            raise GateError(f"{_command_text(command)}: {error}") from error
    else:
        try:
            return_code = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired as error:
            _terminate_group(process)
            raise GateError(
                f"{_command_text(command)} timed out; its process group was terminated"
            ) from error
        result = CommandResult(b"", b"")
        _reject_surviving_descendants(process)
        if return_code != 0:
            raise GateError(
                f"{_command_text(command)} exited {return_code}"
            )
        return result
    _reject_surviving_descendants(process)
    if process.returncode != 0:
        detail = _redacted_detail(result.stderr or result.stdout)
        suffix = f": {detail}" if detail else ""
        raise GateError(
            f"{_command_text(command)} exited {process.returncode}{suffix}"
        )
    return result


def run_command(
    command: list[str],
    *,
    cwd: Path,
    timeout: float,
    environment: dict[str, str] | None = None,
    capture_stdout: bool = False,
    stdout_limit: int = 0,
    pass_fds: tuple[int, ...] = (),
) -> CommandResult:
    """Run one command with exclusive, inescapable descendant containment."""

    with _RUN_LOCK:
        return _run_command_locked(
            command,
            cwd=cwd,
            timeout=timeout,
            environment=environment,
            capture_stdout=capture_stdout,
            stdout_limit=stdout_limit,
            pass_fds=pass_fds,
        )
