"""Portable manifest schema and Cargo-target discovery."""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from cargo_test_manifest import (
    CargoTarget,
    GateError,
    selector_kind,
    unique_object,
)


TOP_LEVEL_KEYS = {"schema_version", "targets"}
ENTRY_KEYS = {
    "features",
    "package",
    "selector",
    "target",
    "test_cfgs",
    "tests",
}
SELECTORS = {"bin", "doc", "example", "lib", "test"}
TEST_CFGS = {"litci_test_boundaries"}


@dataclass(frozen=True)
class Candidate:
    """One portable Cargo harness that must be probed even while empty."""

    package: str
    target: str
    selector: str
    features: tuple[str, ...]
    test_cfgs: tuple[str, ...]

    @property
    def identity(self) -> tuple[str, str, str]:
        """Return the exact manifest identity for this Cargo harness."""

        return (self.package, self.selector, self.target)


def text_list(value: Any, location: str, *, nonempty: bool) -> list[str]:
    """Validate one sorted unique string array."""

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
    if result != sorted(result):
        raise GateError(f"{location} must be sorted")
    return result


def load_manifest(path: Path) -> list[dict[str, Any]]:
    """Load the strict exact-case portable manifest."""

    if path.is_symlink() or not path.is_file():
        raise GateError(f"{path}: manifest must be a regular non-symlink file")
    try:
        value = json.loads(
            path.read_text(encoding="utf-8"),
            object_pairs_hook=unique_object,
        )
    except OSError as error:
        raise GateError(f"could not read {path}: {error}") from error
    except json.JSONDecodeError as error:
        raise GateError(f"{path}: invalid JSON: {error}") from error
    if not isinstance(value, dict) or set(value) != TOP_LEVEL_KEYS:
        raise GateError(
            f"{path}: top level must contain exactly {sorted(TOP_LEVEL_KEYS)!r}"
        )
    if value["schema_version"] != 1:
        raise GateError(f"{path}: schema_version must be 1")
    entries = value["targets"]
    if not isinstance(entries, list) or not entries:
        raise GateError(f"{path}: targets must be a nonempty array")
    validate_entries(entries, path)
    return entries


def validate_entries(entries: list[object], path: Path) -> None:
    """Validate ordered identities and every strict target field."""

    identities: set[tuple[str, str, str]] = set()
    previous: tuple[str, str, str] | None = None
    for index, entry in enumerate(entries):
        location = f"{path}: targets[{index}]"
        if not isinstance(entry, dict) or set(entry) != ENTRY_KEYS:
            raise GateError(f"{location} must contain exactly {sorted(ENTRY_KEYS)!r}")
        for field in ("package", "selector", "target"):
            text = entry[field]
            if not isinstance(text, str) or not text or text.strip() != text:
                raise GateError(f"{location}.{field} must be nonempty trimmed text")
        if entry["selector"] not in SELECTORS:
            raise GateError(
                f"{location}.selector must be one of {sorted(SELECTORS)!r}"
            )
        entry["features"] = text_list(
            entry["features"], f"{location}.features", nonempty=False
        )
        if entry["features"]:
            raise GateError(
                f"{location}.features must be empty; feature-gated targets "
                "belong to the capability manifest"
            )
        entry["test_cfgs"] = text_list(
            entry["test_cfgs"], f"{location}.test_cfgs", nonempty=False
        )
        unknown_cfgs = set(entry["test_cfgs"]) - TEST_CFGS
        if unknown_cfgs:
            raise GateError(
                f"{location}.test_cfgs contains unknown cfgs {sorted(unknown_cfgs)!r}"
            )
        expected_cfgs = (
            ["litci_test_boundaries"]
            if entry["package"] == "greenlit-app"
            else []
        )
        if entry["test_cfgs"] != expected_cfgs:
            raise GateError(
                f"{location}.test_cfgs must be {expected_cfgs!r} for "
                f"{entry['package']}"
            )
        entry["tests"] = text_list(
            entry["tests"], f"{location}.tests", nonempty=True
        )
        identity = (entry["package"], entry["selector"], entry["target"])
        if identity in identities:
            raise GateError(f"{location} duplicates target identity {identity!r}")
        if previous is not None and identity < previous:
            raise GateError(
                f"{path}: targets must be sorted by package, selector, and target"
            )
        identities.add(identity)
        previous = identity


def portable_candidates(targets: list[CargoTarget]) -> list[Candidate]:
    """Derive every ordinary, non-feature test and doctest harness from Cargo."""

    candidates: list[Candidate] = []
    identities: set[tuple[str, str, str]] = set()
    for target in targets:
        cfgs = (
            ("litci_test_boundaries",)
            if target.package == "greenlit-app"
            else ()
        )
        if target.test and not target.required_features:
            add_candidate(
                Candidate(
                    package=target.package,
                    target=target.name,
                    selector=selector_kind(target),
                    features=(),
                    test_cfgs=cfgs,
                ),
                candidates,
                identities,
            )
        if target.doctest and not target.required_features:
            add_candidate(
                Candidate(
                    package=target.package,
                    target=target.name,
                    selector="doc",
                    features=(),
                    test_cfgs=cfgs,
                ),
                candidates,
                identities,
            )
    if not candidates:
        raise GateError("Cargo metadata declares no portable test harness candidates")
    return sorted(candidates, key=lambda candidate: candidate.identity)


def add_candidate(
    candidate: Candidate,
    candidates: list[Candidate],
    identities: set[tuple[str, str, str]],
) -> None:
    """Add one Cargo-derived target without accepting duplicate identities."""

    if candidate.identity in identities:
        raise GateError(
            f"Cargo metadata repeats portable target {candidate.identity!r}"
        )
    identities.add(candidate.identity)
    candidates.append(candidate)
