"""Bounded file and import-closure mechanics for non-Cargo harness authority."""

from __future__ import annotations

import hashlib
import os
import stat
from collections.abc import Iterable
from pathlib import Path, PurePosixPath
from typing import Any

from .model import GateError
from .noncargo_fs import SourceTree, stable_directory_identity


MAX_FILE_BYTES = 2 * 1024 * 1024
MAX_TOTAL_BYTES = 32 * 1024 * 1024
MAX_FILES = 256
MAX_DEPTH = 8
MAX_PATH_BYTES = 64 * 1024


def source_language(relative: str, raw: bytes) -> str:
    """Return the explicit reviewed language of one non-Cargo source."""

    if (
        relative.endswith(".py")
        or raw.startswith(b"#!/usr/bin/env python3\n")
        or raw.startswith(b"#!/usr/bin/python3 -I\n")
    ):
        return "python"
    if raw.startswith((b"#!/usr/bin/env bash\n", b"#!/bin/bash -p\n")):
        return "shell"
    raise GateError(f"{relative}: reviewed source language is not explicit")


def canonical_path(value: Any, location: str) -> str:
    """Validate one canonical repository-relative path under tools."""

    if (
        not isinstance(value, str)
        or "\0" in value
        or any(0xD800 <= ord(character) <= 0xDFFF for character in value)
    ):
        raise GateError(f"{location}: source path must be text")
    path = PurePosixPath(value)
    if (
        path.is_absolute()
        or str(path) != value
        or not path.parts
        or path.parts[0] != "tools"
        or len(path.parts) > MAX_DEPTH
        or any(part in {"", ".", ".."} for part in path.parts)
    ):
        raise GateError(f"{location}: source path must be canonical under tools/")
    return value


def read_regular(root: Path, relative: str, *, limit: int = MAX_FILE_BYTES) -> bytes:
    """Read one bounded regular file without following any path component."""

    with SourceTree(root) as tree:
        return tree.read_regular(relative, limit)


def inventory_python(root: Path, relatives: tuple[str, ...]) -> set[str]:
    """Enumerate bounded package roots and reject bytecode or hidden sources."""

    discovered: set[str] = set()
    entries_seen = 0
    retained_paths = 0

    with SourceTree(root) as tree:

        def walk(relative: str, depth: int) -> None:
            nonlocal entries_seen, retained_paths
            if depth > MAX_DEPTH:
                raise GateError(
                    f"{root / relative}: harness inventory exceeds depth limit"
                )
            descriptor = tree.open_directory(relative)
            before = stable_directory_identity(descriptor)
            try:
                with os.scandir(descriptor) as children:
                    for child in children:
                        name = child.name
                        if any(
                            0xD800 <= ord(character) <= 0xDFFF
                            for character in name
                        ):
                            raise GateError(
                                f"{root / relative}: source name is not UTF-8"
                            )
                        entries_seen += 1
                        child_relative = f"{relative}/{name}"
                        retained_paths += len(child_relative.encode("utf-8"))
                        if (
                            entries_seen > MAX_FILES
                            or retained_paths > MAX_PATH_BYTES
                        ):
                            raise GateError(
                                f"{root / relative}: harness inventory exceeds "
                                "traversal limits"
                            )
                        try:
                            metadata = os.stat(
                                name,
                                dir_fd=descriptor,
                                follow_symlinks=False,
                            )
                        except OSError as error:
                            raise GateError(
                                f"{root / child_relative}: could not inspect "
                                f"harness source: {error}"
                            ) from error
                        if stat.S_ISLNK(metadata.st_mode):
                            raise GateError(
                                f"{root / child_relative}: harness source must "
                                "not be a symlink"
                            )
                        if stat.S_ISDIR(metadata.st_mode):
                            if name == "__pycache__":
                                raise GateError(
                                    f"{root / child_relative}: Python bytecode "
                                    "cache is forbidden in a reviewed harness closure"
                                )
                            walk(child_relative, depth + 1)
                        elif stat.S_ISREG(metadata.st_mode):
                            if name.endswith((".pyc", ".pyo")):
                                raise GateError(
                                    f"{root / child_relative}: compiled Python "
                                    "is forbidden in a reviewed harness closure"
                                )
                            if not name.endswith(".py"):
                                raise GateError(
                                    f"{root / child_relative}: unexpected file "
                                    "in Python source inventory"
                                )
                            discovered.add(child_relative)
                        else:
                            raise GateError(
                                f"{root / child_relative}: special source node "
                                "is forbidden"
                            )
                if stable_directory_identity(descriptor) != before:
                    raise GateError(
                        f"{root / relative}: harness inventory changed during "
                        "streaming traversal"
                    )
            finally:
                os.close(descriptor)

        for relative in relatives:
            walk(relative, len(PurePosixPath(relative).parts))
    return discovered


