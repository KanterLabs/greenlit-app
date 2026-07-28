"""Deterministic mode-preserving sealing and safe authenticated unpacking."""

from __future__ import annotations

import os
import tarfile
import tempfile
from pathlib import Path

from .bundle_contract import (
    CLOSURE,
    MAX_BUNDLE_BYTES,
    MAX_BUNDLE_EXPANDED_BYTES,
    MAX_BUNDLE_FILE_BYTES,
)
from .candidate import verify
from .common import (
    ProvenanceError,
    hash_stream,
    open_regular,
    require_mode,
    validated_directory,
)
from .repository import verify_repository


def _source_path(
    name: str,
    relative: str,
    candidate: Path,
    parity: Path,
) -> Path:
    return (candidate if name.startswith("release-candidate/") else parity) / relative


def _tar_info(name: str, kind: str, mode: int, size: int = 0) -> tarfile.TarInfo:
    info = tarfile.TarInfo(name=name)
    info.mode = mode
    info.uid = 0
    info.gid = 0
    info.uname = ""
    info.gname = ""
    info.mtime = 0
    info.size = size
    info.type = tarfile.DIRTYPE if kind == "directory" else tarfile.REGTYPE
    return info


def _write_tar(
    destination: Path,
    candidate: Path,
    parity: Path,
    identity: tuple[int, int],
) -> None:
    flags = os.O_WRONLY | os.O_TRUNC
    flags |= getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(destination, flags)
    opened = os.fstat(descriptor)
    if (opened.st_dev, opened.st_ino) != identity:
        os.close(descriptor)
        raise ProvenanceError("sealed-bundle temporary identity changed")
    with os.fdopen(descriptor, "wb", closefd=True) as output:
        os.fchmod(output.fileno(), 0o644)
        with tarfile.open(
            fileobj=output,
            mode="w",
            format=tarfile.USTAR_FORMAT,
        ) as archive:
            for name in sorted(CLOSURE):
                kind, mode, relative = CLOSURE[name]
                if kind == "directory":
                    archive.addfile(_tar_info(name, kind, mode))
                    continue
                if relative is None:
                    raise ProvenanceError(f"bundle source is absent for {name}")
                source_path = _source_path(name, relative, candidate, parity)
                require_mode(
                    source_path,
                    mode,
                    f"bundle source {name}",
                    directory=False,
                )
                with open_regular(source_path, f"bundle source {name}") as source:
                    before = os.fstat(source.fileno())
                    if before.st_size > MAX_BUNDLE_FILE_BYTES:
                        raise ProvenanceError(f"bundle source {name} is oversized")
                    archive.addfile(
                        _tar_info(name, kind, mode, before.st_size),
                        source,
                    )
                    after = os.fstat(source.fileno())
                if (
                    before.st_dev,
                    before.st_ino,
                    before.st_size,
                    before.st_mtime_ns,
                    before.st_ctime_ns,
                ) != (
                    after.st_dev,
                    after.st_ino,
                    after.st_size,
                    after.st_mtime_ns,
                    after.st_ctime_ns,
                ):
                    raise ProvenanceError(f"bundle source changed while read: {name}")
        output.flush()
        os.fsync(output.fileno())
    if destination.stat().st_size > MAX_BUNDLE_BYTES:
        raise ProvenanceError("sealed release bundle exceeds its size limit")


def _validate_members(archive: tarfile.TarFile) -> list[tarfile.TarInfo]:
    members: list[tarfile.TarInfo] = []
    names: set[str] = set()
    expanded = 0
    if archive.pax_headers:
        raise ProvenanceError("release bundle has global PAX overrides")
    for member in archive:
        if member.name in names:
            raise ProvenanceError(
                f"release bundle repeats member {member.name!r}"
            )
        names.add(member.name)
        expected = CLOSURE.get(member.name)
        if expected is None:
            raise ProvenanceError(
                f"release bundle contains unexpected member {member.name!r}"
            )
        kind, mode, _ = expected
        actual_kind = (
            "directory"
            if member.isdir()
            else "file"
            if member.isfile()
            else "special"
        )
        if (
            actual_kind != kind
            or member.mode != mode
            or member.uid != 0
            or member.gid != 0
            or member.uname
            or member.gname
            or member.pax_headers
            or member.linkname
            or getattr(member, "sparse", None)
            or member.mtime != 0
            or (kind == "directory" and member.size != 0)
        ):
            raise ProvenanceError(
                f"release bundle member is noncanonical: {member.name!r}"
            )
        if member.size > MAX_BUNDLE_FILE_BYTES:
            raise ProvenanceError(
                f"release bundle member is oversized: {member.name!r}"
            )
        expanded += member.size
        if expanded > MAX_BUNDLE_EXPANDED_BYTES:
            raise ProvenanceError("release bundle exceeds its expanded-byte limit")
        members.append(member)
    if names != set(CLOSURE):
        raise ProvenanceError("release bundle does not have its exact closure")
    if [member.name for member in members] != sorted(CLOSURE):
        raise ProvenanceError("release bundle members are not canonically ordered")
    return members


def _private_empty(path: Path) -> None:
    metadata = require_mode(path, 0o700, "bundle output root", directory=True)
    if metadata.st_uid != os.geteuid():
        raise ProvenanceError("bundle output root is not owned by this process")
    try:
        if any(path.iterdir()):
            raise ProvenanceError("bundle output root must initially be empty")
    except OSError as error:
        raise ProvenanceError(f"could not inspect bundle output root: {error}") from error


def _remove_created(output: Path, names: list[str]) -> None:
    for name in reversed(names):
        path = output / name
        try:
            if path.is_dir() and not path.is_symlink():
                path.rmdir()
            else:
                path.unlink()
        except OSError:
            pass


