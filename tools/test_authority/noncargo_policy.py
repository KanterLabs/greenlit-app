"""Independent reviewed-source authority for non-Cargo capability harnesses."""

from __future__ import annotations

from collections import Counter, deque
from pathlib import Path

from .model import GateError
from .noncargo_bootstrap import validate_isolated_entrypoint
from .noncargo_commands import (
    CommandEdge,
    shell_command_edges,
    verify_release_wrapper,
)
from .noncargo_graph import (
    shell_local_references,
)
from .noncargo_python_graph import python_command_references
from .noncargo_schema import Entry, POLICY_RELATIVE, Policy, load_policy
from .noncargo_semantics import validate_semantics
from .noncargo_imports import python_dependencies, shell_dependencies
from .noncargo_sources import (
    closure_digest,
    inventory_python,
    read_declared_sources,
    reject_python_bytecode,
    source_language,
    verify_declared_sources,
)

CONTROL_ROOT = "tools/test_authority/"

def _verify_schema(policy: Policy) -> dict[str, Entry]:
    entries = {item.entrypoint: item for item in policy.entries}
    if len(entries) != len(policy.entries):
        raise GateError("non-Cargo policy repeats an entry point")
    if not set(policy.route_entrypoints).issubset(entries):
        raise GateError("non-Cargo route entry points include an undeclared harness")
    if not {
        command.entrypoint for command in policy.execution_commands
    }.issubset(entries):
        raise GateError("non-Cargo execution command names an undeclared harness")
    owners: dict[str, str] = {}
    graph: dict[str, set[str]] = {entrypoint: set() for entrypoint in entries}
    for entry in policy.entries:
        dynamic_identities = [
            (edge.source, edge.path)
            for edge in entry.dynamic_sources
        ]
        delegate_identities = [
            (edge.source, edge.path, edge.target, edge.authority)
            for edge in entry.delegates
        ]
        if len(dynamic_identities) != len(set(dynamic_identities)):
            raise GateError(f"{entry.entrypoint}: dynamic source edge is repeated")
        if len(delegate_identities) != len(set(delegate_identities)):
            raise GateError(f"{entry.entrypoint}: delegate edge is repeated")
        if entry.entrypoint not in entry.sources:
            raise GateError(f"{entry.entrypoint}: entry point is absent from its closure")
        for source in entry.sources:
            if source.startswith(CONTROL_ROOT):
                raise GateError(
                    f"{entry.entrypoint}: source closure contains independent "
                    f"authority control state: {source}"
                )
            previous = owners.setdefault(source, entry.entrypoint)
            if previous != entry.entrypoint:
                raise GateError(
                    f"{source}: reviewed source is owned by both {previous} "
                    f"and {entry.entrypoint}"
                )
        for dynamic in entry.dynamic_sources:
            if dynamic.path not in entry.sources or dynamic.source not in entry.sources:
                raise GateError(f"{entry.entrypoint}: dynamic source edge escapes its closure")
        for delegate in entry.delegates:
            if delegate.source not in entry.sources:
                raise GateError(f"{entry.entrypoint}: delegate edge has an unowned source")
            if delegate.target is not None:
                if delegate.path != delegate.target:
                    raise GateError(
                        f"{entry.entrypoint}: delegate target and command path differ"
                    )
                if delegate.target not in entries:
                    raise GateError(f"{entry.entrypoint}: delegate target is undeclared")
                graph[entry.entrypoint].add(delegate.target)
            elif delegate.path in entries:
                raise GateError(
                    f"{entry.entrypoint}: declared harness delegate must use "
                    "a target edge, not external authority text"
                )
        for target in entry.import_targets:
            if target not in entries:
                raise GateError(f"{entry.entrypoint}: import target is undeclared")
            graph[entry.entrypoint].add(target)
    reachable = set(policy.route_entrypoints)
    queue = deque(reachable)
    while queue:
        for target in graph[queue.popleft()]:
            if target not in reachable:
                reachable.add(target)
                queue.append(target)
    if reachable != set(entries):
        raise GateError(
            "reviewed harness graph contains unreachable entries: "
            f"{sorted(set(entries) - reachable)!r}"
        )
    return entries


