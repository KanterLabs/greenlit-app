"""Public command-boundary mutation canaries for the capability manifest."""

from __future__ import annotations

import copy
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

from cargo_test_manifest import GateError


CHECKER = Path(__file__).with_name("check-capability-test-manifest")
ROOT = Path(__file__).resolve().parents[2]
MANIFEST = Path(__file__).with_name("capability-test-manifest.json")


def _run(
    manifest: Path,
    repository_root: Path = ROOT,
    environment_overrides: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    environment = os.environ.copy()
    if environment_overrides is not None:
        environment.update(environment_overrides)
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
        env=environment,
        text=True,
    )


def _copy_workflows(destination: Path, value: dict[str, Any]) -> None:
    workflows = {
        route["workflow"]
        for route in value["workflow_routes"]
    }
    for relative in workflows:
        source = ROOT / relative
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, target)


def _replace_once(path: Path, old: str, new: str) -> None:
    content = path.read_text(encoding="utf-8")
    if content.count(old) != 1:
        raise GateError(f"self-test mutation expected one exact block in {path}")
    mutated = content.replace(old, new, 1)
    if mutated == content:
        raise GateError(f"self-test mutation did not change {path}")
    path.write_text(mutated, encoding="utf-8")
    if path.read_text(encoding="utf-8") != mutated:
        raise GateError(f"self-test mutation could not be verified in {path}")


def _route(
    value: dict[str, Any],
    workflow: str,
    job: str,
) -> dict[str, Any]:
    matches = [
        route
        for route in value["workflow_routes"]
        if (route["workflow"], route["job"]) == (workflow, job)
    ]
    if len(matches) != 1:
        raise GateError(f"self-test expected one route for {(workflow, job)!r}")
    return matches[0]


def _step(
    route: dict[str, Any],
    role: str,
    name: str,
) -> dict[str, str]:
    matches = [step for step in route[role] if step["name"] == name]
    if len(matches) != 1 or "run" not in matches[0]:
        raise GateError(f"self-test expected one run step {name!r}")
    return matches[0]


