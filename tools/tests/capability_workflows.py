"""Strict non-Cargo capability route validation for the public manifest gate."""

from __future__ import annotations

import re
from pathlib import Path
from typing import Any

from cargo_test_manifest import GateError


ROUTE_KEYS = {
    "capabilities",
    "executions",
    "job",
    "needs",
    "prerequisites",
    "runs_on",
    "workflow",
}
STEP_RUN_KEYS = {"name", "run"}
STEP_USES_KEYS = {"name", "uses"}
CAPABILITIES = {
    "credential-keyring",
    "docker-policy",
    "docker-runtime",
    "host-deep-path",
    "live-parity-compare",
    "live-parity-github",
    "live-parity-local",
    "release-dogfood",
    "stargz-provider",
}
REQUIRED_ROUTES = {
    (".github/workflows/ci.yml", "credential-capability"): (
        "homelab-heavy",
        ("ci",),
        frozenset({"credential-keyring"}),
    ),
    (".github/workflows/ci.yml", "dogfood"): (
        "homelab-heavy",
        ("provider-and-policy", "credential-capability", "host-deep-path"),
        frozenset({"release-dogfood"}),
    ),
    (".github/workflows/ci.yml", "host-deep-path"): (
        "homelab-heavy",
        ("ci",),
        frozenset({"host-deep-path"}),
    ),
    (".github/workflows/ci.yml", "live_parity_compare"): (
        "homelab-heavy",
        ("live_parity_local", "live_parity_github"),
        frozenset({"live-parity-compare"}),
    ),
    (".github/workflows/ci.yml", "live_parity_github"): (
        "homelab",
        ("ci",),
        frozenset({"live-parity-github"}),
    ),
    (".github/workflows/ci.yml", "live_parity_local"): (
        "homelab-heavy",
        ("ci",),
        frozenset({"live-parity-local"}),
    ),
    (".github/workflows/ci.yml", "provider-and-policy"): (
        "homelab-heavy",
        ("runtime-integration",),
        frozenset({"docker-policy", "stargz-provider"}),
    ),
    (".github/workflows/ci.yml", "runtime-integration"): (
        "homelab-heavy",
        ("ci",),
        frozenset({"docker-runtime"}),
    ),
    (".github/workflows/release.yml", "finalize"): (
        "homelab-heavy",
        ("prepare", "local_parity", "github_parity"),
        frozenset(
            {
                "credential-keyring",
                "docker-policy",
                "docker-runtime",
                "host-deep-path",
                "live-parity-compare",
                "release-dogfood",
                "stargz-provider",
            }
        ),
    ),
    (".github/workflows/release.yml", "github_parity"): (
        "homelab",
        ("prepare",),
        frozenset({"live-parity-github"}),
    ),
    (".github/workflows/release.yml", "local_parity"): (
        "homelab-heavy",
        ("prepare",),
        frozenset({"live-parity-local"}),
    ),
}
EXECUTION_MARKERS = (
    "tools/tests/check-capability-test-manifest --run-owner docker-runtime",
    "tools/tests/check-capability-test-manifest --run-owner docker-policy",
    "tools/tests/check-capability-test-manifest --run-owner host-deep-path",
    "tools/tests/check-greenlit-init-copy-strategies",
    "tools/test-credential-capability",
    "tools/test-stargz-provider",
    "tools/check-release-dogfood",
    "tools/check-live-parity local",
    "tools/check-live-parity github",
    "tools/check-live-parity compare",
    "tools/release-check finalize",
    "tools/compare-parity \\",
)
JOB_HEADER = re.compile(r"^  ([A-Za-z0-9_-]+):$")
STEP_HEADER = re.compile(r"^      - name: (.+)$")


def _text_list(value: Any, location: str, *, nonempty: bool) -> list[str]:
    if not isinstance(value, list) or (nonempty and not value):
        qualifier = "nonempty " if nonempty else ""
        raise GateError(f"{location} must be a {qualifier}array")
    result: list[str] = []
    for index, item in enumerate(value):
        if not isinstance(item, str) or not item or item.strip() != item:
            raise GateError(f"{location}[{index}] must be nonempty trimmed text")
        result.append(item)
    if len(result) != len(set(result)):
        raise GateError(f"{location} contains a duplicate")
    return result


