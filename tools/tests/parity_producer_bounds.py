"""Public-command canaries for parity producer process-output bounds."""

from __future__ import annotations

import json
import os
import stat
import subprocess
import sys
import time
from pathlib import Path


TOKEN_DESCRIPTOR = "GREENLIT_GITHUB_PRODUCER_CREDENTIAL_FD"


def run_bounds_canaries(
    *,
    tool: Path,
    test_root: Path,
    checkout: Path,
    output_root: Path,
    trusted: list[str],
    source_commit: str,
    github_inputs: dict[str, Path],
    run_id: int,
) -> None:
    """Require bounded rejection and descendant cleanup for both producers."""
    protected = (
        output_root / "seed-github-actions.json",
        output_root / "captures/shell-only-seed-github-actions.json",
    )
    before = {path: path.read_bytes() for path in protected}
    _local_success_escape(
        tool=tool,
        test_root=test_root,
        output_root=output_root,
        trusted=trusted,
        source_commit=source_commit,
    )
    _local_overflow(
        tool=tool,
        test_root=test_root,
        output_root=output_root,
        trusted=trusted,
        source_commit=source_commit,
    )
    _github_log_overflow(
        tool=tool,
        test_root=test_root,
        output_root=output_root,
        trusted=trusted,
        github_inputs=github_inputs,
        run_id=run_id,
    )
    if any(path.read_bytes() != before[path] for path in protected):
        raise RuntimeError("failed bounded producer changed prior parity evidence")
    if checkout.joinpath(".litci").exists():
        raise RuntimeError("bounded producer canary wrote into the source checkout")


def _local_success_escape(
    *,
    tool: Path,
    test_root: Path,
    output_root: Path,
    trusted: list[str],
    source_commit: str,
) -> None:
    target = test_root / "escaped-success-target/release"
    target.mkdir(parents=True)
    binary = target / "litci"
    home = test_root / "escaped-success-home"
    home.mkdir(mode=0o700)
    sentinel = test_root / "successful-descendant-survived"
    child_identity = test_root / "successful-descendant.identity"
    _write_success_escape_executable(
        binary,
        version_commit=source_commit,
        sentinel=sentinel,
        child_identity=child_identity,
    )
    result = _run(
        [
            sys.executable,
            "-B",
            str(tool),
            "greenlit-release",
            *trusted,
            "--output-root",
            str(output_root),
            "--binary",
            str(binary),
            "--home",
            str(home),
        ],
        success=False,
    )
    if "left a surviving descendant process" not in result.stderr:
        raise RuntimeError(
            "release producer accepted a successful command with an "
            "escaped-session descendant: " + result.stderr
        )
    _require_descendant_gone(
        sentinel,
        child_identity,
        "successful release producer command",
    )


def _local_overflow(
    *,
    tool: Path,
    test_root: Path,
    output_root: Path,
    trusted: list[str],
    source_commit: str,
) -> None:
    target = test_root / "hostile-target/release"
    target.mkdir(parents=True)
    binary = target / "litci"
    home = test_root / "hostile-home"
    home.mkdir(mode=0o700)
    sentinel = test_root / "local-descendant-survived"
    child_identity = test_root / "local-descendant.identity"
    _write_overflow_executable(
        binary,
        version_commit=source_commit,
        sentinel=sentinel,
        child_identity=child_identity,
    )
    result = _run(
        [
            sys.executable,
            "-B",
            str(tool),
            "greenlit-release",
            *trusted,
            "--output-root",
            str(output_root),
            "--binary",
            str(binary),
            "--home",
            str(home),
        ],
        success=False,
    )
    if "stdout exceeds the 8388608-byte safety limit" not in result.stderr:
        raise RuntimeError(
            "release producer did not reject bounded stdout: " + result.stderr
        )
    _require_descendant_gone(
        sentinel,
        child_identity,
        "failed release producer command",
    )


