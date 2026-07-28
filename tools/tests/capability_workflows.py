"""Strict non-Cargo capability route validation for the public manifest gate."""

from __future__ import annotations

from pathlib import Path
from typing import Any

from capability_workflow_policy import (
    load_governed_workflows,
    validate_route_policy,
    validate_step_policy,
    validate_workflow_bytes,
)
from capability_yaml import job_block, needs, scalar, steps
from cargo_test_manifest import GateError
from test_authority.noncargo_commands import CommandEdge, shell_command_edges
from test_authority.noncargo_policy import (
    validate_execution_commands,
    validate_harness_policy,
)


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
        (
            "provider-and-policy",
            "performance-policy",
            "credential-capability",
            "host-deep-path",
        ),
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
    (".github/workflows/ci.yml", "performance-policy"): (
        "homelab-heavy",
        ("runtime-integration",),
        frozenset({"docker-policy"}),
    ),
    (".github/workflows/ci.yml", "provider-and-policy"): (
        "homelab-heavy",
        ("runtime-integration",),
        frozenset({"stargz-provider"}),
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
        required_runner, required_needs, required_capabilities = REQUIRED_ROUTES[
            identity
        ]
        actual = (
            route["runs_on"],
            tuple(route["needs"]),
            frozenset(route["capabilities"]),
        )
        if actual != (
            required_runner,
            required_needs,
            required_capabilities,
        ):
            raise GateError(f"{identity!r} route policy differs from required binding")
    validate_route_policy(routes)
    return routes


def validate_workflow_routes(
    routes: list[dict[str, Any]], root: Path
) -> tuple[int, int, int]:
    """Bind every declared route to its exact workflow job and step commands."""

    governed_workflows = {workflow for workflow, _job in REQUIRED_ROUTES}
    workflow_lines = load_governed_workflows(root, governed_workflows)

    parsed_edges: dict[tuple[str, str], tuple[CommandEdge, ...]] = {}
    execution_edges: list[CommandEdge] = []
    for route in routes:
        relative = route["workflow"]
        path = root / relative
        _raw, lines = workflow_lines[relative]
        block = job_block(lines, route["job"], path)
        identity = (relative, route["job"])
        if scalar(block, "runs-on", path, route["job"]) != route["runs_on"]:
            raise GateError(f"{identity!r} runs-on differs from manifest")
        if needs(block, path, route["job"]) != route["needs"]:
            raise GateError(f"{identity!r} needs topology differs from manifest")
        actual_steps = steps(block, path, route["job"])
        validate_step_policy(identity, actual_steps)
        positions = {
            step.name: step.ordinal
            for step in actual_steps
            if step.name is not None
        }
        all_edges: list[CommandEdge] = []
        edges_by_ordinal: dict[int, tuple[CommandEdge, ...]] = {}
        for actual in actual_steps:
            current = (
                shell_command_edges(path, actual.command)
                if actual.kind == "run"
                else ()
            )
            edges_by_ordinal[actual.ordinal] = current
            all_edges.extend(current)
        parsed_edges[identity] = tuple(all_edges)
        for role in ("prerequisites", "executions"):
            expected_steps = route[role]
            expected_positions: list[int] = []
            for expected in expected_steps:
                name = expected["name"]
                if name not in positions:
                    raise GateError(f"{identity!r} is missing required step {name!r}")
                actual = actual_steps[positions[name]]
                expected_kind = "run" if "run" in expected else "uses"
                if (
                    actual.name != name
                    or actual.kind != expected_kind
                    or actual.command != expected[expected_kind]
                ):
                    raise GateError(f"{identity!r} step {name!r} command differs")
                expected_positions.append(positions[name])
                if role == "executions":
                    current_edges = edges_by_ordinal[actual.ordinal]
                    if not current_edges:
                        raise GateError(
                            f"{identity!r} execution step {name!r} has no "
                            "structured local command edge"
                        )
                    execution_edges.extend(current_edges)
            if expected_positions != sorted(expected_positions):
                raise GateError(f"{identity!r} {role} order differs from manifest")

    validate_workflow_bytes(workflow_lines)
    entrypoints, route_commands = validate_execution_commands(
        root,
        parsed_edges,
        tuple(execution_edges),
    )
    harness_count, source_count = validate_harness_policy(
        root, entrypoints, route_commands
    )
    return len(routes), harness_count, source_count