def _step_list(value: Any, location: str) -> list[dict[str, str]]:
    if not isinstance(value, list) or not value:
        raise GateError(f"{location} must be a nonempty array")
    result: list[dict[str, str]] = []
    names: set[str] = set()
    for index, item in enumerate(value):
        item_location = f"{location}[{index}]"
        keys = set(item) if isinstance(item, dict) else set()
        if not isinstance(item, dict) or (
            keys != STEP_RUN_KEYS and keys != STEP_USES_KEYS
        ):
            raise GateError(
                f"{item_location} must contain name and exactly one of run or uses"
            )
        name = item["name"]
        command_key = "run" if "run" in item else "uses"
        command = item[command_key]
        if (
            not isinstance(name, str)
            or not name
            or name.strip() != name
            or not isinstance(command, str)
            or not command
        ):
            raise GateError(f"{item_location} contains invalid step text")
        if name in names:
            raise GateError(f"{location} repeats step {name!r}")
        names.add(name)
        result.append({"name": name, command_key: command})
    return result


def validate_route_schema(value: Any, path: Path) -> list[dict[str, Any]]:
    """Validate the fixed job/tier/topology inventory before reading workflows."""

    if not isinstance(value, list) or not value:
        raise GateError(f"{path}: workflow_routes must be a nonempty array")
    routes: list[dict[str, Any]] = []
    identities: set[tuple[str, str]] = set()
    previous: tuple[str, str] | None = None
    for index, route in enumerate(value):
        location = f"{path}: workflow_routes[{index}]"
        if not isinstance(route, dict) or set(route) != ROUTE_KEYS:
            raise GateError(f"{location} must contain exactly {sorted(ROUTE_KEYS)!r}")
        for field in ("workflow", "job", "runs_on"):
            text = route[field]
            if not isinstance(text, str) or not text or text.strip() != text:
                raise GateError(f"{location}.{field} must be nonempty trimmed text")
        route["capabilities"] = _text_list(
            route["capabilities"], f"{location}.capabilities", nonempty=True
        )
        unknown = set(route["capabilities"]) - CAPABILITIES
        if unknown:
            raise GateError(f"{location} has unknown capabilities {sorted(unknown)!r}")
        if route["capabilities"] != sorted(route["capabilities"]):
            raise GateError(f"{location}.capabilities must be sorted")
        route["needs"] = _text_list(
            route["needs"], f"{location}.needs", nonempty=True
        )
        route["prerequisites"] = _step_list(
            route["prerequisites"], f"{location}.prerequisites"
        )
        route["executions"] = _step_list(
            route["executions"], f"{location}.executions"
        )
        names = [item["name"] for item in route["prerequisites"]]
        names.extend(item["name"] for item in route["executions"])
        if len(names) != len(set(names)):
            raise GateError(f"{location} repeats a prerequisite/execution step")
        identity = (route["workflow"], route["job"])
        if identity in identities:
            raise GateError(f"{location} duplicates workflow job {identity!r}")
        if previous is not None and identity < previous:
            raise GateError(f"{path}: workflow_routes must be sorted")
        identities.add(identity)
        previous = identity
        routes.append(route)
    if identities != set(REQUIRED_ROUTES):
        raise GateError(
            "workflow routes differ from required capability inventory; "
            f"missing={sorted(set(REQUIRED_ROUTES) - identities)!r}, "
            f"unexpected={sorted(identities - set(REQUIRED_ROUTES))!r}"
        )
    for route in routes:
        identity = (route["workflow"], route["job"])
        runner, needs, capabilities = REQUIRED_ROUTES[identity]
        actual = (
            route["runs_on"],
            tuple(route["needs"]),
            frozenset(route["capabilities"]),
        )
        if actual != (runner, needs, capabilities):
            raise GateError(f"{identity!r} route policy differs from required binding")
    return routes


def _job_block(lines: list[str], job: str, path: Path) -> list[str]:
    matches = [
        index
        for index, line in enumerate(lines)
        if (match := JOB_HEADER.fullmatch(line)) and match.group(1) == job
    ]
    if len(matches) != 1:
        raise GateError(f"{path}: expected exactly one job id {job!r}")
    start = matches[0] + 1
    end = len(lines)
    for index in range(start, len(lines)):
        if JOB_HEADER.fullmatch(lines[index]) or (
            lines[index]
            and not lines[index].startswith((" ", "#"))
        ):
            end = index
            break
    return lines[start:end]


def _scalar(block: list[str], key: str, path: Path, job: str) -> str:
    prefix = f"    {key}: "
    values = [line[len(prefix) :] for line in block if line.startswith(prefix)]
    if len(values) != 1 or not values[0]:
        raise GateError(f"{path}: job {job!r} must declare exactly one {key}")
    return values[0]


