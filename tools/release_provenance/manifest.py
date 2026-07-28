"""Closed canonical release-provenance manifest schema."""

from __future__ import annotations

from pathlib import Path

from .common import (
    BINARY_BASENAME,
    CRATES,
    MAX_MANIFEST_BYTES,
    PARITY_FILES,
    SCHEMA,
    ProvenanceError,
    canonical_json,
    decode_json,
    expected_version,
    read_bounded,
    require_digest,
    require_exact_keys,
    require_object,
    require_string,
    validate_source,
)


def _validate(value: object, expected_source: str) -> dict[str, object]:
    root = require_object(value, "release provenance")
    require_exact_keys(
        root,
        {"binary", "crates", "parity", "schema", "source_commit"},
        "release provenance",
    )
    if require_string(root["schema"], "$.schema") != SCHEMA:
        raise ProvenanceError(f"$.schema must be {SCHEMA!r}")
    source = require_string(root["source_commit"], "$.source_commit")
    validate_source(source, "$.source_commit")
    if source != expected_source:
        raise ProvenanceError("$.source_commit does not match expected source")
    binary = require_object(root["binary"], "$.binary")
    require_exact_keys(
        binary,
        {"basename", "elf", "sha256", "version_output"},
        "$.binary",
    )
    if require_string(binary["basename"], "$.binary.basename") != BINARY_BASENAME:
        raise ProvenanceError("$.binary.basename is not litci")
    require_digest(binary["sha256"], "$.binary.sha256")
    if require_string(
        binary["version_output"], "$.binary.version_output"
    ) != expected_version(expected_source):
        raise ProvenanceError("$.binary.version_output is not exact")
    elf = require_object(binary["elf"], "$.binary.elf")
    require_exact_keys(elf, {"class", "data", "machine"}, "$.binary.elf")
    if elf != {
        "class": "ELF64",
        "data": "little-endian",
        "machine": "x86_64",
    }:
        raise ProvenanceError("$.binary.elf is not the fixed Linux x86-64 shape")
    crates = require_object(root["crates"], "$.crates")
    require_exact_keys(crates, set(CRATES), "$.crates")
    for basename in sorted(CRATES):
        entry = require_object(crates[basename], f"$.crates[{basename!r}]")
        require_exact_keys(
            entry,
            {"package", "sha256", "version", "vcs"},
            f"$.crates[{basename!r}]",
        )
        require_digest(entry["sha256"], f"$.crates[{basename!r}].sha256")
        expected_package = basename[: -len("-0.1.0.crate")]
        if entry["package"] != expected_package or entry["version"] != "0.1.0":
            raise ProvenanceError(f"$.crates[{basename!r}] identity is invalid")
        vcs = require_object(entry["vcs"], f"$.crates[{basename!r}].vcs")
        require_exact_keys(vcs, {"dirty", "sha1"}, f"$.crates[{basename!r}].vcs")
        if vcs != {"dirty": False, "sha1": expected_source}:
            raise ProvenanceError(f"$.crates[{basename!r}].vcs is invalid")
    parity = require_object(root["parity"], "$.parity")
    require_exact_keys(parity, set(PARITY_FILES), "$.parity")
    for relative in PARITY_FILES:
        require_digest(parity[relative], f"$.parity[{relative!r}]")
    return root


def difference(left: object, right: object, path: str = "$") -> str | None:
    """Return the first deterministic structural/value difference."""

    if type(left) is not type(right):
        return path
    if isinstance(left, dict):
        if set(left) != set(right):
            return path
        for key in sorted(left):
            found = difference(left[key], right[key], f"{path}[{key!r}]")
            if found is not None:
                return found
        return None
    return None if left == right else path


def load_manifest(candidate: Path, expected_source: str) -> dict[str, object]:
    """Read and validate one canonical manifest."""

    data = read_bounded(
        candidate / "RELEASE-PROVENANCE.json",
        "release provenance manifest",
        MAX_MANIFEST_BYTES,
    )
    document = _validate(
        decode_json(data, "release provenance manifest"),
        expected_source,
    )
    if data != canonical_json(document):
        raise ProvenanceError("release provenance manifest is not canonical JSON")
    return document
