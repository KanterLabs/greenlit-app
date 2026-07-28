#!/usr/bin/env python3
"""Closed member inventory and extraction for release transfer bundles."""

from __future__ import annotations

import os
import re
import tarfile
from pathlib import Path

from release_check_credential_bundle_io import (
    MAX_FILE_BYTES,
    BundleError,
)


CRATE = re.compile(r"greenlit-[a-z-]+-0\.1\.0\.crate")
ROLES = ("oracle", "github-actions", "greenlit-release")


def expected_names(
    kind: str,
    members: tuple[tarfile.TarInfo, ...],
) -> set[str]:
    """Return the one complete visible member inventory for a bundle kind."""

    names = {member.name for member in members}
    if kind == "prepared":
        crates = {
            name
            for name in names
            if name.startswith("candidate/target/package/")
        }
        if len(crates) != 8 or any(
            CRATE.fullmatch(name.rsplit("/", 1)[-1]) is None for name in crates
        ):
            raise BundleError("prepared transfer bundle has invalid crate closure")
        return {
            "source-commit",
            "candidate",
            "candidate/target",
            "candidate/target/package",
            "candidate/target/release",
            "candidate/target/release/litci",
            *crates,
        }
    roles = (
        ("oracle", "greenlit-release")
        if kind == "local"
        else ("github-actions",)
        if kind == "github"
        else ROLES
    )
    expected = {
        "source-commit",
        "parity",
        "parity/captures",
        *(f"parity/seed-{role}.json" for role in roles),
        *(f"parity/captures/shell-only-seed-{role}.json" for role in roles),
    }
    if kind == "local":
        expected.update(("binary", "binary/litci"))
    return expected


def extract_member(
    archive: tarfile.TarFile,
    member: tarfile.TarInfo,
    output: Path,
) -> None:
    """Extract one already inventoried member without links or unsafe metadata."""

    if (
        member.pax_headers
        or member.uid != 0
        or member.gid != 0
        or member.uname
        or member.gname
        or member.mtime != 0
        or member.name.startswith("/")
        or ".." in Path(member.name).parts
    ):
        raise BundleError("release transfer member metadata is invalid")
    destination = output / member.name
    if member.isdir():
        expected_mode = 0o700 if member.name.startswith("parity") else 0o755
        if member.mode != expected_mode:
            raise BundleError("release transfer directory mode is invalid")
        destination.mkdir(parents=True, exist_ok=False, mode=expected_mode)
        destination.chmod(expected_mode)
        return
    expected_mode = (
        0o755
        if member.name in ("candidate/target/release/litci", "binary/litci")
        else 0o600
        if member.name == "source-commit" or member.name.startswith("parity/")
        else 0o644
    )
    if (
        not member.isfile()
        or member.mode != expected_mode
        or member.size > MAX_FILE_BYTES
    ):
        raise BundleError("release transfer file metadata is invalid")
    destination.parent.mkdir(parents=True, exist_ok=True)
    source_stream = archive.extractfile(member)
    if source_stream is None:
        raise BundleError("release transfer file content is missing")
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC
    flags |= getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(destination, flags, expected_mode)
    with os.fdopen(descriptor, "wb") as target:
        os.fchmod(descriptor, expected_mode)
        remaining = member.size
        while remaining:
            chunk = source_stream.read(min(1024 * 1024, remaining))
            if not chunk:
                raise BundleError("release transfer file is truncated")
            target.write(chunk)
            remaining -= len(chunk)
        if source_stream.read(1):
            raise BundleError("release transfer file exceeds declared size")