def _needs(block: list[str], path: Path, job: str) -> list[str]:
    inline = [
        line[len("    needs: ") :]
        for line in block
        if line.startswith("    needs: ")
    ]
    headers = [index for index, line in enumerate(block) if line == "    needs:"]
    if len(inline) + len(headers) != 1:
        raise GateError(f"{path}: job {job!r} must declare exactly one needs")
    if inline:
        return [inline[0]]
    start = headers[0] + 1
    result: list[str] = []
    for line in block[start:]:
        if line.startswith("      - "):
            result.append(line[len("      - ") :])
            continue
        if line.strip() and not line.startswith("      "):
            break
    if not result:
        raise GateError(f"{path}: job {job!r} has an empty needs list")
    return result


def _run_text(step: list[str], path: Path, job: str, name: str) -> str | None:
    indices = [
        index for index, line in enumerate(step) if line.startswith("        run: ")
    ]
    if not indices:
        return None
    if len(indices) != 1:
        raise GateError(f"{path}: job {job!r} step {name!r} repeats run")
    index = indices[0]
    value = step[index][len("        run: ") :]
    if value not in {"|", "|-", ">", ">-"}:
        return value
    content: list[str] = []
    for line in step[index + 1 :]:
        if line and not line.startswith("          "):
            break
        content.append(line[10:] if line else "")
    if not content:
        raise GateError(f"{path}: job {job!r} step {name!r} has an empty run block")
    return "\n".join(content)


def _steps(block: list[str], path: Path, job: str) -> list[dict[str, str]]:
    starts = [
        index for index, line in enumerate(block) if STEP_HEADER.fullmatch(line)
    ]
    result: list[dict[str, str]] = []
    names: set[str] = set()
    for ordinal, start in enumerate(starts):
        end = starts[ordinal + 1] if ordinal + 1 < len(starts) else len(block)
        step = block[start:end]
        name = STEP_HEADER.fullmatch(step[0]).group(1)  # type: ignore[union-attr]
        if name in names:
            raise GateError(f"{path}: job {job!r} repeats step name {name!r}")
        names.add(name)
        run = _run_text(step, path, job, name)
        uses = [
            line[len("        uses: ") :]
            for line in step
            if line.startswith("        uses: ")
        ]
        if run is not None and uses:
            raise GateError(f"{path}: job {job!r} step {name!r} mixes run and uses")
        item = {"name": name}
        if run is not None:
            item["run"] = run
        elif len(uses) == 1:
            item["uses"] = uses[0]
        result.append(item)
    return result


def validate_workflow_routes(routes: list[dict[str, Any]], root: Path) -> int:
    """Bind every declared route to its exact workflow job and step commands."""

    parsed: dict[tuple[str, str], list[dict[str, str]]] = {}
    execution_bindings: set[tuple[str, str, str]] = set()
    for route in routes:
        relative = route["workflow"]
        path = root / relative
        if path.is_symlink() or not path.is_file():
            raise GateError(f"{path}: workflow must be a regular non-symlink file")
        try:
            lines = path.read_text(encoding="utf-8").splitlines()
        except (OSError, UnicodeError) as error:
            raise GateError(f"could not read workflow {path}: {error}") from error
        block = _job_block(lines, route["job"], path)
        identity = (relative, route["job"])
        if _scalar(block, "runs-on", path, route["job"]) != route["runs_on"]:
            raise GateError(f"{identity!r} runs-on differs from manifest")
        if _needs(block, path, route["job"]) != route["needs"]:
            raise GateError(f"{identity!r} needs topology differs from manifest")
        actual_steps = _steps(block, path, route["job"])
        parsed[identity] = actual_steps
        positions = {step["name"]: index for index, step in enumerate(actual_steps)}
        for role in ("prerequisites", "executions"):
            expected_steps = route[role]
            expected_positions: list[int] = []
            for expected in expected_steps:
                name = expected["name"]
                if name not in positions:
                    raise GateError(f"{identity!r} is missing required step {name!r}")
                actual = actual_steps[positions[name]]
                if actual != expected:
                    raise GateError(f"{identity!r} step {name!r} command differs")
                expected_positions.append(positions[name])
                if role == "executions":
                    execution_bindings.add((relative, route["job"], name))
            if expected_positions != sorted(expected_positions):
                raise GateError(f"{identity!r} {role} order differs from manifest")

    discovered: set[tuple[str, str, str]] = set()
    for (workflow, job), steps in parsed.items():
        for step in steps:
            run = step.get("run", "")
            if any(marker in run for marker in EXECUTION_MARKERS):
                discovered.add((workflow, job, step["name"]))
    if not discovered <= execution_bindings:
        raise GateError(
            "capability execution steps are not inventory-bound; "
            f"unbound={sorted(discovered - execution_bindings)!r}"
        )
    return len(routes)