def _github_log_overflow(
    *,
    tool: Path,
    test_root: Path,
    output_root: Path,
    trusted: list[str],
    github_inputs: dict[str, Path],
    run_id: int,
) -> None:
    executable = test_root / "hostile-gh"
    record = test_root / "hostile-gh-calls.ndjson"
    sentinel = test_root / "github-descendant-survived"
    child_identity = test_root / "github-descendant.identity"
    endpoints = {
        "run": f"repos/KanterLabs/greenlit-app/actions/runs/{run_id}",
        "jobs": (
            f"repos/KanterLabs/greenlit-app/actions/runs/{run_id}"
            "/attempts/1/jobs?per_page=100"
        ),
        "content": (
            "repos/KanterLabs/greenlit-app/contents/"
            ".github/workflows/parity-seed.yml"
        ),
        "logs": (
            f"repos/KanterLabs/greenlit-app/actions/runs/{run_id}"
            "/attempts/1/logs"
        ),
    }
    _write_gh_executable(
        executable,
        record=record,
        sentinel=sentinel,
        child_identity=child_identity,
        endpoints=endpoints,
        github_inputs=github_inputs,
    )
    read_descriptor, write_descriptor = os.pipe()
    try:
        os.write(write_descriptor, b"producer-boundary-test-token")
    finally:
        os.close(write_descriptor)
    try:
        result = _run(
            [
                sys.executable,
                "-B",
                str(tool),
                "github-actions",
                *trusted,
                "--output-root",
                str(output_root),
                "--run-id",
                str(run_id),
                "--self-test-raw-evidence",
                "--self-test-gh-executable",
                str(executable),
            ],
            success=False,
            environment_overrides={TOKEN_DESCRIPTOR: str(read_descriptor)},
            pass_fds=(read_descriptor,),
        )
    finally:
        os.close(read_descriptor)
    if "stdout exceeds the 33554432-byte safety limit" not in result.stderr:
        raise RuntimeError(
            "GitHub producer did not reject bounded log stdout: " + result.stderr
        )
    calls = [
        json.loads(line)
        for line in record.read_text(encoding="utf-8").splitlines()
    ]
    observed_endpoints = [
        next(argument for argument in call if argument.startswith("repos/"))
        for call in calls
    ]
    if observed_endpoints != [
        endpoints["run"],
        endpoints["jobs"],
        endpoints["content"],
        endpoints["logs"],
    ]:
        raise RuntimeError("GitHub producer lost exact attempt-specific API binding")
    if any(
        call[1:4] != ["api", "--hostname", "github.com"]
        for call in calls
    ):
        raise RuntimeError("GitHub producer did not pin the official API host")
    _require_descendant_gone(
        sentinel,
        child_identity,
        "failed GitHub producer command",
    )


def _write_success_escape_executable(
    path: Path,
    *,
    version_commit: str,
    sentinel: Path,
    child_identity: Path,
) -> None:
    child = (
        "import time; from pathlib import Path; time.sleep(0.5); "
        f"Path({str(sentinel)!r}).write_text('survived', encoding='utf-8')"
    )
    source = f"""#!/usr/bin/python3
import subprocess
import sys
from pathlib import Path
if sys.argv[1:] == ["--version"]:
    descendant = subprocess.Popen(
        ["/usr/bin/python3", "-c", {child!r}],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    raw = Path(f"/proc/{{descendant.pid}}/stat").read_bytes()
    start_time = raw[raw.rfind(b")") + 2:].split()[19]
    Path({str(child_identity)!r}).write_bytes(
        str(descendant.pid).encode("ascii") + b" " + start_time
    )
    print("litci 0.0.0 ({version_commit})")
    raise SystemExit(0)
raise SystemExit(7)
"""
    path.write_text(source, encoding="utf-8")
    path.chmod(0o755)


def _write_overflow_executable(
    path: Path,
    *,
    version_commit: str,
    sentinel: Path,
    child_identity: Path,
) -> None:
    child = (
        "import time; from pathlib import Path; time.sleep(0.5); "
        f"Path({str(sentinel)!r}).write_text('survived', encoding='utf-8')"
    )
    source = f"""#!/usr/bin/python3
import os
import subprocess
import sys
from pathlib import Path
if sys.argv[1:] == ["--version"]:
    print("litci 0.0.0 ({version_commit})")
    raise SystemExit(0)
descendant = subprocess.Popen(
    ["/usr/bin/python3", "-c", {child!r}],
    stdin=subprocess.DEVNULL,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
    start_new_session=True,
)
raw = Path(f"/proc/{{descendant.pid}}/stat").read_bytes()
start_time = raw[raw.rfind(b")") + 2:].split()[19]
Path({str(child_identity)!r}).write_bytes(
    str(descendant.pid).encode("ascii") + b" " + start_time
)
chunk = b"x" * 65536
while True:
    os.write(1, chunk)
"""
    path.write_text(source, encoding="utf-8")
    path.chmod(0o755)


