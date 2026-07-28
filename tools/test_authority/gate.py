"""Repository-wide test-authority policy evaluation."""

from __future__ import annotations

import sys
from pathlib import Path

from .cargo import cargo_targets, package_rust_files, target_source_sets
from .model import (
    EARLY_SUCCESS_PATTERN,
    EXPLICIT_RETURN_PATTERN,
    IGNORED_PATTERN,
    INLINE_FEATURE_CFG_PATTERN,
    OFFICIAL_RELEASE_COMMANDS,
    ORACLE_CRATES,
    OWN_TRAIT_PATTERN,
    OWN_TYPE_PATTERN,
    PROCESS_SUCCESS_PATTERN,
    SOURCE_TEST_PATTERN,
    TEST_ATTRIBUTE_PATTERN,
    TEST_BOUNDARY_CFG,
    GateError,
    TargetSource,
    Violation,
)
from .rust_source import read_source, test_bodies


def source_ownership(
    targets: list[TargetSource],
    target_directory: Path,
) -> tuple[
    dict[Path, set[str]],
    dict[Path, list[TargetSource]],
    dict[Path, list[TargetSource]],
]:
    """Build package, production-target, and test-target source maps."""

    production, test_sources = target_source_sets(targets)
    ownership = package_rust_files(targets, target_directory)
    for target in targets:
        ownership.setdefault(target.source, set()).add(target.package)
    for path, associated in production.items():
        ownership.setdefault(path, set()).update(
            target.package for target in associated
        )
    for path, associated in test_sources.items():
        ownership.setdefault(path, set()).update(
            target.package for target in associated
        )
    return ownership, production, test_sources


def collect_violations(root: Path) -> tuple[list[Violation], int]:
    """Collect every deterministic test-authority violation."""

    violations: list[Violation] = []
    targets, target_directory = cargo_targets(root)
    ownership, production, test_sources = source_ownership(
        targets, target_directory
    )
    scrubbed_sources: dict[Path, str] = {}
    for path in sorted(ownership):
        _, scrubbed = read_source(path)
        scrubbed_sources[path] = scrubbed
        packages = ownership[path]
        source_packages = {
            target.package for target in production.get(path, [])
        }
        if not source_packages and not test_sources.get(path):
            source_packages = set(packages)
        non_oracle_source = source_packages - ORACLE_CRATES
        if non_oracle_source:
            match = SOURCE_TEST_PATTERN.search(scrubbed)
            if match is not None:
                violations.append(
                    Violation(
                        path,
                        match.start(),
                        "source-local test code",
                        "non-oracle package tests must live at a Cargo-selected "
                        "integration or invariant boundary",
                    )
                )

        selected_tests = test_sources.get(path, [])
        integration_like = bool(selected_tests) or in_test_directory(
            path, packages, targets
        )
        if not integration_like:
            continue
        match = TEST_ATTRIBUTE_PATTERN.search(scrubbed)
        if not selected_tests and match is not None:
            violations.append(
                Violation(
                    path,
                    match.start(),
                    "unselected test code",
                    "the test is not reachable from any Cargo metadata test target",
                )
            )
        match = INLINE_FEATURE_CFG_PATTERN.search(scrubbed)
        if match is not None:
            violations.append(
                Violation(
                    path,
                    match.start(),
                    "inline feature-gated test",
                    "gate the complete Cargo test target with [[test]] "
                    "required-features so the capability manifest owns it",
                )
            )
        if packages - ORACLE_CRATES:
            match = OWN_TRAIT_PATTERN.search(scrubbed) or OWN_TYPE_PATTERN.search(
                scrubbed
            )
            if match is not None:
                violations.append(
                    Violation(
                        path,
                        match.start(),
                        "own-crate runtime substitute",
                        "mock only a true external boundary, never Greenlit behavior",
                    )
                )
        collect_self_skips(path, scrubbed, selected_tests, violations)

    for path, scrubbed in scrubbed_sources.items():
        match = IGNORED_PATTERN.search(scrubbed)
        if match is not None:
            violations.append(
                Violation(
                    path,
                    match.start(),
                    "ignored test",
                    "ignored tests are forbidden; provision the capability or fail",
                )
            )
    collect_release_cfg_leaks(root, violations)
    violations.sort(key=lambda item: (str(item.path), item.offset, item.category))
    return violations, len(ownership)


def in_test_directory(
    path: Path,
    packages: set[str],
    targets: list[TargetSource],
) -> bool:
    """Return whether a package-owned source sits under tests or benches."""

    roots = {
        target.package_root for target in targets if target.package in packages
    }
    return any(
        path.is_relative_to(package_root / directory)
        for package_root in roots
        for directory in ("tests", "benches")
    )


def collect_self_skips(
    path: Path,
    scrubbed: str,
    selected_tests: list[TargetSource],
    violations: list[Violation],
) -> None:
    """Reject success-return paths in ordinary and capability tests."""

    capability_source = any(target.is_capability_test for target in selected_tests)
    for name, start, body in test_bodies(scrubbed):
        match = (
            EXPLICIT_RETURN_PATTERN.search(body)
            if capability_source
            else EARLY_SUCCESS_PATTERN.search(body)
        )
        if match is None:
            match = PROCESS_SUCCESS_PATTERN.search(body)
        if match is not None:
            violations.append(
                Violation(
                    path,
                    start + match.start(),
                    "capability self-skip",
                    f"test `{name}` contains an explicit success return instead "
                    "of failing when its boundary is absent",
                )
            )


def collect_release_cfg_leaks(root: Path, violations: list[Violation]) -> None:
    """Reject raw test-boundary cfg spelling in official release commands."""

    for relative in OFFICIAL_RELEASE_COMMANDS:
        path = root / relative
        if not path.exists():
            continue
        if path.is_symlink() or not path.is_file():
            raise GateError(f"{path}: release command source must be a regular file")
        try:
            raw = path.read_text(encoding="utf-8")
        except (OSError, UnicodeError) as error:
            raise GateError(
                f"could not read release command source {path}: {error}"
            ) from error
        offset = raw.find(TEST_BOUNDARY_CFG)
        if offset >= 0:
            violations.append(
                Violation(
                    path,
                    offset,
                    "release test cfg",
                    f"raw official release commands must never enable "
                    f"{TEST_BOUNDARY_CFG}; invoke the manifest test runner instead",
                )
            )


def line_number(text: str, offset: int) -> int:
    """Convert a byte-compatible character offset to a one-based line."""

    return text.count("\n", 0, offset) + 1


def display_path(path: Path, root: Path) -> Path:
    """Prefer repository-relative diagnostics while supporting external targets."""

    try:
        return path.relative_to(root)
    except ValueError:
        return path


def check(root: Path) -> int:
    """Run the repository gate and render stable source diagnostics."""

    violations, scanned = collect_violations(root)
    if violations:
        for violation in violations:
            raw = violation.path.read_text(encoding="utf-8")
            relative = display_path(violation.path, root)
            line = line_number(raw, violation.offset)
            print(
                f"{relative}:{line}: {violation.category}: {violation.detail}",
                file=sys.stderr,
            )
        print(
            f"test authority gate failed: {len(violations)} violation(s)",
            file=sys.stderr,
        )
        return 1
    print(f"test authority gate passed: {scanned} Rust source/test files")
    return 0
