"""Strict schema for the external non-Cargo harness policy."""

from __future__ import annotations

import json
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .model import GateError
from .noncargo_sources import canonical_path, read_regular


POLICY_RELATIVE = "tools/test_authority/noncargo-policy.json"
SHA256 = re.compile(r"^[0-9a-f]{64}$")
ENTRY_KEYS = {
    "authority_imports",
    "closure_sha256",
    "delegates",
    "dynamic_sources",
    "entrypoint",
    "import_roots",
    "import_targets",
    "inventory_roots",
    "language",
    "sources",
}
TOP_KEYS = {
    "entries",
    "execution_commands",
    "route_entrypoints",
    "schema_version",
}


@dataclass(frozen=True)
class DynamicSource:
    """One non-imported source reached by an exact path reference."""

    path: str
    source: str
    count: int


@dataclass(frozen=True)
class Delegate:
    """One exact local executable or independently governed gate edge."""

    source: str
    path: str
    count: int
    target: str | None
    authority: str | None


@dataclass(frozen=True)
class Entry:
    """One reviewed harness entry point and its exact implementation closure."""

    entrypoint: str
    language: str
    import_roots: tuple[str, ...]
    inventory_roots: tuple[str, ...]
    sources: tuple[str, ...]
    closure_sha256: str
    dynamic_sources: tuple[DynamicSource, ...]
    delegates: tuple[Delegate, ...]
    import_targets: tuple[str, ...]
    authority_imports: tuple[str, ...]


@dataclass(frozen=True)
class Policy:
    """Complete fixed non-Cargo harness authority."""

    entries: tuple[Entry, ...]
    execution_commands: tuple[ExecutionCommand, ...]
    route_entrypoints: tuple[str, ...]


@dataclass(frozen=True, order=True)
class ExecutionCommand:
    """One exact executable/argv edge required from governed execution steps."""

    entrypoint: str
    executable: str
    argv: tuple[str, ...]
    count: int


def _unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise GateError(f"non-Cargo policy repeats JSON key {key!r}")
        result[key] = value
    return result


def _text(value: Any, location: str) -> str:
    if not isinstance(value, str) or not value or value.strip() != value:
        raise GateError(f"{location} must be nonempty trimmed text")
    return value


def _text_array(
    value: Any,
    location: str,
    *,
    paths: bool = False,
    sorted_values: bool = True,
    unique: bool = True,
) -> tuple[str, ...]:
    if not isinstance(value, list):
        raise GateError(f"{location} must be an array")
    items = tuple(
        canonical_path(item, f"{location}[{index}]")
        if paths
        else _text(item, f"{location}[{index}]")
        for index, item in enumerate(value)
    )
    if unique and len(items) != len(set(items)):
        raise GateError(f"{location} must be unique")
    if sorted_values and items != tuple(sorted(items)):
        raise GateError(f"{location} must be sorted")
    return items


