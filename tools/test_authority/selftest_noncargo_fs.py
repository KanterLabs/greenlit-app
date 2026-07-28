"""Filesystem and race canaries for public non-Cargo test authority."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import threading
from collections.abc import Callable
from pathlib import Path
from typing import Any

from .model import GateError
from .noncargo_policy import copy_policy_paths, required_harness_source_paths
from .noncargo_schema import POLICY_RELATIVE
from .noncargo_sources import MAX_FILE_BYTES, closure_digest


CHECKER = Path(__file__).resolve().parents[1] / "check-test-authority"
REPOSITORY = CHECKER.parent.parent


def _write(root: Path, relative: str, text: str) -> None:
    path = root / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def _copy_reviewed_harness(root: Path) -> None:
    for relative in (*copy_policy_paths(), *required_harness_source_paths()):
        source = REPOSITORY / relative
        target = root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target)


def fresh_case(temporary: Path, clean: dict[str, str], label: str) -> Path:
    """Copy one clean Cargo fixture and the exact reviewed non-Cargo harness."""

    root = temporary / f"noncargo-{label}"
    for relative, text in clean.items():
        _write(root, relative, text)
    _copy_reviewed_harness(root)
    return root


def load_policy(root: Path) -> dict[str, Any]:
    """Load one copied policy for a controlled negative mutation."""

    try:
        return json.loads((root / POLICY_RELATIVE).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise GateError(f"could not load canary policy: {error}") from error


def refresh_reviewed_digest(root: Path, changed: str) -> None:
    """Re-seal one changed source closure inside a temporary canary fixture."""

    path = root / POLICY_RELATIVE
    value = load_policy(root)
    matches = [entry for entry in value["entries"] if changed in entry["sources"]]
    if len(matches) != 1:
        raise GateError(f"semantic canary source has no unique policy owner: {changed}")
    entry = matches[0]
    sources = {
        relative: (root / relative).read_bytes() for relative in entry["sources"]
    }
    entry["closure_sha256"] = closure_digest(sources)
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def hostile_environment(temporary: Path) -> dict[str, str]:
    """Set ambient Python and executable lookup state that isolation must ignore."""

    empty_path = temporary / "empty-path"
    poison = temporary / "ambient-python"
    empty_path.mkdir(exist_ok=True)
    cargo_marker = temporary / "cargo-was-executed"
    cargo = empty_path / "cargo"
    cargo.write_text(
        "#!/bin/sh\n: > \"${GREENLIT_CARGO_CANARY:?}\"\nexit 99\n",
        encoding="utf-8",
    )
    cargo.chmod(0o755)
    _write(
        poison,
        "test_authority/__init__.py",
        "raise RuntimeError('ambient import authority was trusted')\n",
    )
    (poison / "bytecode").mkdir()
    environment = os.environ.copy()
    environment.update(
        {
            "PATH": str(empty_path),
            "PYTHONHOME": str(poison),
            "PYTHONPATH": str(poison),
            "PYTHONPYCACHEPREFIX": str(poison / "bytecode"),
            "GREENLIT_CARGO_CANARY": str(cargo_marker),
        }
    )
    return environment


def _boundary(
    root: Path,
    environment: dict[str, str],
) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(
            [
                "/usr/bin/python3",
                "-I",
                "-B",
                str(CHECKER),
                "--repository-root",
                str(root),
            ],
            check=False,
            capture_output=True,
            env=environment,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise GateError(
            f"could not execute semantic boundary canary: {error}"
        ) from error


def require_rejection(
    root: Path,
    environment: dict[str, str],
    label: str,
    expected: str | tuple[str, ...],
    source: str | None = None,
    result: subprocess.CompletedProcess[str] | None = None,
) -> None:
    """Require a precise public rejection without Cargo or ambient bytecode."""

    cargo_marker = Path(environment["GREENLIT_CARGO_CANARY"])
    bytecode_prefix = Path(environment["PYTHONPYCACHEPREFIX"])
    if result is None:
        cargo_marker.unlink(missing_ok=True)
        result = _boundary(root, environment)
    expected_options = (expected,) if isinstance(expected, str) else expected
    source_error = source is not None and f"{root / source}:" not in result.stderr
    if (
        result.returncode != 1
        or not any(item in result.stderr for item in expected_options)
        or source_error
        or cargo_marker.exists()
        or any(bytecode_prefix.iterdir())
    ):
        raise GateError(
            f"{label} canary did not fail before Cargo at the public boundary\n"
            f"status={result.returncode}\n{result.stdout}{result.stderr}"
        )


def _source_symlink(root: Path) -> None:
    source = root / "tools/live_parity/errors.py"
    target = root / "symlink-canary.py"
    target.write_bytes(source.read_bytes())
    source.unlink()
    source.symlink_to("../../symlink-canary.py")


def _adjacent_bytecode(root: Path) -> None:
    (root / "tools/live_parity/errors.pyc").write_bytes(b"unreviewed bytecode")


def _bytecode_cache(root: Path) -> None:
    (root / "tools/live_parity/__pycache__").mkdir()


def _compiled_extension(root: Path) -> None:
    (root / "tools/live_parity/unreviewed.so").write_bytes(b"compiled extension")


def _special_node(root: Path) -> None:
    os.mkfifo(root / "tools/live_parity/unreviewed.py")


def _sourceless_package(root: Path) -> None:
    (root / "tools/subprocess").mkdir()


def _entry_limit(root: Path) -> None:
    directory = root / "tools/live_parity"
    for index in range(270):
        (directory / f"entry_canary_{index:03d}.py").write_text(
            "# bounded inventory canary\n",
            encoding="utf-8",
        )


def _path_limit(root: Path) -> None:
    directory = root / "tools/live_parity"
    for index in range(245):
        name = f"p{index:03d}_{'x' * 242}.py"
        (directory / name).write_text("# path canary\n", encoding="utf-8")


def _file_byte_limit(root: Path) -> None:
    path = root / "tools/live_parity/errors.py"
    path.write_bytes(path.read_bytes() + b"\n#" + b"x" * MAX_FILE_BYTES)


def _aggregate_byte_limit(root: Path) -> None:
    sources = sorted(
        source
        for entry in load_policy(root)["entries"]
        for source in entry["sources"]
        if source.endswith(".py")
    )
    if len(sources) < 18:
        raise GateError("reviewed policy has too few Python sources for byte canary")
    for relative in sources[:18]:
        path = root / relative
        raw = path.read_bytes().rstrip() + b"\n#"
        path.write_bytes(raw + b"x" * (MAX_FILE_BYTES - 1 - len(raw)))


def _filesystem_canaries(
    temporary: Path,
    clean: dict[str, str],
    environment: dict[str, str],
) -> int:
    compiled = "Python bytecode or compiled extensions are forbidden"
    cases: tuple[tuple[str, str, Callable[[Path], None]], ...] = (
        ("source-symlink", "regular non-symlink file", _source_symlink),
        ("adjacent-pyc", compiled, _adjacent_bytecode),
        ("pycache", compiled, _bytecode_cache),
        ("adjacent-extension", compiled, _compiled_extension),
        (
            "sourceless-package",
            "namespace or sourceless local package candidates are forbidden",
            _sourceless_package,
        ),
        ("inventory-entry-limit", "inventory exceeds traversal limits", _entry_limit),
        ("inventory-path-limit", "inventory exceeds traversal limits", _path_limit),
        (
            "per-file-byte-limit",
            "reviewed source exceeds the byte limit",
            _file_byte_limit,
        ),
        (
            "aggregate-byte-limit",
            "policy exceeds aggregate byte limit",
            _aggregate_byte_limit,
        ),
    )
    if hasattr(os, "mkfifo"):
        cases += (("special-node", "special source node is forbidden", _special_node),)
    for label, expected, mutate in cases:
        root = fresh_case(temporary, clean, label)
        mutate(root)
        require_rejection(root, environment, label, expected)
    return len(cases)


def _race_canary(
    temporary: Path,
    clean: dict[str, str],
    environment: dict[str, str],
) -> int:
    label = "source-metadata-race"
    relative = "tools/check-live-parity"
    root = fresh_case(temporary, clean, label)
    path = root / relative
    raw = path.read_bytes().rstrip() + b"\n#"
    if len(raw) >= MAX_FILE_BYTES - 64:
        raise GateError("reviewed race source leaves no bounded mutation space")
    path.write_bytes(raw + b"x" * (MAX_FILE_BYTES - 64 - len(raw)))
    refresh_reviewed_digest(root, relative)

    metadata = path.stat()
    stop = threading.Event()
    started = threading.Event()
    failures: list[OSError] = []

    def churn_metadata() -> None:
        counter = 1
        while not stop.is_set():
            try:
                os.utime(
                    path,
                    ns=(
                        metadata.st_atime_ns,
                        metadata.st_mtime_ns + counter * 1_000_000_000,
                    ),
                    follow_symlinks=False,
                )
            except OSError as error:
                failures.append(error)
                return
            started.set()
            counter = 1 if counter == 2 else 2

    worker = threading.Thread(target=churn_metadata, name=label)
    cargo_marker = Path(environment["GREENLIT_CARGO_CANARY"])
    cargo_marker.unlink(missing_ok=True)
    worker.start()
    try:
        if not started.wait(timeout=2):
            raise GateError("source metadata race worker did not start")
        result = _boundary(root, environment)
    finally:
        stop.set()
        worker.join(timeout=2)
    if worker.is_alive():
        raise GateError("source metadata race worker did not stop")
    if failures:
        raise GateError(f"source metadata race worker failed: {failures[0]}")
    require_rejection(
        root,
        environment,
        label,
        ("changed while", "changed during"),
        relative,
        result,
    )
    return 1


def filesystem_race_canaries(
    temporary: Path,
    clean: dict[str, str],
    environment: dict[str, str],
) -> int:
    """Run all filesystem and metadata-race cases at the public boundary."""

    return _filesystem_canaries(
        temporary, clean, environment
    ) + _race_canary(temporary, clean, environment)
