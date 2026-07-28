"""Public release-check command-boundary canaries."""

from __future__ import annotations

import os
import signal
import stat
import subprocess
import time
from pathlib import Path

from .model import GateError


RELEASE_CHECK = Path(__file__).resolve().parents[1] / "release-check"
PYTHON_SUCCESS = "#!/usr/bin/python3 -I\nraise SystemExit(0)\n"
TOOLCHAIN_COMMANDS = (
    "cargo",
    "cargo-clippy",
    "cargo-deny",
    "cargo-fmt",
    "rustc",
    "rustdoc",
    "rustfmt",
)
SUCCESS_AUTHORITY_TOOLS = (
    "tools/tests/check-release-provenance-temp",
    "tools/check-stubs",
    "tools/check-stabilization-ledger",
    "tools/check-test-authority",
    "tools/tests/check-capability-test-manifest",
    "tools/tests/check-criterion-manifest",
    "tools/release_check_credential_bundle.py",
    "tools/check-live-parity",
    "tools/release-provenance",
)


def _write(root: Path, relative: str, text: str) -> Path:
    path = root / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")
    return path


def _executable(root: Path, relative: str, text: str) -> Path:
    path = _write(root, relative, text)
    path.chmod(0o755)
    return path


def _checked(
    command: list[str],
    root: Path,
    label: str,
    environment: dict[str, str],
) -> str:
    try:
        result = subprocess.run(
            command,
            cwd=root,
            check=False,
            capture_output=True,
            env=environment,
            text=True,
        )
    except OSError as error:
        raise GateError(f"could not execute {label}: {error}") from error
    if result.returncode != 0:
        raise GateError(
            f"{label} failed with status {result.returncode}\n"
            f"{result.stdout}{result.stderr}"
        )
    return result.stdout.strip()


def _git_environment(home: Path, gnupg: Path) -> dict[str, str]:
    return {
        "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_SYSTEM": "/dev/null",
        "GIT_TERMINAL_PROMPT": "0",
        "GNUPGHOME": str(gnupg),
        "HOME": str(home),
        "LANG": "C",
        "LC_ALL": "C",
        "PATH": "/usr/bin:/bin",
        "XDG_CONFIG_HOME": str(home / ".config"),
    }


def _marker_writer_source() -> str:
    return """
def write_marker(path, payload):
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(path, flags, 0o600)
    try:
        os.write(descriptor, payload.encode("utf-8"))
        os.fsync(descriptor)
    finally:
        os.close(descriptor)
"""


def _comparator_source() -> str:
    return (
        "#!/usr/bin/python3 -I\n"
        "import os\n"
        "import sys\n"
        "from pathlib import Path\n"
        f"{_marker_writer_source()}\n"
        'state = Path(os.environ["AUTHORITY_CANARY_STATE"])\n'
        'nonce = os.environ["AUTHORITY_CANARY_NONCE"]\n'
        'logical_command = tuple(Path(sys.argv[0]).parts[-2:])\n'
        'if logical_command != ("tools", "compare-parity") '
        'or sys.argv[1:] != ["--self-test"]:\n'
        '    write_marker(state / "wrong-comparator-argv", repr(sys.argv[1:]))\n'
        "    raise SystemExit(64)\n"
        'write_marker(state / "comparator", f"compare|--self-test|{nonce}\\n")\n'
        "raise SystemExit(19)\n"
    )


def _producer_source() -> str:
    delayed_child = (
        "import os,sys,time;"
        "time.sleep(0.2);"
        "fd=os.open(sys.argv[1],os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600);"
        "os.write(fd,b'delayed producer\\n');"
        "os.close(fd);"
        "time.sleep(10)"
    )
    return (
        "#!/usr/bin/python3 -I\n"
        "import os\n"
        "import subprocess\n"
        "import sys\n"
        "from pathlib import Path\n"
        'state = Path(os.environ["AUTHORITY_CANARY_STATE"])\n'
        "if sys.argv[1:]:\n"
        "    raise SystemExit(65)\n"
        '(state / "producer").symlink_to("missing-producer-target")\n'
        "subprocess.Popen(\n"
        "    [\n"
        '        "/usr/bin/python3",\n'
        '        "-I",\n'
        '        "-B",\n'
        '        "-c",\n'
        f"        {delayed_child!r},\n"
        '        str(state / "producer-delayed"),\n'
        "    ],\n"
        "    stdin=subprocess.DEVNULL,\n"
        "    stdout=subprocess.DEVNULL,\n"
        "    stderr=subprocess.DEVNULL,\n"
        "    close_fds=True,\n"
        ")\n"
    )


