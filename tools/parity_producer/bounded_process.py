"""Bounded subprocess capture with stable descendant containment."""

from __future__ import annotations

import ctypes
import os
import selectors
import signal
import subprocess
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Mapping, Sequence

from parity_producer.common import ProducerError


READ_CHUNK_BYTES = 64 * 1024
PR_SET_CHILD_SUBREAPER = 36
PR_GET_CHILD_SUBREAPER = 37
REAP_TIMEOUT_SECONDS = 5.0
CHILDREN_SNAPSHOT_BYTES = 4096
CHILDREN_PER_PASS = 64
_RUN_LOCK = threading.Lock()


@dataclass(frozen=True)
class BoundedResult:
    """A completed process whose captured streams stayed within their limits."""

    returncode: int
    stdout: bytes
    stderr: bytes


def run_bounded(
    command: Sequence[str],
    *,
    label: str,
    timeout_seconds: int,
    stdout_limit: int,
    stderr_limit: int,
    cwd: Path | str | None = None,
    environment: Mapping[str, str] | None = None,
    pass_fds: tuple[int, ...] = (),
) -> BoundedResult:
    """Run one command while bounding both streams and its process lifetime."""
    if timeout_seconds <= 0 or stdout_limit < 0 or stderr_limit < 0:
        raise ProducerError(f"{label} has invalid process safety limits")
    with _RUN_LOCK:
        return _run_bounded_locked(
            command,
            label=label,
            timeout_seconds=timeout_seconds,
            stdout_limit=stdout_limit,
            stderr_limit=stderr_limit,
            cwd=cwd,
            environment=environment,
            pass_fds=pass_fds,
        )