def _verify_entry(
    root: Path,
    entry: Entry,
    owners: dict[str, str],
    raw_sources: dict[str, bytes],
    graph_paths: set[str],
) -> None:
    own = {relative: raw_sources[relative] for relative in entry.sources}
    reject_python_bytecode(root, entry.sources)
    actual_entry_language = source_language(
        entry.entrypoint, own[entry.entrypoint]
    )
    if actual_entry_language != entry.language:
        raise GateError(
            f"{root / entry.entrypoint}: entry-point language differs from policy"
        )
    isolated_sources = {
        entry.entrypoint,
        *(
            edge.path
            for edge in entry.dynamic_sources
            if source_language(edge.source, own[edge.source]) == "python"
        ),
    }
    isolated_sources = {
        relative
        for relative in isolated_sources
        if source_language(relative, own[relative]) == "python"
    }
    for relative in isolated_sources:
        validate_isolated_entrypoint(
            root / relative,
            own[relative],
            entry.sources,
            entry.import_roots,
        )
    for relative, raw in own.items():
        validate_semantics(
            root / relative,
            relative,
            raw,
            source_language(relative, raw),
        )
    for relative, raw in own.items():
        language = source_language(relative, raw)
        expected_delegates = Counter(
            {
                edge.path: edge.count
                for edge in entry.delegates
                if edge.source == relative
            }
        )
        expected_dynamic = Counter(
            {
                edge.path: edge.count
                for edge in entry.dynamic_sources
                if edge.source == relative
            }
        )
        if language == "python":
            actual = python_command_references(
                root / relative,
                raw,
                graph_paths,
            )
            expected = expected_delegates + expected_dynamic
            if actual != expected:
                raise GateError(
                    f"{root / relative}: static Python delegate graph differs; "
                    f"missing={sorted((expected - actual).elements())!r}, "
                    f"unexpected={sorted((actual - expected).elements())!r}"
                )
            continue
        try:
            text = raw.decode("utf-8")
        except UnicodeError as error:
            raise GateError(f"{root / relative}: source must be UTF-8: {error}") from error
        command_edges = shell_command_edges(root / relative, text)
        actual_commands = Counter(edge.entrypoint for edge in command_edges)
        if any(edge.executable == "run_release_gate" for edge in command_edges):
            verify_release_wrapper(root / relative, text)
        if actual_commands != expected_delegates:
            raise GateError(
                f"{root / relative}: structured shell delegate graph differs; "
                f"missing={sorted((expected_delegates - actual_commands).elements())!r}, "
                f"unexpected={sorted((actual_commands - expected_delegates).elements())!r}"
            )
        references, arrays = shell_local_references(root / relative, raw)
        for path, count in expected_dynamic.items():
            if references[path] != count:
                raise GateError(
                    f"{root / relative}: static source-transfer edge count "
                    f"differs for {path!r}"
                )
        allowed = (
            set(expected_delegates)
            | set(expected_dynamic)
            | {entry.entrypoint}
        )
        unexpected_references = set(references) - allowed
        if unexpected_references:
            raise GateError(
                f"{root / relative}: local shell path is absent from the "
                f"reviewed graph: {sorted(unexpected_references)!r}"
            )
        bound = arrays.get("authority_tools")
        if bound is not None:
            if (
                len(bound) != len(set(bound))
                or set(bound) != set(expected_delegates)
            ):
                raise GateError(
                    f"{root / relative}: authority_tools binding differs "
                    "from the structured delegate graph"
                )
    inventory = inventory_python(root, entry.inventory_roots)
    expected_inventory = {
        source
        for source in entry.sources
        if any(
            Path(source).is_relative_to(Path(directory))
            for directory in entry.inventory_roots
        )
    }
    if inventory != expected_inventory:
        raise GateError(
            f"{entry.entrypoint}: Python source inventory differs; "
            f"missing={sorted(inventory - expected_inventory)!r}, "
            f"unexpected={sorted(expected_inventory - inventory)!r}"
        )
    visited = {entry.entrypoint}
    queue = deque([entry.entrypoint])
    for dynamic in entry.dynamic_sources:
        visited.add(dynamic.path)
        queue.append(dynamic.path)
    used_import_targets: set[str] = set()
    used_authority_imports: set[str] = set()
    while queue:
        relative = queue.popleft()
        raw = own[relative]
        language = source_language(relative, raw)
        if language == "python":
            dependencies, used_authorities = python_dependencies(
                root,
                relative,
                raw,
                entry.import_roots,
                entry.authority_imports,
                allow_path_bootstrap=relative in isolated_sources,
            )
            used_authority_imports.update(used_authorities)
        else:
            dependencies = shell_dependencies(root, relative, raw)
        for dependency in dependencies:
            try:
                dependency_relative = dependency.relative_to(root).as_posix()
            except ValueError as error:
                raise GateError(
                    f"{root / relative}: local dependency escapes repository"
                ) from error
            if dependency_relative in own:
                if dependency_relative not in visited:
                    visited.add(dependency_relative)
                    queue.append(dependency_relative)
                continue
            owner = owners.get(dependency_relative)
            if owner is None or owner not in entry.import_targets:
                raise GateError(
                    f"{root / relative}: local import is absent from the exact "
                    f"reviewed closure: {dependency_relative}"
                )
            used_import_targets.add(owner)
    if visited != set(entry.sources):
        raise GateError(
            f"{entry.entrypoint}: declared sources are not reachable; "
            f"unreachable={sorted(set(entry.sources) - visited)!r}"
        )
    if used_import_targets != set(entry.import_targets):
        raise GateError(
            f"{entry.entrypoint}: reviewed import edges differ; "
            f"unused={sorted(set(entry.import_targets) - used_import_targets)!r}"
        )
    if used_authority_imports != set(entry.authority_imports):
        raise GateError(
            f"{entry.entrypoint}: independent authority imports differ; "
            f"unused={sorted(set(entry.authority_imports) - used_authority_imports)!r}"
        )
    if closure_digest(own) != entry.closure_sha256:
        raise GateError(
            f"{root / entry.entrypoint}: capability harness closure differs "
            "from reviewed policy"
        )


