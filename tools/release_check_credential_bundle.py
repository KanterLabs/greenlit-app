#!/usr/bin/env python3
"""Deterministic, SHA-bound transfer bundles for split release jobs."""

from __future__ import annotations

import argparse
from contextlib import ExitStack
import os
import re
import sys
import tarfile
import tempfile
from pathlib import Path
from typing import BinaryIO

sys.dont_write_bytecode = True

from release_check_credential_bundle_io import (
    MAX_BUNDLE_BYTES,
    MAX_FILE_BYTES,
    BundleError,
    hash_stream as _hash_stream,
    reader as _reader,
    sha256 as _sha256,
)
from release_check_credential_bundle_extract import (
    expected_names as _expected_names,
    extract_member as _extract_member,
)
from release_check_credential_bundle_inputs import bind_inputs as _bind_inputs

SHA256 = re.compile(r"[0-9a-f]{64}")
COMMIT = re.compile(r"[0-9a-f]{40}")


def _tar_info(name: str, mode: int, size: int, kind: bytes) -> tarfile.TarInfo:
    result = tarfile.TarInfo(name)
    result.mode = mode
    result.uid = 0
    result.gid = 0
    result.uname = ""
    result.gname = ""
    result.mtime = 0
    result.size = size
    result.type = kind
    return result


def _pack(
    kind: str,
    root: Path,
    output: Path,
    source: str,
    binary: Path | None,
    *,
    prepared_unpacked: bool = False,
) -> str:
    if not root.is_absolute() or not output.is_absolute():
        raise BundleError("release transfer paths must be absolute")
    if output.exists() or output.is_symlink():
        raise BundleError("release transfer output must not already exist")
    temporary: Path | None = None
    try:
        with ExitStack() as inputs:
            directories, opened = _bind_inputs(
                kind,
                root,
                inputs,
                source,
                binary,
                prepared_unpacked=prepared_unpacked,
            )
            descriptor, temporary_name = tempfile.mkstemp(
                prefix=f".{output.name}.", dir=output.parent
            )
            temporary = Path(temporary_name)
            os.fchmod(descriptor, 0o600)
            with os.fdopen(descriptor, "w+b", closefd=True) as stream:
                with tarfile.open(
                    fileobj=stream, mode="w", format=tarfile.USTAR_FORMAT
                ) as archive:
                    source_bytes = f"{source}\n".encode()
                    source_info = _tar_info(
                        "source-commit", 0o600, len(source_bytes), b"0"
                    )
                    import io

                    archive.addfile(source_info, io.BytesIO(source_bytes))
                    for name, mode in directories:
                        archive.addfile(_tar_info(name, mode, 0, b"5"))
                    for name, content, metadata, mode in opened:
                        info = _tar_info(name, mode, metadata.st_size, b"0")
                        archive.addfile(info, content)
                stream.flush()
                os.fsync(stream.fileno())
        digest = _sha256(temporary)
        os.replace(temporary, output)
        temporary = None
        return digest
    except (OSError, tarfile.TarError) as error:
        raise BundleError(f"cannot seal release transfer bundle: {error}") from error
    finally:
        if temporary is not None:
            try:
                temporary.unlink()
            except FileNotFoundError:
                pass


def _unpack(kind: str, bundle: Path, output: Path, digest: str, source: str) -> None:
    if not bundle.is_absolute() or not output.is_absolute():
        raise BundleError("release transfer paths must be absolute")
    if output.exists() or output.is_symlink():
        raise BundleError("release transfer output must not already exist")
    try:
        with _reader(bundle, 0o600, MAX_BUNDLE_BYTES) as (stream, _):
            if _hash_stream(stream) != digest:
                raise BundleError("release transfer bundle digest does not match")
            stream.seek(0)
            output.mkdir(mode=0o700)
            output.chmod(0o700)
            archive = tarfile.open(fileobj=stream, mode="r:")
            try:
                members = tuple(archive.getmembers())
                names = [member.name for member in members]
                if (
                    len(names) != len(set(names))
                    or set(names) != _expected_names(kind, members)
                ):
                    raise BundleError(
                        "release transfer bundle does not have exact closure"
                    )
                for member in members:
                    _extract_member(archive, member, output)
            finally:
                archive.close()
            marker_path = output / "source-commit"
            with _reader(marker_path, 0o600, 41) as (marker_stream, _):
                marker = marker_stream.read(42).decode("ascii")
            if marker != f"{source}\n":
                raise BundleError("release transfer source identity does not match")
            _require_canonical(kind, output, source, stream)
    except (OSError, tarfile.TarError, UnicodeError) as error:
        raise BundleError(f"cannot unpack release transfer bundle: {error}") from error