def _portable_manifest_source() -> str:
    return (
        "#!/usr/bin/python3 -I\n"
        "import os\n"
        "import sys\n"
        "from pathlib import Path\n"
        f"{_marker_writer_source()}\n"
        'if sys.argv[1:] == ["--run"]:\n'
        '    state = Path(os.environ["AUTHORITY_CANARY_STATE"])\n'
        '    write_marker(state / "after-producer", "after|portable-run\\n")\n'
        "    raise SystemExit(29)\n"
        "raise SystemExit(0)\n"
    )


def _swap_gate_source() -> str:
    return (
        "#!/usr/bin/python3 -I\n"
        "import os\n"
        "from pathlib import Path\n"
        f"{_marker_writer_source()}\n"
        'repository = Path(os.environ["AUTHORITY_CANARY_REPOSITORY"])\n'
        'attack = Path(os.environ["AUTHORITY_CANARY_ATTACK"])\n'
        'os.replace(attack / "compare-parity", repository / "tools/compare-parity")\n'
        "os.replace(\n"
        '    attack / "check-parity-producer",\n'
        '    repository / "tools/tests/check-parity-producer",\n'
        ")\n"
        'write_marker(\n'
        '    Path(os.environ["AUTHORITY_CANARY_STATE"]) / "swap-complete",\n'
        '    os.environ["AUTHORITY_CANARY_NONCE"] + "\\n",\n'
        ")\n"
    )


def _laundering_comparator_source() -> str:
    return (
        "#!/usr/bin/python3 -I\n"
        "import os\n"
        "import sys\n"
        "from pathlib import Path\n"
        'state = Path(os.environ["AUTHORITY_CANARY_STATE"])\n'
        'attack = Path(os.environ["AUTHORITY_CANARY_ATTACK"])\n'
        'target = attack / "laundered-comparator-target"\n'
        'target.write_text(\n'
        '    f"compare|--self-test|{os.environ[\'AUTHORITY_CANARY_NONCE\']}\\n",\n'
        '    encoding="utf-8",\n'
        ")\n"
        '(state / "comparator").symlink_to(target)\n'
        "raise SystemExit(19 if sys.argv[1:] == ['--self-test'] else 64)\n"
    )


def _hostile_interpreter_source(marker_name: str, status_code: int) -> str:
    return (
        "#!/usr/bin/python3 -I\n"
        "import os\n"
        "from pathlib import Path\n"
        f'Path(os.environ["AUTHORITY_CANARY_STATE"], {marker_name!r}).touch()\n'
        f"raise SystemExit({status_code})\n"
    )


