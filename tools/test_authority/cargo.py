"""Locked Cargo metadata authority for Rust source ownership."""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

from .model import GateError, TargetSource
from .rust_source import module_sources, resolve_source


def cargo_targets(root: Path) -> tuple[list[TargetSource], Path]:
    """Load the workspace target graph from Cargo's locked public metadata."""

    command = [
        "cargo",
        "metadata",
        "--locked",
        "--no-deps",
        "--format-version",
        "1",
    ]
    try:
        completed = subprocess.run(
            command,
            cwd=root,
            check=False,
            capture_output=True,
            text=True,
        )
    except OSError as error:
        raise GateError(f"could not execute {' '.join(command)}: {error}") from error
    if completed.returncode != 0:
        details = "\n".join(
            part
            for part in (completed.stdout.strip(), completed.stderr.strip())
            if part
        )
        suffix = f"\n{details}" if details else ""
        raise GateError(f"Cargo metadata exited {completed.returncode}{suffix}")
    try:
        metadata = json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise GateError(f"Cargo emitted invalid metadata JSON: {error}") from error
    if not isinstance(metadata, dict):
        raise GateError("Cargo metadata root is not an object")
    packages = metadata.get("packages")
    members = metadata.get("workspace_members")
    target_directory = metadata.get("target_directory")
    if (
        not isinstance(packages, list)
        or not isinstance(members, list)
        or not all(isinstance(member, str) for member in members)
        or not isinstance(target_directory, str)
    ):
        raise GateError("Cargo metadata omitted workspace package/target data")
    member_ids = set(members)
    targets: list[TargetSource] = []
    package_names: set[str] = set()
    for package in packages:
        if not isinstance(package, dict) or package.get("id") not in member_ids:
            continue
        package_name = package.get("name")
        manifest_path = package.get("manifest_path")
        package_targets = package.get("targets")
        if (
            not isinstance(package_name, str)
            or not package_name
            or not isinstance(manifest_path, str)
            or not isinstance(package_targets, list)
        ):
            raise GateError("Cargo metadata contains a malformed workspace package")
        if package_name in package_names:
            raise GateError(f"Cargo metadata repeats package name {package_name!r}")
        package_names.add(package_name)
        package_root = resolve_source(Path(manifest_path)).parent
        targets.extend(
            package_target_sources(package_name, package_root, package_targets)
        )
    if not targets:
        raise GateError("Cargo metadata contains no workspace targets")
    return targets, Path(target_directory).resolve()


def package_target_sources(
    package: str,
    package_root: Path,
    targets: list[object],
) -> list[TargetSource]:
    """Validate and convert one package's Cargo target array."""

    result: list[TargetSource] = []
    for target in targets:
        if not isinstance(target, dict):
            raise GateError(f"Cargo metadata contains a malformed {package} target")
        name = target.get("name")
        kinds = target.get("kind")
        source = target.get("src_path")
        required = target.get("required-features") or []
        if (
            not isinstance(name, str)
            or not name
            or not isinstance(kinds, list)
            or not kinds
            or not all(isinstance(kind, str) and kind for kind in kinds)
            or not isinstance(source, str)
            or not isinstance(required, list)
            or not all(isinstance(feature, str) and feature for feature in required)
        ):
            raise GateError(f"Cargo metadata contains malformed target data in {package}")
        result.append(
            TargetSource(
                package=package,
                package_root=package_root,
                name=name,
                kinds=tuple(sorted(kinds)),
                source=resolve_source(Path(source)),
                required_features=tuple(sorted(required)),
            )
        )
    return result


def package_rust_files(
    targets: list[TargetSource],
    target_directory: Path,
) -> dict[Path, set[str]]:
    """Find package-local Rust files from metadata-derived package roots."""

    files: dict[Path, set[str]] = {}
    roots = sorted({(target.package, target.package_root) for target in targets})
    for package, package_root in roots:
        for path in sorted(package_root.rglob("*.rs")):
            if path.is_symlink():
                raise GateError(f"{path}: Rust source must not be a symbolic link")
            try:
                path.relative_to(target_directory)
            except ValueError:
                pass
            else:
                continue
            resolved = resolve_source(path)
            files.setdefault(resolved, set()).add(package)
    return files


def target_source_sets(
    targets: list[TargetSource],
) -> tuple[dict[Path, list[TargetSource]], dict[Path, list[TargetSource]]]:
    """Map every target module source to its production or test target."""

    production: dict[Path, list[TargetSource]] = {}
    tests: dict[Path, list[TargetSource]] = {}
    for target in targets:
        destination = tests if target.is_test_code else production
        for path in module_sources(target.source):
            destination.setdefault(path, []).append(target)
    return production, tests
