"""Public command-boundary mutation canaries for the capability manifest."""

from __future__ import annotations

import copy
import json
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

from cargo_test_manifest import GateError


CHECKER = Path(__file__).with_name("check-capability-test-manifest")
ROOT = Path(__file__).resolve().parents[2]
MANIFEST = Path(__file__).with_name("capability-test-manifest.json")


def _run(manifest: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [
            sys.executable,
            "-B",
            str(CHECKER),
            "--repository-root",
            str(ROOT),
            "--manifest",
            str(manifest),
        ],
        check=False,
        capture_output=True,
        text=True,
    )


def _write(path: Path, value: dict[str, Any]) -> None:
    path.write_text(
        json.dumps(value, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


def self_test_negative() -> int:
    """Prove route removal, identity, tier, and command drift fail publicly."""

    try:
        baseline_value = json.loads(MANIFEST.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise GateError(f"could not load self-test manifest: {error}") from error
    with tempfile.TemporaryDirectory(
        prefix="greenlit-capability-manifest-"
    ) as directory:
        temporary = Path(directory)
        baseline = temporary / "baseline.json"
        _write(baseline, baseline_value)
        result = _run(baseline)
        if result.returncode != 0:
            raise GateError(
                "clean capability command-boundary canary failed\n"
                f"{result.stdout}{result.stderr}"
            )

        mutations: list[tuple[str, dict[str, Any], str]] = []
        removed = copy.deepcopy(baseline_value)
        removed["workflow_routes"].pop()
        mutations.append(("route removal", removed, "workflow routes differ"))

        identity = copy.deepcopy(baseline_value)
        identity["workflow_routes"][0]["job"] += "-substitute"
        mutations.append(("job substitution", identity, "workflow routes differ"))

        tier = copy.deepcopy(baseline_value)
        tier["workflow_routes"][0]["runs_on"] = "homelab"
        mutations.append(("tier substitution", tier, "route policy differs"))

        prerequisite = copy.deepcopy(baseline_value)
        prerequisite["workflow_routes"][0]["prerequisites"][0]["run"] += "\ntrue"
        mutations.append(
            ("prerequisite substitution", prerequisite, "step")
        )

        execution = copy.deepcopy(baseline_value)
        execution["workflow_routes"][0]["executions"][0]["run"] = "true"
        mutations.append(("execution substitution", execution, "step"))

        for index, (label, value, expected) in enumerate(mutations):
            manifest = temporary / f"negative-{index}.json"
            _write(manifest, value)
            result = _run(manifest)
            if result.returncode != 1 or expected not in result.stderr:
                raise GateError(
                    f"{label} did not fail at the public command boundary\n"
                    f"status={result.returncode}\n{result.stdout}{result.stderr}"
                )
    print(
        "capability manifest negative gate passed: route, job, tier, "
        "prerequisite, and execution mutations rejected"
    )
    return 0
