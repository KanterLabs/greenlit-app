#!/usr/bin/env python3
"""Descriptor-bound input inventory for release transfer bundles."""

from __future__ import annotations

from contextlib import ExitStack
import os
from pathlib import Path
from typing import BinaryIO

from release_check_credential_bundle_extract import CRATE
from release_check_credential_bundle_io import (
    MAX_FILE_BYTES,
    BoundDirectory,
    BundleError,
    child_directory,
    child_reader,
    directory_reader,
    reader,
)


ROLES = ("oracle", "github-actions", "greenlit-release")
OpenedInput = tuple[str, BinaryIO, os.stat_result, int]


def _enter_directory(
    stack: ExitStack,
    parent: BoundDirectory,
    name: str,
    mode: int,
    expected: set[str],
) -> BoundDirectory:
    directory = stack.enter_context(child_directory(parent, name, mode))
    if directory.names != expected:
        raise BundleError(f"{directory.label} does not have exact closure")
    return directory


def _enter_file(
    stack: ExitStack,
    directory: BoundDirectory,
    archive_name: str,
    file_name: str,
    mode: int,
) -> OpenedInput:
    stream, metadata = stack.enter_context(
        child_reader(directory, file_name, mode, MAX_FILE_BYTES)
    )
    return archive_name, stream, metadata, mode


def _prepared_inputs(
    root: Path,
    stack: ExitStack,
    source: str,
    *,
    unpacked: bool,
) -> tuple[tuple[tuple[str, int], ...], tuple[OpenedInput, ...]]:
    top = stack.enter_context(directory_reader(root, 0o700))
    expected_top = {"candidate", "source-commit"} if unpacked else {"candidate"}
    if top.names != expected_top:
        raise BundleError("prepared transfer root does not have exact closure")
    if unpacked:
        marker, _ = stack.enter_context(
            child_reader(top, "source-commit", 0o600, 41)
        )
        if marker.read(42) != f"{source}\n".encode("ascii"):
            raise BundleError("prepared transfer source identity does not match")
        marker.seek(0)
    candidate = _enter_directory(stack, top, "candidate", 0o755, {"target"})
    target = _enter_directory(
        stack,
        candidate,
        "target",
        0o755,
        {"release", "package"},
    )
    release = _enter_directory(stack, target, "release", 0o755, {"litci"})
    package = stack.enter_context(child_directory(target, "package", 0o755))
    if len(package.names) != 8 or any(
        CRATE.fullmatch(name) is None for name in package.names
    ):
        raise BundleError("prepared package directory must contain exactly eight crates")
    files = [
        _enter_file(
            stack,
            release,
            "candidate/target/release/litci",
            "litci",
            0o755,
        )
    ]
    for name in sorted(package.names):
        files.append(
            _enter_file(
                stack,
                package,
                f"candidate/target/package/{name}",
                name,
                0o644,
            )
        )
    directories = (
        ("candidate", 0o755),
        ("candidate/target", 0o755),
        ("candidate/target/package", 0o755),
        ("candidate/target/release", 0o755),
    )
    return directories, tuple(files)


def _parity_inputs(
    root: Path,
    stack: ExitStack,
    roles: tuple[str, ...],
) -> tuple[tuple[tuple[str, int], ...], tuple[OpenedInput, ...]]:
    top = stack.enter_context(directory_reader(root, 0o700))
    expected_top = {f"seed-{role}.json" for role in roles} | {"captures"}
    if top.names != expected_top:
        raise BundleError("parity transfer root does not have exact closure")
    expected_captures = {f"shell-only-seed-{role}.json" for role in roles}
    captures = _enter_directory(
        stack,
        top,
        "captures",
        0o700,
        expected_captures,
    )
    files: list[OpenedInput] = []
    for role in roles:
        observation = f"seed-{role}.json"
        capture = f"shell-only-seed-{role}.json"
        files.extend(
            (
                _enter_file(
                    stack,
                    top,
                    f"parity/{observation}",
                    observation,
                    0o600,
                ),
                _enter_file(
                    stack,
                    captures,
                    f"parity/captures/{capture}",
                    capture,
                    0o600,
                ),
            )
        )
    return (
        (("parity", 0o700), ("parity/captures", 0o700)),
        tuple(files),
    )


def bind_inputs(
    kind: str,
    root: Path,
    stack: ExitStack,
    source: str,
    binary: Path | None,
    *,
    prepared_unpacked: bool,
) -> tuple[tuple[tuple[str, int], ...], tuple[OpenedInput, ...]]:
    """Bind one exact input closure without reopening a pathname."""

    if kind == "prepared":
        return _prepared_inputs(
            root,
            stack,
            source,
            unpacked=prepared_unpacked,
        )
    roles = (
        ("oracle", "greenlit-release")
        if kind == "local"
        else ("github-actions",)
        if kind == "github"
        else ROLES
    )
    directories, files = _parity_inputs(root, stack, roles)
    if kind != "local":
        return directories, files
    if binary is None:
        raise BundleError("local evidence bundle requires its exact binary")
    stream, metadata = stack.enter_context(reader(binary, 0o755, MAX_FILE_BYTES))
    return (
        (("binary", 0o755), *directories),
        (("binary/litci", stream, metadata, 0o755), *files),
    )