def _create_case(
    temporary: Path,
    name: str,
    *,
    mutate_checkout: bool,
) -> tuple[Path, Path, Path, dict[str, str], str]:
    case_root = temporary / name
    repository = case_root / "repository"
    state = case_root / "state"
    command_path = case_root / "commands"
    runtime = case_root / "runtime"
    home = case_root / "home"
    hooks = case_root / "hooks"
    gnupg = case_root / "gnupg"
    attack = case_root / "attack"
    for directory in (
        repository,
        state,
        command_path,
        runtime,
        home,
        hooks,
        gnupg,
        attack,
    ):
        directory.mkdir(parents=True)
    gnupg.chmod(0o700)
    nonce = os.urandom(24).hex()

    _write(
        repository,
        "Cargo.lock",
        "# This file is automatically @generated by Cargo.\n"
        "# It is not intended for manual editing.\n"
        "version = 4\n",
    )
    try:
        release_source = RELEASE_CHECK.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise GateError(f"could not read release-check for order canary: {error}") from error
    _executable(repository, "tools/release-check", release_source)

    first_gate = _swap_gate_source() if mutate_checkout else PYTHON_SUCCESS
    _executable(repository, "tools/tests/check-release-transfer-bundles", first_gate)
    for relative in SUCCESS_AUTHORITY_TOOLS:
        _executable(repository, relative, PYTHON_SUCCESS)
    _executable(
        repository,
        "tools/tests/check-portable-test-manifest",
        _portable_manifest_source(),
    )
    _executable(repository, "tools/compare-parity", _comparator_source())
    _executable(
        repository,
        "tools/tests/check-parity-producer",
        _producer_source(),
    )

    for command_name in TOOLCHAIN_COMMANDS:
        _executable(command_path, command_name, PYTHON_SUCCESS)
    _executable(
        command_path,
        "bash",
        _hostile_interpreter_source("path-bash-loaded", 71),
    )
    _executable(
        command_path,
        "python3",
        _hostile_interpreter_source("path-python-loaded", 73),
    )
    _write(
        hooks,
        "bash-env",
        '/usr/bin/printf "loaded\\n" >'
        ' "${AUTHORITY_CANARY_STATE:?}/bash-env-loaded"\n'
        "exit 72\n",
    )

    _executable(attack, "compare-parity", _laundering_comparator_source())
    _executable(attack, "check-parity-producer", _producer_source())

    git_environment = _git_environment(home, gnupg)
    for command, label in (
        (
            [
                "/usr/bin/git",
                "-c",
                "init.defaultObjectFormat=sha1",
                "-c",
                "init.defaultBranch=main",
                "init",
                "-q",
            ],
            "release canary git init",
        ),
        (
            ["/usr/bin/git", "config", "user.name", "Greenlit authority"],
            "release canary git name",
        ),
        (
            ["/usr/bin/git", "config", "user.email", "authority@example.invalid"],
            "release canary git email",
        ),
        (
            ["/usr/bin/git", "config", "commit.gpgSign", "false"],
            "release canary git commit signing",
        ),
        (
            ["/usr/bin/git", "config", "tag.gpgSign", "false"],
            "release canary git tag signing",
        ),
        (
            ["/usr/bin/git", "config", "core.hooksPath", str(hooks)],
            "release canary git hooks",
        ),
        (["/usr/bin/git", "add", "--all"], "release canary git add"),
        (
            ["/usr/bin/git", "commit", "-q", "-m", "authority canary"],
            "release canary git commit",
        ),
    ):
        _checked(command, repository, label, git_environment)
    source_commit = _checked(
        ["/usr/bin/git", "rev-parse", "HEAD"],
        repository,
        "release canary git identity",
        git_environment,
    )
    environment = {
        **git_environment,
        "AUTHORITY_CANARY_ATTACK": str(attack),
        "AUTHORITY_CANARY_NONCE": nonce,
        "AUTHORITY_CANARY_REPOSITORY": str(repository),
        "AUTHORITY_CANARY_STATE": str(state),
        "BASH_ENV": str(hooks / "bash-env"),
        "ENV": str(hooks / "bash-env"),
        "GIT_DIR": str(attack / "ambient-git-directory"),
        "GIT_WORK_TREE": str(attack / "ambient-git-work-tree"),
        "GREENLIT_BUILD_COMMIT": source_commit,
        "PATH": f"{command_path}{os.pathsep}/usr/bin{os.pathsep}/bin",
        "PYTHONHOME": str(attack / "ambient-python-home"),
        "PYTHONPATH": str(attack / "ambient-python-path"),
        "RUNNER_TEMP": str(runtime),
        "TMPDIR": str(runtime),
    }
    return repository, state, runtime, environment, nonce