def _render_run_step(name: str, run: str) -> str:
    lines = run.removesuffix("\n").split("\n")
    if len(lines) == 1:
        return f"      - name: {name}\n        run: {lines[0]}\n"
    body = "".join(f"          {line}\n" for line in lines)
    return f"      - name: {name}\n        run: |\n{body}"


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

        release_defaults = _run(
            baseline,
            environment_overrides={
                "CARGO_BUILD_INCREMENTAL": "false",
                "CARGO_ENCODED_RUSTFLAGS": "",
                "CARGO_INCREMENTAL": "0",
                "RUSTFLAGS": "",
            },
        )
        if release_defaults.returncode != 0:
            raise GateError(
                "release-default environment canary failed\n"
                f"{release_defaults.stdout}{release_defaults.stderr}"
            )

        no_tools = temporary / "no-tools"
        no_tools.mkdir()
        environment_canaries = (
            ("CARGO_INCREMENTAL", "1"),
            ("RUSTFLAGS", "--cfg litci_test_boundaries"),
            ("RUSTUP_TOOLCHAIN", "stable"),
        )
        for variable, value in environment_canaries:
            result = _run(
                baseline,
                environment_overrides={
                    "PATH": str(no_tools),
                    variable: value,
                },
            )
            expected_environment_rejection = (
                "capability test manifest gate failed: release-profile Cargo "
                "environment contains forbidden compiler/profile customization: "
                f"{variable}\n"
            )
            if (
                result.returncode != 1
                or result.stdout
                or result.stderr != expected_environment_rejection
            ):
                raise GateError(
                    f"ambient release customization {variable} did not fail "
                    "before Cargo\n"
                    f"status={result.returncode}\n{result.stdout}{result.stderr}"
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
            (
                "prerequisite substitution",
                prerequisite,
                "checker-owned command policy",
            )
        )

        execution = copy.deepcopy(baseline_value)
        execution["workflow_routes"][0]["executions"][0]["run"] = "true"
        mutations.append(
            ("execution substitution", execution, "checker-owned command policy")
        )

        profiles = copy.deepcopy(baseline_value)
        performance = next(
            entry
            for entry in profiles["targets"]
            if (entry["package"], entry["target"])
            == ("greenlit-app", "performance_budgets")
        )
        policy = next(
            entry
            for entry in profiles["targets"]
            if (entry["package"], entry["target"])
            == ("greenlit-app", "policy_modes_live")
        )
        performance["cargo_profile"], policy["cargo_profile"] = (
            policy["cargo_profile"],
            performance["cargo_profile"],
        )
        mutations.append(
            (
                "release-profile substitution",
                profiles,
                "release-profile targets differ from the fixed authority",
            )
        )

        for index, (label, value, expected) in enumerate(mutations):
            manifest = temporary / f"negative-{index}.json"
            _write(manifest, value)
            result = _run(manifest)
            if result.returncode != 1 or expected not in result.stderr:
                raise GateError(
                    f"{label} did not fail at the public command boundary\n"
                    f"status={result.returncode}\n{result.stdout}{result.stderr}"
                )

        coordinated = (
            (
                "credential prerequisite",
                ".github/workflows/ci.yml",
                "credential-capability",
                "prerequisites",
                "Provision isolated keyring prerequisites",
            ),
            (
                "copy execution",
                ".github/workflows/ci.yml",
                "runtime-integration",
                "executions",
                "Reflink and bounded-stream copy strategies",
            ),
        )
        for index, (label, workflow, job, role, name) in enumerate(coordinated):
            value = copy.deepcopy(baseline_value)
            route = _route(value, workflow, job)
            step = _step(route, role, name)
            old_block = _render_run_step(name, step["run"])
            step["run"] = "true"
            root = temporary / f"coordinated-root-{index}"
            root.mkdir()
            _copy_workflows(root, baseline_value)
            _replace_once(
                root / workflow,
                old_block,
                _render_run_step(name, "true"),
            )
            manifest = temporary / f"coordinated-{index}.json"
            _write(manifest, value)
            result = _run(manifest, root)
            if (
                result.returncode != 1
                or "checker-owned command policy" not in result.stderr
            ):
                raise GateError(
                    f"coordinated {label} substitution did not fail closed\n"
                    f"status={result.returncode}\n{result.stdout}{result.stderr}"
                )

        workflow_mutations = (
            (
                "step condition",
                ".github/workflows/ci.yml",
                "      - name: Reflink and bounded-stream copy strategies\n",
                "      - name: Reflink and bounded-stream copy strategies\n"
                "        if: ${{ false }}\n",
            ),
            (
                "step continue-on-error",
                ".github/workflows/ci.yml",
                "      - name: Reflink and bounded-stream copy strategies\n",
                "      - name: Reflink and bounded-stream copy strategies\n"
                "        continue-on-error: true\n",
            ),
            (
                "job condition",
                ".github/workflows/ci.yml",
                "  credential-capability:\n",
                "  credential-capability:\n"
                "    if: ${{ false }}\n",
            ),
            (
                "workflow shell default",
                ".github/workflows/ci.yml",
                "permissions:\n",
                "defaults:\n"
                "  run:\n"
                "    shell: bash {0}\n\n"
                "permissions:\n",
            ),
        )
        for index, (label, workflow, old, new) in enumerate(workflow_mutations):
            root = temporary / f"workflow-root-{index}"
            root.mkdir()
            _copy_workflows(root, baseline_value)
            _replace_once(root / workflow, old, new)
            result = _run(baseline, root)
            if (
                result.returncode != 1
                or "workflow differs from checker-owned policy" not in result.stderr
            ):
                raise GateError(
                    f"{label} did not fail at the public command boundary\n"
                    f"status={result.returncode}\n{result.stdout}{result.stderr}"
                )
    print(
        "capability manifest negative gate passed: 15 environment, route, profile, "
        "coordinated, condition, and command mutations rejected"
    )
    return 0