def _positive_count(value: Any, location: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < 1:
        raise GateError(f"{location} must be a positive integer")
    return value


def _dynamic(value: Any, location: str) -> DynamicSource:
    keys = {"count", "path", "source"}
    if not isinstance(value, dict) or set(value) != keys:
        raise GateError(f"{location} must contain exactly {sorted(keys)!r}")
    return DynamicSource(
        path=canonical_path(value["path"], f"{location}.path"),
        source=canonical_path(value["source"], f"{location}.source"),
        count=_positive_count(value["count"], f"{location}.count"),
    )


def _delegate(value: Any, location: str) -> Delegate:
    if not isinstance(value, dict):
        raise GateError(f"{location} must be an object")
    common = {"count", "path", "source"}
    variant = set(value) - common
    if common - set(value) or variant not in ({"target"}, {"authority"}):
        raise GateError(f"{location} must declare exactly one target or authority")
    target = value.get("target")
    authority = value.get("authority")
    if ("target" in value and target is None) or (
        "authority" in value and authority is None
    ):
        raise GateError(f"{location} target/authority variant must not be null")
    return Delegate(
        source=canonical_path(value["source"], f"{location}.source"),
        path=canonical_path(value["path"], f"{location}.path"),
        count=_positive_count(value["count"], f"{location}.count"),
        target=(
            canonical_path(target, f"{location}.target")
            if target is not None
            else None
        ),
        authority=(
            _text(authority, f"{location}.authority")
            if authority is not None
            else None
        ),
    )


def _entry(value: Any, location: str) -> Entry:
    if not isinstance(value, dict) or set(value) != ENTRY_KEYS:
        raise GateError(f"{location} must contain exactly {sorted(ENTRY_KEYS)!r}")
    language = _text(value["language"], f"{location}.language")
    if language not in {"python", "shell"}:
        raise GateError(f"{location}.language must be python or shell")
    sources = _text_array(value["sources"], f"{location}.sources", paths=True)
    digest = _text(value["closure_sha256"], f"{location}.closure_sha256")
    if SHA256.fullmatch(digest) is None:
        raise GateError(f"{location}.closure_sha256 must be lowercase SHA-256")
    dynamic_values = value["dynamic_sources"]
    delegate_values = value["delegates"]
    if not isinstance(dynamic_values, list) or not isinstance(delegate_values, list):
        raise GateError(f"{location} dynamic sources and delegates must be arrays")
    return Entry(
        entrypoint=canonical_path(value["entrypoint"], f"{location}.entrypoint"),
        language=language,
        import_roots=_text_array(
            value["import_roots"],
            f"{location}.import_roots",
            paths=True,
            sorted_values=False,
        ),
        inventory_roots=_text_array(
            value["inventory_roots"], f"{location}.inventory_roots", paths=True
        ),
        sources=sources,
        closure_sha256=digest,
        dynamic_sources=tuple(
            _dynamic(item, f"{location}.dynamic_sources[{index}]")
            for index, item in enumerate(dynamic_values)
        ),
        delegates=tuple(
            _delegate(item, f"{location}.delegates[{index}]")
            for index, item in enumerate(delegate_values)
        ),
        import_targets=_text_array(
            value["import_targets"], f"{location}.import_targets", paths=True
        ),
        authority_imports=_text_array(
            value["authority_imports"], f"{location}.authority_imports"
        ),
    )


def load_policy(root: Path) -> Policy:
    """Load the strict external digest and graph authority."""

    raw = read_regular(root, POLICY_RELATIVE, limit=512 * 1024)
    try:
        value = json.loads(raw, object_pairs_hook=_unique_object)
    except (UnicodeError, json.JSONDecodeError) as error:
        raise GateError(f"{root / POLICY_RELATIVE}: invalid policy JSON: {error}") from error
    if not isinstance(value, dict) or set(value) != TOP_KEYS:
        raise GateError(
            f"{root / POLICY_RELATIVE}: top level must contain exactly "
            f"{sorted(TOP_KEYS)!r}"
        )
    if (
        not isinstance(value["schema_version"], int)
        or isinstance(value["schema_version"], bool)
        or value["schema_version"] != 1
    ):
        raise GateError(f"{root / POLICY_RELATIVE}: schema_version must be 1")
    raw_entries = value["entries"]
    raw_commands = value["execution_commands"]
    if not isinstance(raw_entries, list) or not raw_entries:
        raise GateError(f"{root / POLICY_RELATIVE}: entries must be nonempty")
    if not isinstance(raw_commands, list) or not raw_commands:
        raise GateError(
            f"{root / POLICY_RELATIVE}: execution_commands must be nonempty"
        )
    entries = tuple(
        _entry(item, f"{root / POLICY_RELATIVE}: entries[{index}]")
        for index, item in enumerate(raw_entries)
    )
    if tuple(item.entrypoint for item in entries) != tuple(
        sorted(item.entrypoint for item in entries)
    ):
        raise GateError(f"{root / POLICY_RELATIVE}: entries must be sorted")
    commands: list[ExecutionCommand] = []
    for index, item in enumerate(raw_commands):
        location = f"{root / POLICY_RELATIVE}: execution_commands[{index}]"
        keys = {"argv", "count", "entrypoint", "executable"}
        if not isinstance(item, dict) or set(item) != keys:
            raise GateError(f"{location} must contain exactly {sorted(keys)!r}")
        commands.append(
            ExecutionCommand(
                entrypoint=canonical_path(
                    item["entrypoint"],
                    f"{location}.entrypoint",
                ),
                executable=_text(item["executable"], f"{location}.executable"),
                argv=_text_array(
                    item["argv"],
                    f"{location}.argv",
                    sorted_values=False,
                    unique=False,
                ),
                count=_positive_count(item["count"], f"{location}.count"),
            )
        )
    if tuple(commands) != tuple(sorted(commands)):
        raise GateError(f"{root / POLICY_RELATIVE}: execution commands must be sorted")
    return Policy(
        entries=entries,
        execution_commands=tuple(commands),
        route_entrypoints=_text_array(
            value["route_entrypoints"],
            f"{root / POLICY_RELATIVE}: route_entrypoints",
            paths=True,
        ),
    )
