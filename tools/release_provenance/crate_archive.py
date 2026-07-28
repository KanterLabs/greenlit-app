"""Static, source-derived validation for Cargo package archives."""

from __future__ import annotations

import os
import tarfile
import tomllib
from pathlib import Path

from .common import (
    APP_VERSION,
    MAX_CRATE_BYTES,
    MAX_CRATE_EXPANDED_BYTES,
    MAX_CRATE_MEMBER_BYTES,
    MAX_TAR_MEMBERS,
    MAX_VCS_INFO_BYTES,
    ProvenanceError,
    decode_json,
    hash_stream,
    open_regular,
    read_bounded,
    require_exact_keys,
    require_object,
    require_string,
    validate_source,
)
from .package_source import package_projection


def _safe_relative(value: str, label: str) -> tuple[str, ...]:
    if (
        not value
        or value.startswith("/")
        or "\\" in value
        or "\0" in value
    ):
        raise ProvenanceError(f"{label} has unsafe path {value!r}")
    parts = tuple(value.split("/"))
    if any(part in {"", ".", ".."} for part in parts):
        raise ProvenanceError(f"{label} has unsafe path {value!r}")
    return parts


def _vcs_document(
    data: bytes,
    label: str,
    expected_source: str,
    expected_path: str,
) -> dict[str, object]:
    root = require_object(decode_json(data, label), label)
    require_exact_keys(root, {"git", "path_in_vcs"}, label)
    git = require_object(root["git"], f"{label}.git")
    if set(git) not in ({"sha1"}, {"sha1", "dirty"}):
        require_exact_keys(git, {"sha1", "dirty"}, f"{label}.git")
    sha1 = require_string(git["sha1"], f"{label}.git.sha1")
    validate_source(sha1, f"{label}.git.sha1")
    if sha1 != expected_source:
        raise ProvenanceError(
            f"{label}.git.sha1 is {sha1}, not expected source {expected_source}"
        )
    dirty = git.get("dirty", False)
    if type(dirty) is not bool or dirty:
        raise ProvenanceError(f"{label}.git.dirty must be false or omitted")
    path_in_vcs = require_string(root["path_in_vcs"], f"{label}.path_in_vcs")
    if path_in_vcs != expected_path:
        raise ProvenanceError(
            f"{label}.path_in_vcs is {path_in_vcs!r}, expected {expected_path!r}"
        )
    return {"dirty": False, "sha1": sha1}