def _write_gh_executable(
    path: Path,
    *,
    record: Path,
    sentinel: Path,
    child_identity: Path,
    endpoints: dict[str, str],
    github_inputs: dict[str, Path],
) -> None:
    child = (
        "import time; from pathlib import Path; time.sleep(0.5); "
        f"Path({str(sentinel)!r}).write_text('survived', encoding='utf-8')"
    )
    mapping = {
        endpoints["run"]: str(github_inputs["run"]),
        endpoints["jobs"]: str(github_inputs["jobs"]),
        endpoints["content"]: str(github_inputs["content"]),
    }
    source = f"""#!/usr/bin/python3
import json
import os
import subprocess
import sys
from pathlib import Path
with Path({str(record)!r}).open("a", encoding="utf-8") as handle:
    handle.write(json.dumps(sys.argv) + "\\n")
endpoint = next(value for value in sys.argv if value.startswith("repos/"))
mapping = {mapping!r}
if endpoint in mapping:
    sys.stdout.buffer.write(Path(mapping[endpoint]).read_bytes())
    raise SystemExit(0)
if endpoint != {endpoints["logs"]!r}:
    raise SystemExit(3)
descendant = subprocess.Popen(
    ["/usr/bin/python3", "-c", {child!r}],
    stdin=subprocess.DEVNULL,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
    start_new_session=True,
)
raw = Path(f"/proc/{{descendant.pid}}/stat").read_bytes()
start_time = raw[raw.rfind(b")") + 2:].split()[19]
Path({str(child_identity)!r}).write_bytes(
    str(descendant.pid).encode("ascii") + b" " + start_time
)
chunk = b"x" * 65536
while True:
    os.write(1, chunk)
"""
    path.write_text(source, encoding="utf-8")
    path.chmod(0o755)
    if not stat.S_ISREG(path.lstat().st_mode):
        raise RuntimeError("hostile gh boundary is not a regular file")


def _require_descendant_gone(
    sentinel: Path,
    child_identity: Path,
    label: str,
) -> None:
    identity = child_identity.read_text(encoding="ascii").split()
    if len(identity) != 2:
        raise RuntimeError(f"{label} did not record an exact process identity")
    pid = int(identity[0])
    expected_start_time = identity[1]
    deadline = time.monotonic() + 2
    while True:
        try:
            raw = Path(f"/proc/{pid}/stat").read_bytes()
        except FileNotFoundError:
            break
        fields = raw[raw.rfind(b")") + 2 :].split()
        if len(fields) <= 19:
            raise RuntimeError(f"{label} descendant identity became malformed")
        if fields[19].decode("ascii") != expected_start_time:
            break
        if time.monotonic() >= deadline:
            raise RuntimeError(
                f"{label} left exact descendant identity {pid} alive"
            )
        time.sleep(0.02)
    time.sleep(0.55)
    if sentinel.exists():
        raise RuntimeError(f"{label} left a descendant process alive")


def _run(
    command: list[str],
    *,
    success: bool,
    environment_overrides: dict[str, str] | None = None,
    pass_fds: tuple[int, ...] = (),
) -> subprocess.CompletedProcess[str]:
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.upper().endswith("_TOKEN")
        and all(
            fragment not in key.upper()
            for fragment in ("SECRET", "PASSWORD", "CREDENTIAL")
        )
    }
    environment.update(environment_overrides or {})
    result = subprocess.run(
        command,
        env=environment,
        pass_fds=pass_fds,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        timeout=30,
        check=False,
    )
    if success and result.returncode != 0:
        raise RuntimeError(
            f"command failed ({result.returncode}): {' '.join(command)}\n"
            + result.stderr
        )
    if not success and result.returncode == 0:
        raise RuntimeError(f"command unexpectedly passed: {' '.join(command)}")
    return result