def _require_canonical(
    kind: str,
    output: Path,
    source: str,
    supplied: BinaryIO,
) -> None:
    with tempfile.TemporaryDirectory(
        prefix="greenlit-canonical-transfer.",
        dir=output.parent,
    ) as raw:
        canonical_root = Path(raw)
        canonical_root.chmod(0o700)
        canonical = canonical_root / "canonical.tar"
        pack_root = output if kind == "prepared" else output / "parity"
        binary = output / "binary/litci" if kind == "local" else None
        _pack(
            kind,
            pack_root,
            canonical,
            source,
            binary,
            prepared_unpacked=kind == "prepared",
        )
        with _reader(canonical, 0o600, MAX_BUNDLE_BYTES) as (
            canonical_stream,
            _,
        ):
            supplied.seek(0)
            while True:
                expected = canonical_stream.read(1024 * 1024)
                observed = supplied.read(1024 * 1024)
                if expected != observed:
                    raise BundleError(
                        "release transfer bundle is not canonical USTAR"
                    )
                if not expected:
                    return


def _verify_binary_match(prepared: Path, local: Path) -> None:
    if not prepared.is_absolute() or not local.is_absolute():
        raise BundleError("release binary comparison paths must be absolute")
    with _reader(prepared, 0o755, MAX_FILE_BYTES) as (prepared_stream, _):
        with _reader(local, 0o755, MAX_FILE_BYTES) as (local_stream, _):
            while True:
                prepared_chunk = prepared_stream.read(1024 * 1024)
                local_chunk = local_stream.read(1024 * 1024)
                if prepared_chunk != local_chunk:
                    raise BundleError(
                        "local parity binary differs from the prepared release binary"
                    )
                if not prepared_chunk:
                    return


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    for kind in ("prepared", "local", "github", "parity"):
        pack = commands.add_parser(f"pack-{kind}")
        pack.add_argument("--input-root", type=Path, required=True)
        pack.add_argument("--output", type=Path, required=True)
        pack.add_argument("--expected-source", required=True)
        if kind == "local":
            pack.add_argument("--greenlit-binary", type=Path, required=True)
        unpack = commands.add_parser(f"unpack-{kind}")
        unpack.add_argument("--bundle", type=Path, required=True)
        unpack.add_argument("--output-root", type=Path, required=True)
        unpack.add_argument("--expected-sha256", required=True)
        unpack.add_argument("--expected-source", required=True)
    binary_match = commands.add_parser("verify-binary-match")
    binary_match.add_argument("--prepared-binary", type=Path, required=True)
    binary_match.add_argument("--local-binary", type=Path, required=True)
    return parser


def main() -> int:
    """Route deterministic split-job bundle operations."""

    arguments = _parser().parse_args()
    try:
        if arguments.command == "verify-binary-match":
            _verify_binary_match(
                arguments.prepared_binary.absolute(),
                arguments.local_binary.absolute(),
            )
            print("verified prepared/local release binary identity")
            return 0
        if COMMIT.fullmatch(arguments.expected_source) is None:
            raise BundleError("expected source must be a full lowercase commit")
        kind = arguments.command.split("-", 1)[1]
        if arguments.command.startswith("pack-"):
            binary = getattr(arguments, "greenlit_binary", None)
            digest = _pack(
                kind,
                arguments.input_root.absolute(),
                arguments.output.absolute(),
                arguments.expected_source,
                binary.absolute() if binary is not None else None,
            )
            print(digest)
        else:
            if SHA256.fullmatch(arguments.expected_sha256) is None:
                raise BundleError("expected digest must be lowercase SHA-256")
            _unpack(
                kind,
                arguments.bundle.absolute(),
                arguments.output_root.absolute(),
                arguments.expected_sha256,
                arguments.expected_source,
            )
    except (BundleError, OSError) as error:
        print(f"release transfer failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
