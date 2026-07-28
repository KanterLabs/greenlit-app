"""Executable boundary fixtures used by parity producer behavior gates."""

from __future__ import annotations

import stat
from pathlib import Path


def write_success_escape_executable(
    path: Path,
    *,
    version_commit: str,
    sentinel: Path,
    child_identity: Path,
) -> None:
    """Write a release boundary that succeeds while a descendant escapes."""
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


def write_overflow_executable(
    path: Path,
    *,
    version_commit: str,
    sentinel: Path,
    child_identity: Path,
) -> None:
    """Write a release boundary that emits unbounded output with a descendant."""
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


def write_recorded_gh_executable(
    path: Path,
    *,
    record: Path,
    endpoints: dict[str, str],
    github_inputs: dict[str, Path],
) -> None:
    """Write a recorded GitHub API boundary with successful fixture responses."""
    mapping = {
        endpoints["run"]: str(github_inputs["run"]),
        endpoints["jobs"]: str(github_inputs["jobs"]),
        endpoints["content"]: str(github_inputs["content"]),
        endpoints["log"]: str(github_inputs["log"]),
    }
    source = f"""#!/usr/bin/python3
import json
import sys
from pathlib import Path
with Path({str(record)!r}).open("a", encoding="utf-8") as handle:
    handle.write(json.dumps(sys.argv) + "\\n")
endpoint = next(value for value in sys.argv if value.startswith("repos/"))
mapping = {mapping!r}
if endpoint not in mapping:
    raise SystemExit(3)
sys.stdout.buffer.write(Path(mapping[endpoint]).read_bytes())
"""
    path.write_text(source, encoding="utf-8")
    path.chmod(0o755)
    if not stat.S_ISREG(path.lstat().st_mode):
        raise RuntimeError("recorded gh boundary is not a regular file")


def write_gh_executable(
    path: Path,
    *,
    record: Path,
    sentinel: Path,
    child_identity: Path,
    endpoints: dict[str, str],
    github_inputs: dict[str, Path],
) -> None:
    """Write a recorded GitHub boundary whose job-log response overflows."""
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
if endpoint != {endpoints["log"]!r}:
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
