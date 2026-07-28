"""Exact portable inventory comparison and execution."""

from __future__ import annotations

from pathlib import Path
from typing import Any

from cargo_test_manifest import (
    GateError,
    cargo_metadata,
    listed_tests,
    run_target,
)

from .model import Candidate, load_manifest, portable_candidates


Inventory = dict[tuple[str, str, str], tuple[Candidate, list[str]]]


def actual_inventory(root: Path, candidates: list[Candidate]) -> Inventory:
    """Probe every candidate and retain exactly the nonzero harnesses."""

    inventory: Inventory = {}
    for candidate in candidates:
        tests = listed_tests(
            root,
            package=candidate.package,
            target=candidate.target,
            selector=candidate.selector,
            features=candidate.features,
            test_cfgs=candidate.test_cfgs,
        )
        if tests:
            inventory[candidate.identity] = (candidate, tests)
    if not inventory:
        raise GateError("portable Cargo commands selected zero tests")
    return inventory


def validate_inventory(entries: list[dict[str, Any]], inventory: Inventory) -> int:
    """Require exact target and test-name equality."""

    manifest = {
        (entry["package"], entry["selector"], entry["target"]): entry
        for entry in entries
    }
    actual_targets = set(inventory)
    manifest_targets = set(manifest)
    if actual_targets != manifest_targets:
        raise GateError(
            "portable nonzero targets differ from the manifest; "
            f"missing={sorted(actual_targets - manifest_targets)!r}, "
            f"unexpected={sorted(manifest_targets - actual_targets)!r}"
        )
    selected = 0
    for identity in sorted(actual_targets):
        candidate, actual = inventory[identity]
        entry = manifest[identity]
        if tuple(entry["features"]) != candidate.features:
            raise GateError(f"{identity!r}: portable feature selection changed")
        if tuple(entry["test_cfgs"]) != candidate.test_cfgs:
            raise GateError(f"{identity!r}: portable test cfg selection changed")
        expected = entry["tests"]
        if actual != expected:
            expected_set = set(expected)
            actual_set = set(actual)
            raise GateError(
                f"{identity!r}: selected tests differ from the manifest; "
                f"missing={sorted(expected_set - actual_set)!r}, "
                f"unexpected={sorted(actual_set - expected_set)!r}"
            )
        selected += len(actual)
    return selected


def check(root: Path, manifest_path: Path, *, execute: bool) -> tuple[int, int]:
    """Validate the exact inventory and optionally run every whole target."""

    entries = load_manifest(manifest_path)
    candidates = portable_candidates(cargo_metadata(root))
    inventory = actual_inventory(root, candidates)
    selected = validate_inventory(entries, inventory)
    if execute:
        manifest = {
            (entry["package"], entry["selector"], entry["target"]): entry
            for entry in entries
        }
        for identity in sorted(inventory):
            entry = manifest[identity]
            run_target(
                root,
                package=entry["package"],
                target=entry["target"],
                selector=entry["selector"],
                features=tuple(entry["features"]),
                test_cfgs=tuple(entry["test_cfgs"]),
            )
    return len(entries), selected
