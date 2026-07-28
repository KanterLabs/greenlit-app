"""Shared locked-Cargo command authority for exact test manifests."""

from __future__ import annotations

import json
import os
import subprocess
import sys
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Any


sys.dont_write_bytecode = True


class GateError(Exception):
    """A concise manifest or Cargo command failure."""


@dataclass(frozen=True)
class CargoTarget:
    """The target fields that select one Cargo test command."""

    package: str
    name: str
    kinds: tuple[str, ...]
    required_features: tuple[str, ...]
    source: Path
    test: bool
    doctest: bool
    feature_definitions: tuple[tuple[str, tuple[str, ...]], ...]


def unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    """Reject duplicate JSON keys instead of accepting the last value."""

    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise GateError(f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def cargo_metadata(root: Path) -> list[CargoTarget]:
    """Return only locked workspace targets from Cargo metadata."""

    command = [
        "cargo",
        "metadata",
        "--locked",
        "--no-deps",
        "--format-version",
        "1",
    ]
    completed = run(command, root=root, capture=True)
    try:
        metadata = json.loads(completed.stdout, object_pairs_hook=unique_object)
    except (json.JSONDecodeError, GateError) as error:
        raise GateError(f"Cargo emitted invalid metadata JSON: {error}") from error
    if not isinstance(metadata, dict):
        raise GateError("Cargo metadata root is not an object")
    packages = metadata.get("packages")
    members = metadata.get("workspace_members")
    if (
        not isinstance(packages, list)
        or not isinstance(members, list)
        or not all(isinstance(member, str) for member in members)
    ):
        raise GateError("Cargo metadata omitted its workspace package arrays")
    member_ids = set(members)
    result: list[CargoTarget] = []
    packages_seen: set[str] = set()
    for package in packages:
        if not isinstance(package, dict) or package.get("id") not in member_ids:
            continue
        package_name = package.get("name")
        targets = package.get("targets")
        feature_definitions = package.get("features")
        if (
            not isinstance(package_name, str)
            or not package_name
            or not isinstance(targets, list)
            or not isinstance(feature_definitions, dict)
        ):
            raise GateError("Cargo metadata contains a malformed workspace package")
        if package_name in packages_seen:
            raise GateError(f"Cargo metadata repeats package {package_name!r}")
        packages_seen.add(package_name)
        parsed_features: list[tuple[str, tuple[str, ...]]] = []
        for feature, members in feature_definitions.items():
            if (
                not isinstance(feature, str)
                or not feature
                or not isinstance(members, list)
                or not all(isinstance(member, str) and member for member in members)
            ):
                raise GateError(
                    f"Cargo metadata contains malformed feature data in {package_name}"
                )
            parsed_features.append((feature, tuple(sorted(members))))
        parsed_features.sort()
        for target in targets:
            if not isinstance(target, dict):
                raise GateError(
                    f"Cargo metadata contains a malformed target in {package_name}"
                )
            name = target.get("name")
            kinds = target.get("kind")
            required = target.get("required-features") or []
            source = target.get("src_path")
            test = target.get("test")
            doctest = target.get("doctest")
            if (
                not isinstance(name, str)
                or not name
                or not isinstance(kinds, list)
                or not kinds
                or not all(isinstance(kind, str) and kind for kind in kinds)
                or not isinstance(required, list)
                or not all(isinstance(feature, str) and feature for feature in required)
                or not isinstance(source, str)
                or not isinstance(test, bool)
                or not isinstance(doctest, bool)
            ):
                raise GateError(
                    f"Cargo metadata contains malformed target data in {package_name}"
                )
            source_path = Path(source)
            if source_path.is_symlink() or not source_path.is_file():
                raise GateError(
                    f"Cargo target source is not a regular non-symlink file: {source_path}"
                )
            result.append(
                CargoTarget(
                    package=package_name,
                    name=name,
                    kinds=tuple(sorted(kinds)),
                    required_features=tuple(sorted(required)),
                    source=source_path.resolve(),
                    test=test,
                    doctest=doctest,
                    feature_definitions=tuple(parsed_features),
                )
            )
    if not result:
        raise GateError("Cargo metadata declares no workspace targets")
    return result


def selector_kind(target: CargoTarget) -> str:
    """Map one ordinary Cargo target to its unambiguous selector."""

    for kind in ("test", "lib", "bin", "example"):
        if kind in target.kinds:
            return kind
    raise GateError(
        f"{target.package}/{target.name}: unsupported test target kinds {target.kinds!r}"
    )


def target_command(
    package: str,
    target: str,
    selector: str,
    features: tuple[str, ...],
    *,
    cargo_profile: str = "test",
    list_only: bool,
    test_threads: int | None = None,
) -> list[str]:
    """Build one whole-target Cargo test command with no name filter."""

    if cargo_profile not in {"release", "test"}:
        raise GateError(
            f"{package}/{target}: unknown Cargo profile {cargo_profile!r}"
        )
    if test_threads is not None and test_threads < 1:
        raise GateError(
            f"{package}/{target}: test thread count must be positive"
        )
    command = [
        "cargo",
        "test",
        "--locked",
        "--no-default-features",
        "-p",
        package,
    ]
    if cargo_profile == "release":
        command.append("--release")
    if features:
        command.extend(["--features", ",".join(features)])
    if selector == "doc":
        command.append("--doc")
    elif selector == "lib":
        command.append("--lib")
    elif selector in {"bin", "test", "example"}:
        command.extend([f"--{selector}", target])
    else:
        raise GateError(f"{package}/{target}: unknown selector {selector!r}")
    command.append("--")
    if list_only:
        command.extend(["--list", "--format", "terse"])
    else:
        command.append("--nocapture")
        if test_threads is not None:
            command.append(f"--test-threads={test_threads}")
    return command


def _release_environment_entry_is_forbidden(name: str, value: str) -> bool:
    """Identify compiler and Cargo profile inputs that can change release code."""

    if name in {"CARGO_ENCODED_RUSTFLAGS", "RUSTFLAGS"}:
        return value != ""
    if name == "CARGO_INCREMENTAL":
        return value != "0"
    if name == "CARGO_BUILD_INCREMENTAL":
        return value != "false"
    return (
        name
        in {
            "RUSTC",
            "RUSTUP_TOOLCHAIN",
        }
        or name.startswith("RUSTC_")
        or name in {"CARGO_BUILD_RUSTFLAGS", "CARGO_BUILD_TARGET"}
        or name.startswith("CARGO_BUILD_RUSTC")
        or name.startswith("CARGO_PROFILE_")
        or (name.startswith("CARGO_TARGET_") and name != "CARGO_TARGET_DIR")
    )


def validate_release_profile_environment(
    environment: Mapping[str, str] | None = None,
) -> None:
    """Reject ambient inputs that can change release-profile compiler semantics."""

    source = os.environ if environment is None else environment
    forbidden = sorted(
        name
        for name, value in source.items()
        if _release_environment_entry_is_forbidden(name, value)
    )
    if forbidden:
        raise GateError(
            "release-profile Cargo environment contains forbidden compiler/profile "
            f"customization: {', '.join(forbidden)}"
        )


def test_environment(
    test_cfgs: tuple[str, ...],
    *,
    cargo_profile: str = "test",
) -> dict[str, str]:
    """Build the profile-specific Cargo environment boundary."""

    environment = os.environ.copy()
    if cargo_profile == "release":
        validate_release_profile_environment(environment)
        if test_cfgs:
            raise GateError(
                "release-profile Cargo commands cannot inject test-only cfgs"
            )
        return environment
    if cargo_profile != "test":
        raise GateError(f"unknown Cargo profile {cargo_profile!r}")
    if not test_cfgs:
        return environment
    additions = [part for cfg in test_cfgs for part in ("--cfg", cfg)]
    if "CARGO_ENCODED_RUSTFLAGS" in environment:
        encoded = environment["CARGO_ENCODED_RUSTFLAGS"]
        parts = ([encoded] if encoded else []) + additions
        environment["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(parts)
    else:
        existing = environment.get("RUSTFLAGS", "").strip()
        suffix = " ".join(additions)
        environment["RUSTFLAGS"] = f"{existing} {suffix}".strip()
    return environment


def run(
    command: list[str],
    *,
    root: Path,
    capture: bool,
    test_cfgs: tuple[str, ...] = (),
    cargo_profile: str = "test",
) -> subprocess.CompletedProcess[str]:
    """Run one command and turn every nonzero status into a gate failure."""

    try:
        completed = subprocess.run(
            command,
            cwd=root,
            env=test_environment(test_cfgs, cargo_profile=cargo_profile),
            check=False,
            capture_output=capture,
            text=True,
        )
    except OSError as error:
        raise GateError(f"could not execute {' '.join(command)}: {error}") from error
    if completed.returncode != 0:
        details = ""
        if capture:
            details = "\n".join(
                part
                for part in (completed.stdout.strip(), completed.stderr.strip())
                if part
            )
        suffix = f"\n{details}" if details else ""
        raise GateError(
            f"{' '.join(command)} exited {completed.returncode}{suffix}"
        )
    return completed


def listed_tests(
    root: Path,
    *,
    package: str,
    target: str,
    selector: str,
    features: tuple[str, ...],
    test_cfgs: tuple[str, ...],
    cargo_profile: str = "test",
) -> list[str]:
    """List exact Rust test identities for one whole target."""

    command = target_command(
        package,
        target,
        selector,
        features,
        cargo_profile=cargo_profile,
        list_only=True,
    )
    output = run(
        command,
        root=root,
        capture=True,
        test_cfgs=test_cfgs,
        cargo_profile=cargo_profile,
    ).stdout
    suffix = ": test"
    names = [
        line.strip()[: -len(suffix)]
        for line in output.splitlines()
        if line.strip().endswith(suffix)
    ]
    if len(names) != len(set(names)):
        raise GateError(f"{package}/{target}: Cargo listed duplicate tests")
    return sorted(names)


def run_target(
    root: Path,
    *,
    package: str,
    target: str,
    selector: str,
    features: tuple[str, ...],
    test_cfgs: tuple[str, ...],
    cargo_profile: str = "test",
    test_threads: int | None = None,
) -> None:
    """Execute one complete target after its exact list was checked."""

    command = target_command(
        package,
        target,
        selector,
        features,
        cargo_profile=cargo_profile,
        list_only=False,
        test_threads=test_threads,
    )
    run(
        command,
        root=root,
        capture=False,
        test_cfgs=test_cfgs,
        cargo_profile=cargo_profile,
    )