def validate_harness_policy(
    root: Path,
    execution_entrypoints: set[str] | None = None,
    route_commands: set[str] | None = None,
) -> tuple[int, int]:
    """Validate semantics, graph, inventory, closure, and reviewed bytes."""

    policy = load_policy(root)
    entries = _verify_schema(policy)
    if execution_entrypoints is not None and execution_entrypoints != {
        command.entrypoint for command in policy.execution_commands
    }:
        raise GateError("workflow execution commands differ from harness policy")
    if route_commands is not None and route_commands != set(policy.route_entrypoints):
        raise GateError(
            "workflow repo-local commands differ from reviewed harnesses; "
            f"missing={sorted(set(policy.route_entrypoints) - route_commands)!r}, "
            f"unexpected={sorted(route_commands - set(policy.route_entrypoints))!r}"
        )
    all_paths = sorted(
        source for entry in policy.entries for source in entry.sources
    )
    raw_sources, source_identities = read_declared_sources(root, all_paths)
    owners = {
        source: entry.entrypoint
        for entry in policy.entries
        for source in entry.sources
    }
    graph_paths = set(owners) | {
        edge.path
        for entry in policy.entries
        for edge in (*entry.dynamic_sources, *entry.delegates)
    }
    for entry in policy.entries:
        _verify_entry(root, entry, owners, raw_sources, graph_paths)
    verify_declared_sources(root, raw_sources, source_identities)
    return len(entries), len(raw_sources)


def required_harness_source_paths(root: Path | None = None) -> tuple[str, ...]:
    """Return exact reviewed sources for public mutation canaries."""

    repository = (
        root
        if root is not None
        else Path(__file__).resolve().parents[2]
    )
    policy = load_policy(repository)
    return tuple(
        sorted(source for entry in policy.entries for source in entry.sources)
    )


def copy_policy_paths() -> tuple[str, ...]:
    """Return control paths needed by an external-root public canary."""

    return (POLICY_RELATIVE,)


def validate_execution_commands(
    root: Path,
    parsed: dict[tuple[str, str], tuple[CommandEdge, ...]],
    execution_edges: tuple[CommandEdge, ...],
) -> tuple[set[str], set[str]]:
    """Bind exact structured workflow executable/argv edges to policy."""

    policy = load_policy(root)
    expected = Counter(
        CommandEdge(
            command.entrypoint,
            command.executable,
            command.argv,
        )
        for command in policy.execution_commands
        for _unused in range(command.count)
    )
    actual = Counter(execution_edges)
    if actual != expected:
        raise GateError(
            "capability structured execution command/argv edges differ from "
            f"policy; missing={sorted((expected - actual).elements())!r}, "
            f"unexpected={sorted((actual - expected).elements())!r}"
        )
    route_commands = {
        edge.entrypoint
        for edges in parsed.values()
        for edge in edges
    }
    return {edge.entrypoint for edge in execution_edges}, route_commands
