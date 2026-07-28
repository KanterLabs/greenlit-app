"""Locked offline Cargo package projection and independent repackaging."""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
from pathlib import Path

from .common import CRATES, ProvenanceError, hash_regular


MAX_CARGO_OUTPUT = 32 * 1024 * 1024


def _cargo_environment() -> dict[str, str]:
    environment = os.environ.copy()
    for key in tuple(environment):
        if key.startswith("GIT_") or key in {
            "RUSTC",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "CARGO_BUILD_TARGET",
        }:
            environment.pop(key, None)
    environment.update(
        {
            "CARGO_NET_OFFLINE": "true",
            "CARGO_TERM_COLOR": "never",
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_NO_REPLACE_OBJECTS": "1",
        }
    )
    return environment


def _cargo() -> str:
    executable = shutil.which("cargo")
    if executable is None:
        raise ProvenanceError("Cargo is unavailable for package provenance")
    return executable


def package_projection(repository: Path, package: str) -> set[str]:
    """Ask locked offline Cargo for the exact canonical package member list."""

    try:
        result = subprocess.run(
            [
                _cargo(),
                "package",
                "--locked",
                "--offline",
                "-p",
                package,
                "--list",
            ],
            cwd=repository,
            env=_cargo_environment(),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=120,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ProvenanceError(f"could not derive package projection: {error}") from error
    if len(result.stdout) > MAX_CARGO_OUTPUT or len(result.stderr) > MAX_CARGO_OUTPUT:
        raise ProvenanceError("Cargo package projection exceeded its output limit")
    if result.returncode != 0:
        detail = result.stderr[:4096].decode("utf-8", errors="replace").strip()
        raise ProvenanceError(
            f"Cargo package projection failed for {package}"
            + (f": {detail}" if detail else "")
        )
    try:
        members = result.stdout.decode("utf-8", errors="strict").splitlines()
    except UnicodeDecodeError as error:
        raise ProvenanceError("Cargo package projection is not UTF-8") from error
    projection: set[str] = set()
    for member in members:
        parts = Path(member).parts
        if (
            not member
            or member.startswith("/")
            or "\\" in member
            or any(part in {"", ".", ".."} for part in parts)
            or member in projection
        ):
            raise ProvenanceError(
                f"Cargo package projection contains unsafe member {member!r}"
            )
        projection.add(member)
    if not {
        ".cargo_vcs_info.json",
        "Cargo.toml",
        "Cargo.toml.orig",
    } < projection:
        raise ProvenanceError(f"Cargo package projection is incomplete for {package}")
    return projection


def rebuilt_package_digests(repository: Path) -> dict[str, str]:
    """Independently repackage exact HEAD and hash all eight canonical crates."""

    temporary_parent = Path(os.environ.get("RUNNER_TEMP", "/tmp"))
    if not temporary_parent.is_dir() or temporary_parent.is_symlink():
        raise ProvenanceError(
            f"independent package temporary parent is unsafe: {temporary_parent}"
        )
    with tempfile.TemporaryDirectory(
        prefix="greenlit-provenance-packages.",
        dir=temporary_parent,
    ) as raw_target:
        target = Path(raw_target)
        environment = _cargo_environment()
        environment["CARGO_TARGET_DIR"] = os.fspath(target)
        try:
            result = subprocess.run(
                [
                    _cargo(),
                    "package",
                    "--locked",
                    "--offline",
                    "--workspace",
                    "--exclude",
                    "greenlit-init",
                    "--no-verify",
                ],
                cwd=repository,
                env=environment,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
                timeout=10 * 60,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            raise ProvenanceError(
                f"independent exact-source packaging failed: {error}"
            ) from error
        if len(result.stdout) > MAX_CARGO_OUTPUT or len(result.stderr) > MAX_CARGO_OUTPUT:
            raise ProvenanceError("independent packaging exceeded its output limit")
        if result.returncode != 0:
            detail = result.stderr[-8192:].decode(
                "utf-8", errors="replace"
            ).strip()
            raise ProvenanceError(
                "independent exact-source packaging failed"
                + (f": {detail}" if detail else "")
            )
        package_root = target / "package"
        try:
            actual = {
                entry.name
                for entry in os.scandir(package_root)
                if entry.is_file(follow_symlinks=False)
            }
        except OSError as error:
            raise ProvenanceError(
                f"could not inspect independent package output: {error}"
            ) from error
        if actual != set(CRATES):
            raise ProvenanceError(
                "independent packaging did not produce the exact eight-crate closure"
            )
        return {
            basename: hash_regular(
                package_root / basename,
                f"independently packaged crate {basename}",
            )
            for basename in sorted(CRATES)
        }