def _extract(
    handle,
    output: Path,
    expected_digest: str,
) -> None:
    before = os.fstat(handle.fileno())
    if before.st_size > MAX_BUNDLE_BYTES:
        raise ProvenanceError("release bundle exceeds its size limit")
    digest_before = hash_stream(handle)
    if digest_before != expected_digest:
        raise ProvenanceError("release bundle digest does not match trusted output")
    handle.seek(0)
    try:
        with tarfile.open(fileobj=handle, mode="r:") as archive:
            validated = _validate_members(archive)
    except (tarfile.TarError, OSError) as error:
        raise ProvenanceError(f"could not inspect sealed release bundle: {error}") from error

    created: list[str] = []
    try:
        for name in sorted(
            item for item, value in CLOSURE.items() if value[0] == "directory"
        ):
            path = output / name
            path.mkdir(mode=CLOSURE[name][1])
            os.chmod(path, CLOSURE[name][1])
            created.append(name)
        handle.seek(0)
        with tarfile.open(fileobj=handle, mode="r:") as archive:
            second = _validate_members(archive)
            if [member.name for member in second] != [
                member.name for member in validated
            ]:
                raise ProvenanceError("release bundle changed between validation passes")
            for member in second:
                kind, mode, _ = CLOSURE[member.name]
                if kind == "directory":
                    continue
                extracted = archive.extractfile(member)
                if extracted is None:
                    raise ProvenanceError(
                        f"could not read release bundle member {member.name!r}"
                    )
                destination = output / member.name
                flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
                flags |= getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
                descriptor = os.open(destination, flags, 0o600)
                created.append(member.name)
                try:
                    remaining = member.size
                    while remaining:
                        chunk = extracted.read(min(1024 * 1024, remaining))
                        if not chunk:
                            raise ProvenanceError(
                                f"release bundle member {member.name!r} is truncated"
                            )
                        view = memoryview(chunk)
                        while view:
                            written = os.write(descriptor, view)
                            if written <= 0:
                                raise ProvenanceError(
                                    f"could not write bundle member {member.name!r}"
                                )
                            view = view[written:]
                        remaining -= len(chunk)
                    if extracted.read(1):
                        raise ProvenanceError(
                            f"release bundle member {member.name!r} is oversized"
                        )
                    os.fsync(descriptor)
                    os.fchmod(descriptor, mode)
                finally:
                    os.close(descriptor)
        handle.seek(0)
        digest_after = hash_stream(handle)
        after = os.fstat(handle.fileno())
        if (
            digest_after != digest_before
            or (
                before.st_dev,
                before.st_ino,
                before.st_size,
                before.st_mtime_ns,
                before.st_ctime_ns,
            )
            != (
                after.st_dev,
                after.st_ino,
                after.st_size,
                after.st_mtime_ns,
                after.st_ctime_ns,
            )
        ):
            raise ProvenanceError("release bundle changed while it was unpacked")
    except (OSError, tarfile.TarError, ProvenanceError):
        _remove_created(output, created)
        raise


def unpack(
    repository: Path,
    bundle: Path,
    output: Path,
    expected_source: str,
    expected_digest: str,
) -> None:
    """Authenticate, safely unpack, and statically verify one sealed bundle."""

    verify_repository(repository, expected_source)
    _private_empty(output)
    with open_regular(bundle, "sealed release bundle") as handle:
        _extract(handle, output, expected_digest)
    try:
        verify(
            repository,
            output / "release-candidate",
            output / "parity-evidence",
            expected_source,
        )
    except ProvenanceError:
        _remove_created(output, sorted(CLOSURE))
        raise


def _fsync_directory(directory: Path) -> None:
    descriptor = os.open(
        directory,
        os.O_RDONLY | getattr(os, "O_DIRECTORY", 0),
    )
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def seal(
    repository: Path,
    candidate: Path,
    parity: Path,
    output: Path,
    expected_source: str,
) -> str:
    """Verify, seal, self-unpack, and atomically publish one candidate tar."""

    verify(repository, candidate, parity, expected_source)
    if output.exists() or output.is_symlink():
        raise ProvenanceError(f"refusing to overwrite sealed bundle {output}")
    parent = validated_directory(output.parent, "bundle parent")
    if os.lstat(parent).st_uid != os.geteuid():
        raise ProvenanceError("bundle parent is not owned by this process")
    descriptor, raw = tempfile.mkstemp(
        prefix=".greenlit-release-bundle.",
        suffix=".tmp",
        dir=parent,
    )
    temporary = Path(raw)
    temporary_metadata = os.fstat(descriptor)
    temporary_identity = (temporary_metadata.st_dev, temporary_metadata.st_ino)
    os.close(descriptor)
    try:
        _write_tar(temporary, candidate, parity, temporary_identity)
        with open_regular(temporary, "new sealed release bundle") as handle:
            digest = hash_stream(handle)
        with tempfile.TemporaryDirectory(
            prefix="greenlit-bundle-selfcheck.",
            dir=parent,
        ) as raw_extract:
            extract = Path(raw_extract)
            os.chmod(extract, 0o700)
            unpack(repository, temporary, extract, expected_source, digest)
        metadata = os.lstat(temporary)
        identity = (metadata.st_dev, metadata.st_ino)
        os.link(temporary, output, follow_symlinks=False)
        temporary.unlink()
        _fsync_directory(parent)
        final = os.lstat(output)
        if (final.st_dev, final.st_ino) != identity:
            raise ProvenanceError("sealed bundle identity changed during publication")
        return digest
    except OSError as error:
        raise ProvenanceError(f"could not seal release bundle: {error}") from error
    finally:
        if temporary.exists() or temporary.is_symlink():
            temporary.unlink()
