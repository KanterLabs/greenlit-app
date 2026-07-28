"""Reviewed-harness mutations for the capability manifest public gate."""

from __future__ import annotations

import json
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any

from cargo_test_manifest import GateError
from test_authority.noncargo_policy import (
    copy_policy_paths,
    required_harness_source_paths,
)
from test_authority.noncargo_schema import POLICY_RELATIVE
from test_authority.noncargo_sources import closure_digest


CHECKER = Path(__file__).with_name("check-capability-test-manifest")
ROOT = Path(__file__).resolve().parents[2]


def _run(manifest: Path, repository_root: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [
            sys.executable,
            "-B",
            str(CHECKER),
            "--repository-root",
            str(repository_root),
            "--manifest",
            str(manifest),
        ],
        check=False,
        capture_output=True,
        text=True,
    )


def _copy_workflows(destination: Path, value: dict[str, Any]) -> None:
    for relative in {route["workflow"] for route in value["workflow_routes"]}:
        source = ROOT / relative
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, target)


def _copy_harness_sources(destination: Path) -> None:
    for relative in (*copy_policy_paths(), *required_harness_source_paths()):
        source = ROOT / relative
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target)


def _replace_once(path: Path, old: str, new: str) -> None:
    content = path.read_text(encoding="utf-8")
    if content.count(old) != 1:
        raise GateError(f"harness canary expected one exact block in {path}")
    changed = content.replace(old, new, 1)
    path.write_text(changed, encoding="utf-8")
    if path.read_text(encoding="utf-8") != changed:
        raise GateError(f"harness canary could not verify mutation in {path}")


def _refresh_reviewed_digest(root: Path, changed: str) -> None:
    path = root / POLICY_RELATIVE
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise GateError(f"could not load harness policy canary: {error}") from error
    matches = [
        entry for entry in value["entries"] if changed in entry["sources"]
    ]
    if len(matches) != 1:
        raise GateError(f"harness canary source has no unique policy owner: {changed}")
    entry = matches[0]
    sources = {
        relative: (root / relative).read_bytes()
        for relative in entry["sources"]
    }
    entry["closure_sha256"] = closure_digest(sources)
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def harness_semantic_canaries(
    temporary: Path,
    manifest: Path,
    baseline_value: dict[str, Any],
) -> int:
    """Reject explicit bypasses even after coordinating the reviewed digest."""

    mutations = (
        (
            "shell immediate success",
            "tools/test-credential-capability",
            "set -euo pipefail\n",
            "set -euo pipefail\nexit 0\n",
            "non-Cargo harness immediate success",
        ),
        (
            "Python immediate success",
            "tools/tests/check-capability-test-manifest",
            "sys.dont_write_bytecode = True\n",
            "sys.dont_write_bytecode = True\nsys.exit(0)\n",
            "non-Cargo harness immediate success",
        ),
        (
            "delegated Python immediate success",
            "tools/tests/check-greenlit-init-copy-strategies",
            "import sys\n",
            "import sys\nsys.exit(0)\n",
            "non-Cargo harness immediate success",
        ),
        (
            "Python runtime substitution",
            "tools/live_parity/process.py",
            "import subprocess\n",
            "import subprocess\nsubprocess.run = lambda *_a, **_k: None\n",
            "non-Cargo runtime substitute",
        ),
    )
    for index, (label, relative, old, new, category) in enumerate(mutations):
        root = temporary / f"harness-root-{index}"
        root.mkdir()
        _copy_workflows(root, baseline_value)
        _copy_harness_sources(root)
        _replace_once(root / relative, old, new)
        _refresh_reviewed_digest(root, relative)
        result = _run(manifest, root)
        if (
            result.returncode != 1
            or f"{root / relative}:" not in result.stderr
            or category not in result.stderr
        ):
            raise GateError(
                f"{label} did not fail at its public source boundary\n"
                f"status={result.returncode}\n{result.stdout}{result.stderr}"
            )
    return len(mutations)