def reject_python_bytecode(root: Path, relatives: Iterable[str]) -> None:
    """Reject adjacent caches that Python could load instead of reviewed source."""

    directories = {
        str(PurePosixPath(relative).parent)
        for relative in relatives
        if relative.endswith(".py")
    }
    with SourceTree(root) as tree:
        for directory in sorted(directories):
            descriptor = tree.open_directory(directory)
            before = stable_directory_identity(descriptor)
            try:
                with os.scandir(descriptor) as children:
                    for child in children:
                        metadata = os.stat(
                            child.name,
                            dir_fd=descriptor,
                            follow_symlinks=False,
                        )
                        if (
                            child.name == "__pycache__"
                            or child.name.endswith((".pyc", ".pyo", ".so"))
                        ):
                            raise GateError(
                                f"{root / directory / child.name}: Python "
                                "bytecode or compiled extensions are forbidden "
                                "beside reviewed sources"
                            )
                        if stat.S_ISLNK(metadata.st_mode):
                            raise GateError(
                                f"{root / directory / child.name}: local Python "
                                "candidate must not be a symlink"
                            )
                if stable_directory_identity(descriptor) != before:
                    raise GateError(
                        f"{root / directory}: reviewed Python directory changed "
                        "during traversal"
                    )
            finally:
                os.close(descriptor)


def closure_digest(sources: dict[str, bytes]) -> str:
    """Hash exact path, length, and bytes for one sorted source closure."""

    digest = hashlib.sha256()
    for relative, raw in sorted(sources.items()):
        encoded = relative.encode("utf-8")
        digest.update(len(encoded).to_bytes(4, "big"))
        digest.update(encoded)
        digest.update(len(raw).to_bytes(8, "big"))
        digest.update(raw)
    return digest.hexdigest()


def read_declared_sources(
    root: Path,
    paths: Iterable[str],
) -> tuple[dict[str, bytes], dict[str, tuple[int, ...]]]:
    """Read one globally bounded exact source set."""

    result: dict[str, bytes] = {}
    identities: dict[str, tuple[int, ...]] = {}
    total = 0
    retained_paths = 0
    with SourceTree(root) as tree:
        for relative in paths:
            if relative in result:
                raise GateError(
                    f"{root / relative}: reviewed source inventory repeats a path"
                )
            retained_paths += len(relative.encode("utf-8"))
            if len(result) >= MAX_FILES or retained_paths > MAX_PATH_BYTES:
                raise GateError(
                    "reviewed harness policy exceeds source inventory limits"
                )
            raw, identity = tree.read_regular_snapshot(
                relative,
                MAX_FILE_BYTES,
            )
            total += len(raw)
            if total > MAX_TOTAL_BYTES:
                raise GateError(
                    "reviewed harness policy exceeds aggregate byte limit"
                )
            result[relative] = raw
            identities[relative] = identity
    return result, identities


def verify_declared_sources(
    root: Path,
    sources: dict[str, bytes],
    identities: dict[str, tuple[int, ...]],
) -> None:
    """Prove every reviewed source still has its initially reviewed identity."""

    with SourceTree(root) as tree:
        for relative, expected in sources.items():
            raw, identity = tree.read_regular_snapshot(
                relative,
                MAX_FILE_BYTES,
            )
            if raw != expected or identity != identities[relative]:
                raise GateError(
                    f"{root / relative}: reviewed source changed during "
                    "authority validation"
                )