def _package_manifest(data: bytes, package: str, label: str) -> None:
    try:
        document = tomllib.loads(data.decode("utf-8", errors="strict"))
    except (UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
        raise ProvenanceError(f"{label} is not valid UTF-8 TOML: {error}") from error
    package_table = document.get("package")
    if not isinstance(package_table, dict):
        raise ProvenanceError(f"{label} has no [package] table")
    if package_table.get("name") != package:
        raise ProvenanceError(f"{label} package.name is not {package!r}")
    if package_table.get("version") != APP_VERSION:
        raise ProvenanceError(f"{label} package.version is not {APP_VERSION!r}")


def _source_bytes(
    repository: Path,
    expected_path: str,
    relative: str,
) -> bytes:
    source_relative = "Cargo.toml" if relative == "Cargo.toml.orig" else relative
    source = repository / expected_path / source_relative
    return read_bounded(
        source,
        f"exact source file {expected_path}/{source_relative}",
        MAX_CRATE_MEMBER_BYTES,
    )


def inspect_crate(
    path: Path,
    basename: str,
    repository: Path,
    expected_source: str,
    expected_path: str,
    canonical_digest: str,
) -> dict[str, object]:
    """Validate one `.crate` against Cargo's projection and independent bytes."""

    label = f"crate {basename}"
    package = basename[: -len(f"-{APP_VERSION}.crate")]
    archive_root = basename[: -len(".crate")]
    projection = package_projection(repository, package)
    expected_names = {f"{archive_root}/{member}" for member in projection}
    with open_regular(path, label) as handle:
        metadata = os.fstat(handle.fileno())
        if metadata.st_size > MAX_CRATE_BYTES:
            raise ProvenanceError(f"{label} exceeds {MAX_CRATE_BYTES} bytes")
        digest_before = hash_stream(handle)
        if digest_before != canonical_digest:
            raise ProvenanceError(
                f"{label} differs from the independently packaged exact source"
            )
        handle.seek(0)
        contents: dict[str, bytes] = {}
        expanded = 0
        seen: set[str] = set()
        try:
            with tarfile.open(fileobj=handle, mode="r:gz") as archive:
                if archive.pax_headers:
                    raise ProvenanceError(f"{label} has global PAX overrides")
                for index, member in enumerate(archive, start=1):
                    if index > MAX_TAR_MEMBERS:
                        raise ProvenanceError(
                            f"{label} contains more than {MAX_TAR_MEMBERS} members"
                        )
                    _safe_relative(member.name, label)
                    if member.name in seen:
                        raise ProvenanceError(
                            f"{label} repeats member {member.name!r}"
                        )
                    seen.add(member.name)
                    if (
                        member.name not in expected_names
                        or not member.isfile()
                        or member.mode != 0o644
                        or member.uid != 0
                        or member.gid != 0
                        or member.uname
                        or member.gname
                        or member.pax_headers
                        or getattr(member, "sparse", None)
                    ):
                        raise ProvenanceError(
                            f"{label} has noncanonical member {member.name!r}"
                        )
                    if member.size > MAX_CRATE_MEMBER_BYTES:
                        raise ProvenanceError(
                            f"{label} member {member.name!r} exceeds its size limit"
                        )
                    expanded += member.size
                    if expanded > MAX_CRATE_EXPANDED_BYTES:
                        raise ProvenanceError(
                            f"{label} exceeds its expanded-byte limit"
                        )
                    extracted = archive.extractfile(member)
                    if extracted is None:
                        raise ProvenanceError(
                            f"could not read {label} member {member.name!r}"
                        )
                    data = extracted.read(MAX_CRATE_MEMBER_BYTES + 1)
                    if len(data) != member.size:
                        raise ProvenanceError(
                            f"{label} member {member.name!r} changed size"
                        )
                    relative = member.name[len(archive_root) + 1 :]
                    contents[relative] = data
        except (tarfile.TarError, OSError) as error:
            raise ProvenanceError(f"could not inspect {label}: {error}") from error
        if seen != expected_names:
            raise ProvenanceError(
                f"{label} member closure differs from Cargo's exact projection"
            )
        handle.seek(0)
        digest_after = hash_stream(handle)
        final = os.fstat(handle.fileno())
    if (
        digest_before != digest_after
        or (metadata.st_dev, metadata.st_ino, metadata.st_size, metadata.st_mtime_ns)
        != (final.st_dev, final.st_ino, final.st_size, final.st_mtime_ns)
    ):
        raise ProvenanceError(f"{label} changed while it was inspected")

    manifest = contents.get("Cargo.toml")
    original = contents.get("Cargo.toml.orig")
    vcs_data = contents.get(".cargo_vcs_info.json")
    if manifest is None or original is None or vcs_data is None:
        raise ProvenanceError(f"{label} omits required package metadata")
    _package_manifest(manifest, package, f"{label} Cargo.toml")
    if len(vcs_data) > MAX_VCS_INFO_BYTES:
        raise ProvenanceError(f"{label} VCS metadata exceeds its size limit")
    vcs = _vcs_document(
        vcs_data,
        f"{label} .cargo_vcs_info.json",
        expected_source,
        expected_path,
    )
    source_payload = 0
    for relative, data in contents.items():
        if relative in {".cargo_vcs_info.json", "Cargo.lock", "Cargo.toml"}:
            continue
        if data != _source_bytes(repository, expected_path, relative):
            raise ProvenanceError(
                f"{label} member {relative!r} differs from exact source bytes"
            )
        if relative != "Cargo.toml.orig":
            source_payload += 1
    if source_payload == 0:
        raise ProvenanceError(f"{label} contains no exact-source payload")
    return {
        "package": package,
        "sha256": digest_before,
        "version": APP_VERSION,
        "vcs": vcs,
    }