def _run_bounded_locked(
    command: Sequence[str],
    *,
    label: str,
    timeout_seconds: int,
    stdout_limit: int,
    stderr_limit: int,
    cwd: Path | str | None,
    environment: Mapping[str, str] | None,
    pass_fds: tuple[int, ...],
) -> BoundedResult:
    reserved_pidfd = _establish_process_boundary()
    try:
        try:
            process = subprocess.Popen(
                list(command),
                cwd=cwd,
                env=environment,
                pass_fds=pass_fds,
                start_new_session=True,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
        except OSError as error:
            raise ProducerError(f"{label} could not execute: {error}") from error
    finally:
        os.close(reserved_pidfd)

    try:
        process_pidfd = os.pidfd_open(process.pid)
    except OSError as error:
        try:
            _terminate_unbound_root(process)
        finally:
            _close_process_streams(process)
        raise ProducerError(
            f"{label} could not bind its child to a stable process identity: {error}"
        ) from error

    selector: selectors.BaseSelector | None = None
    streams = {
        "stdout": process.stdout,
        "stderr": process.stderr,
    }
    surviving_descendant = False
    try:
        try:
            selector = selectors.DefaultSelector()
        except OSError as error:
            raise ProducerError(
                f"{label} could not create its output monitor: {error}"
            ) from error
        limits = {
            "stdout": stdout_limit,
            "stderr": stderr_limit,
        }
        captured = {
            "stdout": bytearray(),
            "stderr": bytearray(),
        }
        deadline = time.monotonic() + timeout_seconds
        for name, stream in streams.items():
            if stream is None:
                raise ProducerError(f"{label} did not expose its {name} stream")
            try:
                selector.register(stream, selectors.EVENT_READ, name)
            except OSError as error:
                raise ProducerError(
                    f"{label} could not monitor its {name}: {error}"
                ) from error
        while selector.get_map():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise ProducerError(
                    f"{label} exceeded {timeout_seconds} seconds"
                )
            try:
                events = selector.select(remaining)
            except OSError as error:
                raise ProducerError(
                    f"{label} output monitoring failed: {error}"
                ) from error
            if not events:
                raise ProducerError(
                    f"{label} exceeded {timeout_seconds} seconds"
                )
            for key, _ in events:
                name = key.data
                try:
                    chunk = os.read(key.fd, READ_CHUNK_BYTES)
                except OSError as error:
                    raise ProducerError(
                        f"{label} could not read its {name}: {error}"
                    ) from error
                if not chunk:
                    try:
                        selector.unregister(key.fileobj)
                        key.fileobj.close()
                    except OSError as error:
                        raise ProducerError(
                            f"{label} could not close its {name}: {error}"
                        ) from error
                    streams[name] = None
                    continue
                if len(captured[name]) + len(chunk) > limits[name]:
                    raise ProducerError(
                        f"{label} {name} exceeds the "
                        f"{limits[name]}-byte safety limit"
                    )
                captured[name].extend(chunk)
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise ProducerError(f"{label} exceeded {timeout_seconds} seconds")
        try:
            returncode = process.wait(timeout=remaining)
        except subprocess.TimeoutExpired as error:
            raise ProducerError(
                f"{label} exceeded {timeout_seconds} seconds"
            ) from error
        except OSError as error:
            raise ProducerError(f"{label} could not be reaped: {error}") from error
        result = BoundedResult(
            returncode=returncode,
            stdout=bytes(captured["stdout"]),
            stderr=bytes(captured["stderr"]),
        )
    finally:
        try:
            surviving_descendant = _terminate_process_tree(
                process,
                process_pidfd,
            )
        finally:
            os.close(process_pidfd)
            if selector is not None:
                selector.close()
            for stream in streams.values():
                if stream is not None:
                    try:
                        stream.close()
                    except OSError:
                        pass
    if surviving_descendant:
        raise ProducerError(
            f"{label} left a surviving descendant process; "
            "the command result was rejected"
        )
    return result


def _terminate_process_tree(
    process: subprocess.Popen[bytes],
    process_pidfd: int,
) -> bool:
    """Kill the stable root identity, then kill and reap every adopted child."""
    deadline = time.monotonic() + REAP_TIMEOUT_SECONDS
    if process.returncode is None:
        try:
            signal.pidfd_send_signal(process_pidfd, signal.SIGKILL)
        except ProcessLookupError:
            pass
        except OSError as error:
            raise ProducerError(
                f"could not terminate bounded child: {error}"
            ) from error
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise ProducerError(
                "bounded child process could not be reaped after SIGKILL"
            )
        try:
            process.wait(timeout=remaining)
        except subprocess.TimeoutExpired as error:
            raise ProducerError(
                "bounded child process could not be reaped after SIGKILL"
            ) from error
        except OSError as error:
            raise ProducerError(
                f"bounded child process could not be reaped: {error}"
            ) from error
    return _terminate_adopted_descendants(deadline)


def _terminate_unbound_root(process: subprocess.Popen[bytes]) -> None:
    """Clean up a direct unreaped child when its pidfd could not be opened."""
    deadline = time.monotonic() + REAP_TIMEOUT_SECONDS
    # A proven direct child's numeric PID cannot be reused until it is reaped.
    # This path is used only to fail closed after pidfd acquisition fails.
    try:
        os.kill(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    except OSError as error:
        raise ProducerError(
            f"could not terminate unbound bounded child: {error}"
        ) from error
    try:
        process.wait(timeout=max(0.0, deadline - time.monotonic()))
    except subprocess.TimeoutExpired as error:
        raise ProducerError(
            "bounded child process could not be reaped after SIGKILL"
        ) from error
    _terminate_adopted_descendants(deadline)


def _close_process_streams(process: subprocess.Popen[bytes]) -> None:
    for stream in (process.stdout, process.stderr):
        if stream is not None:
            try:
                stream.close()
            except OSError:
                pass


def _establish_process_boundary() -> int:
    """Prove exclusive child ownership and reserve one working pidfd."""
    _ensure_subreaper()
    _require_single_thread()
    if signal.getsignal(signal.SIGCHLD) != signal.SIG_DFL:
        raise ProducerError(
            "could not establish bounded descendant containment: "
            "SIGCHLD disposition is not default"
        )
    if _direct_children():
        raise ProducerError(
            "could not establish bounded descendant containment: "
            "process boundary already has children"
        )
    descriptor: int | None = None
    try:
        descriptor = os.pidfd_open(os.getpid())
        signal.pidfd_send_signal(descriptor, 0)
    except (AttributeError, OSError) as error:
        if descriptor is not None:
            os.close(descriptor)
        raise ProducerError(
            f"could not establish stable process identity containment: {error}"
        ) from error
    return descriptor


def _ensure_subreaper() -> None:
    """Adopt grandchildren and verify the kernel retained that state."""
    try:
        libc = ctypes.CDLL(None, use_errno=True)
        result = libc.prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0)
    except (AttributeError, OSError) as error:
        raise ProducerError(
            f"could not establish bounded descendant containment: {error}"
        ) from error
    if result != 0:
        raise ProducerError(
            "could not establish bounded descendant containment: "
            f"errno {ctypes.get_errno()}"
        )
    state = ctypes.c_int()
    try:
        result = libc.prctl(
            PR_GET_CHILD_SUBREAPER,
            ctypes.byref(state),
            0,
            0,
            0,
        )
    except (AttributeError, OSError, TypeError) as error:
        raise ProducerError(
            f"could not verify bounded descendant containment: {error}"
        ) from error
    if result != 0 or state.value != 1:
        raise ProducerError("could not verify bounded descendant containment")


def _require_single_thread() -> None:
    task_directory = f"/proc/{os.getpid()}/task"
    try:
        entries = os.scandir(task_directory)
    except OSError as error:
        raise ProducerError(
            f"could not inspect bounded process threads: {error}"
        ) from error
    count = 0
    with entries:
        for entry in entries:
            if entry.name.isdecimal():
                count += 1
                if count > 1:
                    break
    if count != 1:
        raise ProducerError(
            "could not establish bounded descendant containment: "
            "producer process is not single-threaded"
        )


def _direct_children() -> tuple[int, ...]:
    path = f"/proc/{os.getpid()}/task/{os.getpid()}/children"
    flags = os.O_RDONLY | os.O_CLOEXEC
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise ProducerError(
            f"could not inspect adopted bounded descendants: {error}"
        ) from error
    try:
        raw = bytearray()
        while len(raw) <= CHILDREN_SNAPSHOT_BYTES:
            try:
                chunk = os.read(
                    descriptor,
                    CHILDREN_SNAPSHOT_BYTES + 1 - len(raw),
                )
            except OSError as error:
                raise ProducerError(
                    f"could not read adopted bounded descendants: {error}"
                ) from error
            if not chunk:
                break
            raw.extend(chunk)
    finally:
        os.close(descriptor)
    if not raw:
        return ()
    if len(raw) > CHILDREN_SNAPSHOT_BYTES:
        boundary = raw.rfind(b" ", 0, CHILDREN_SNAPSHOT_BYTES)
        if boundary < 0:
            raise ProducerError(
                "adopted bounded descendant identity exceeded its fixed bound"
            )
        del raw[boundary + 1 :]
    tokens = raw.split()
    children: list[int] = []
    for token in tokens[:CHILDREN_PER_PASS]:
        if not token.isdigit():
            raise ProducerError(
                "adopted bounded descendant identity is malformed"
            )
        child = int(token)
        if child <= 0 or child == os.getpid():
            raise ProducerError(
                "adopted bounded descendant identity is invalid"
            )
        children.append(child)
    return tuple(children)


def _terminate_adopted_descendants(deadline: float) -> bool:
    found = False
    while True:
        children = _direct_children()
        if not children:
            return found
        found = True
        for child in children:
            if time.monotonic() >= deadline:
                raise ProducerError(
                    "bounded descendants exceeded their cleanup deadline"
                )
            _terminate_adopted_child(child, deadline)


def _terminate_adopted_child(child: int, deadline: float) -> None:
    try:
        descriptor = os.pidfd_open(child)
    except OSError as error:
        raise ProducerError(
            f"could not bind adopted bounded descendant identity: {error}"
        ) from error
    try:
        try:
            signal.pidfd_send_signal(descriptor, signal.SIGKILL)
        except ProcessLookupError:
            pass
        except OSError as error:
            raise ProducerError(
                f"could not terminate adopted bounded descendant: {error}"
            ) from error
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise ProducerError(
                    "bounded descendant could not be reaped after SIGKILL"
                )
            try:
                result = os.waitid(
                    os.P_PIDFD,
                    descriptor,
                    os.WEXITED | os.WNOHANG,
                )
            except ChildProcessError as error:
                raise ProducerError(
                    "adopted bounded descendant ownership was lost"
                ) from error
            except OSError as error:
                raise ProducerError(
                    f"could not reap adopted bounded descendant: {error}"
                ) from error
            if result is not None:
                return
            time.sleep(min(0.01, remaining))
    finally:
        os.close(descriptor)