def _process_group_exists(process_group: int) -> bool:
    try:
        os.killpg(process_group, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def _quiesce_process_group(process_group: int) -> None:
    if not _process_group_exists(process_group):
        return
    try:
        os.killpg(process_group, signal.SIGKILL)
    except ProcessLookupError:
        return
    deadline = time.monotonic() + 2.0
    while _process_group_exists(process_group) and time.monotonic() < deadline:
        time.sleep(0.02)
    if _process_group_exists(process_group):
        raise GateError("could not quiesce release canary process group")


def _run_release(
    repository: Path,
    runtime: Path,
    environment: dict[str, str],
) -> tuple[int, str, str, bool]:
    stdout_path = runtime / "release.stdout"
    stderr_path = runtime / "release.stderr"
    try:
        with stdout_path.open("x", encoding="utf-8") as stdout_file:
            with stderr_path.open("x", encoding="utf-8") as stderr_file:
                process = subprocess.Popen(
                    [str(repository / "tools/release-check"), "prepare"],
                    cwd=repository,
                    env=environment,
                    stdin=subprocess.DEVNULL,
                    stdout=stdout_file,
                    stderr=stderr_file,
                    start_new_session=True,
                    text=True,
                )
                try:
                    status_code = process.wait(timeout=30)
                except subprocess.TimeoutExpired as error:
                    _quiesce_process_group(process.pid)
                    process.wait()
                    raise GateError("release-check order canary timed out") from error
    except OSError as error:
        raise GateError(f"could not execute release-check order canary: {error}") from error

    surviving_process = _process_group_exists(process.pid)
    if surviving_process:
        _quiesce_process_group(process.pid)
    try:
        stdout = stdout_path.read_text(encoding="utf-8")
        stderr = stderr_path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise GateError(f"could not read release-check canary output: {error}") from error
    return status_code, stdout, stderr, surviving_process


def _read_regular_marker(path: Path, expected: str) -> None:
    try:
        before = os.stat(path, follow_symlinks=False)
    except OSError as error:
        raise GateError(f"required release canary marker is unavailable: {path.name}") from error
    if (
        not stat.S_ISREG(before.st_mode)
        or stat.S_IMODE(before.st_mode) != 0o600
        or before.st_nlink != 1
    ):
        raise GateError(f"release canary marker was laundered: {path.name}")
    flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise GateError(f"could not bind release canary marker: {path.name}") from error
    try:
        after = os.fstat(descriptor)
        chunks = []
        while True:
            chunk = os.read(descriptor, 4096)
            if not chunk:
                break
            chunks.append(chunk)
    finally:
        os.close(descriptor)
    if (
        (before.st_dev, before.st_ino) != (after.st_dev, after.st_ino)
        or not stat.S_ISREG(after.st_mode)
        or stat.S_IMODE(after.st_mode) != 0o600
        or after.st_nlink != 1
    ):
        raise GateError(f"release canary marker identity changed: {path.name}")
    try:
        value = b"".join(chunks).decode("utf-8")
    except UnicodeError as error:
        raise GateError(f"release canary marker is not UTF-8: {path.name}") from error
    if value != expected:
        raise GateError(f"release canary marker has the wrong identity: {path.name}")


def _assert_absent(path: Path) -> None:
    try:
        os.stat(path, follow_symlinks=False)
    except FileNotFoundError:
        return
    except OSError as error:
        raise GateError(f"could not inspect release canary marker: {path.name}") from error
    raise GateError(f"unexpected release canary marker exists: {path.name}")


def _result_details(status_code: int, stdout: str, stderr: str) -> str:
    return f"status={status_code}\n{stdout}{stderr}"


def release_gate_order_canary(temporary: Path) -> None:
    """Exercise release order, interpreter, and immutable-authority boundaries."""

    repository, state, runtime, environment, nonce = _create_case(
        temporary,
        "release-order",
        mutate_checkout=False,
    )
    status_code, stdout, stderr, surviving_process = _run_release(
        repository,
        runtime,
        environment,
    )
    if surviving_process:
        raise GateError(
            "release-check launched a delayed/background process before "
            "comparator failure\n"
            f"{_result_details(status_code, stdout, stderr)}"
        )
    if status_code != 19:
        raise GateError(
            "release-check did not preserve exact comparator failure\n"
            f"{_result_details(status_code, stdout, stderr)}"
        )
    _read_regular_marker(
        state / "comparator",
        f"compare|--self-test|{nonce}\n",
    )
    for marker_name in (
        "after-producer",
        "bash-env-loaded",
        "path-bash-loaded",
        "path-python-loaded",
        "producer",
        "producer-delayed",
        "wrong-comparator-argv",
    ):
        _assert_absent(state / marker_name)

    repository, state, runtime, environment, nonce = _create_case(
        temporary,
        "release-swap",
        mutate_checkout=True,
    )
    status_code, stdout, stderr, surviving_process = _run_release(
        repository,
        runtime,
        environment,
    )
    if surviving_process:
        raise GateError(
            "release-check left a process alive after rejecting a source swap\n"
            f"{_result_details(status_code, stdout, stderr)}"
        )
    if (
        status_code != 1
        or "release source changed after authority binding" not in stderr
    ):
        raise GateError(
            "release-check did not fail closed after an authority path swap\n"
            f"{_result_details(status_code, stdout, stderr)}"
        )
    _read_regular_marker(state / "swap-complete", f"{nonce}\n")
    for marker_name in (
        "after-producer",
        "bash-env-loaded",
        "comparator",
        "path-bash-loaded",
        "path-python-loaded",
        "producer",
        "producer-delayed",
        "wrong-comparator-argv",
    ):
        _assert_absent(state / marker_name)
