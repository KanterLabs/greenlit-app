#!/usr/bin/env python3
"""Round-trip and malformed-archive cases for the public transfer canary."""

from __future__ import annotations

from collections.abc import Callable
import hashlib
import io
import os
import re
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

sys.dont_write_bytecode = True


SOURCE = "a" * 40
OTHER_SOURCE = "b" * 40
SHA256 = re.compile(r"[0-9a-f]{64}")
PYTHON = "/usr/bin/python3"
PACKAGES = tuple(
    "greenlit-actions greenlit-app greenlit-engine greenlit-expr greenlit-metrics "
    "greenlit-runtime greenlit-store greenlit-workflow".split()
)
ROLES = ("oracle", "github-actions", "greenlit-release")
FORBIDDEN_ENV = frozenset(
    "GH_TOKEN GITHUB_TOKEN GH_ENTERPRISE_TOKEN GITHUB_ENTERPRISE_TOKEN "
    "BASH_ENV ENV LD_AUDIT LD_PRELOAD PYTHONPATH PYTHONHOME PYTHONSTARTUP "
    "PYTHONINSPECT".split()
)
HELPER = Path(__file__).resolve().parent / "release_check_credential_bundle.py"


class CanaryError(Exception):
    """A public transfer-command canary failed."""


def _environment() -> dict[str, str]:
    environment = os.environ.copy()
    for key in tuple(environment):
        if key in FORBIDDEN_ENV or key.startswith("BASH_FUNC_"):
            environment.pop(key, None)
    environment["PYTHONDONTWRITEBYTECODE"] = "1"
    return environment


def _invoke(root: Path, arguments: list[str], *, accepted: bool) -> bytes:
    result = subprocess.run(
        [PYTHON, "-E", "-s", "-B", str(HELPER), *arguments],
        cwd=root,
        env=_environment(),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=20,
        check=False,
    )
    if accepted:
        if result.returncode != 0 or result.stderr:
            raise CanaryError(
                "transfer command rejected a valid case: "
                + result.stderr.decode("utf-8", errors="replace").strip()
            )
    elif (
        result.returncode == 0
        or result.stdout
        or not result.stderr.startswith(b"release transfer failed:")
    ):
        raise CanaryError("transfer command did not fail closed")
    return result.stdout


def _directory(path: Path, mode: int) -> None:
    path.mkdir(parents=True)
    path.chmod(mode)


def _file(path: Path, content: bytes, mode: int) -> None:
    path.write_bytes(content)
    path.chmod(mode)


def _prepared(root: Path, binary: bytes = b"\x7fELFprepared") -> Path:
    release = root / "candidate/target/release"
    package = root / "candidate/target/package"
    _directory(release, 0o755)
    _directory(package, 0o755)
    (root / "candidate").chmod(0o755)
    (root / "candidate/target").chmod(0o755)
    executable = release / "litci"
    _file(executable, binary, 0o755)
    for name in PACKAGES:
        _file(package / f"{name}-0.1.0.crate", name.encode(), 0o644)
    return executable


def _evidence(root: Path, roles: tuple[str, ...]) -> None:
    captures = root / "captures"
    _directory(captures, 0o700)
    for role in roles:
        _file(root / f"seed-{role}.json", f'{{"role":"{role}"}}'.encode(), 0o600)
        _file(
            captures / f"shell-only-seed-{role}.json",
            f'{{"capture":"{role}"}}'.encode(), 0o600
        )


def _pack(
    root: Path,
    kind: str,
    input_root: Path,
    output: Path,
    binary: Path | None = None,
) -> str:
    arguments = [
        f"pack-{kind}", "--input-root", str(input_root),
        "--output", str(output), "--expected-source", SOURCE,
    ]
    if binary is not None:
        arguments.extend(("--greenlit-binary", str(binary)))
    raw = _invoke(root, arguments, accepted=True).decode("ascii").strip()
    if SHA256.fullmatch(raw) is None:
        raise CanaryError(f"pack-{kind} did not emit one SHA-256 identity")
    return raw


def _unpack(
    root: Path,
    kind: str,
    bundle: Path,
    output: Path,
    digest: str,
    source: str = SOURCE,
    *,
    accepted: bool,
) -> None:
    arguments = [
        f"unpack-{kind}", "--bundle", str(bundle), "--output-root", str(output),
        "--expected-sha256", digest, "--expected-source", source,
    ]
    _invoke(root, arguments, accepted=accepted)


def _digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _rewrite(
    source: Path,
    output: Path,
    mutate: Callable[[list[tarfile.TarInfo]], None],
) -> None:
    with tarfile.open(source, "r:") as archive:
        members = archive.getmembers()
        contents = {
            id(member): archive.extractfile(member).read()
            for member in members
            if member.isfile()
        }
    mutate(members)
    with tarfile.open(output, "w", format=tarfile.USTAR_FORMAT) as archive:
        for member in members:
            archive.addfile(
                member,
                io.BytesIO(contents[id(member)]) if member.isfile() else None,
            )
    output.chmod(0o600)


def _round_trips(root: Path) -> tuple[Path, str, Path, Path]:
    prepared = root / "prepared"
    _directory(prepared, 0o700)
    binary = _prepared(prepared)
    prepared_tar = root / "prepared.tar"
    prepared_digest = _pack(root, "prepared", prepared, prepared_tar)
    prepared_out = root / "prepared-out"
    _unpack(root, "prepared", prepared_tar, prepared_out, prepared_digest, accepted=True)

    local = root / "local"
    _directory(local, 0o700)
    _evidence(local, ("oracle", "greenlit-release"))
    local_tar = root / "local.tar"
    local_digest = _pack(root, "local", local, local_tar, binary)
    local_out = root / "local-out"
    _unpack(root, "local", local_tar, local_out, local_digest, accepted=True)

    github = root / "github"
    _directory(github, 0o700)
    _evidence(github, ("github-actions",))
    github_tar = root / "github.tar"
    github_digest = _pack(root, "github", github, github_tar)
    _unpack(
        root, "github", github_tar, root / "github-out", github_digest, accepted=True
    )

    parity = root / "parity"
    _directory(parity, 0o700)
    _evidence(parity, ROLES)
    parity_tar = root / "parity.tar"
    parity_digest = _pack(root, "parity", parity, parity_tar)
    _unpack(
        root, "parity", parity_tar, root / "parity-out", parity_digest, accepted=True
    )
    if (
        (prepared_out / "candidate/target/release/litci").read_bytes()
        != binary.read_bytes()
        or (local_out / "binary/litci").read_bytes() != binary.read_bytes()
        or (prepared_out / "candidate/target/release/litci").stat().st_mode
        & 0o777
        != 0o755
        or (local_out / "parity/seed-oracle.json").stat().st_mode & 0o777
        != 0o600
    ):
        raise CanaryError("command-boundary round trip changed bytes or exact modes")
    _invoke(
        root,
        [
            "verify-binary-match",
            "--prepared-binary",
            str(prepared_out / "candidate/target/release/litci"),
            "--local-binary",
            str(local_out / "binary/litci"),
        ],
        accepted=True,
    )
    return local_tar, local_digest, github_tar, prepared_out


def _negative_cases(
    root: Path,
    local_tar: Path,
    local_digest: str,
    github_tar: Path,
    prepared_out: Path,
) -> None:
    mutated = root / "byte-mutated.tar"
    data = bytearray(local_tar.read_bytes())
    data[len(data) // 2] ^= 1
    _file(mutated, bytes(data), 0o600)
    _unpack(root, "local", mutated, root / "byte-out", local_digest, accepted=False)
    _unpack(root, "local", local_tar, root / "sha-out", "0" * 64, accepted=False)
    _unpack(
        root, "local", local_tar, root / "source-out",
        local_digest, OTHER_SOURCE, accepted=False,
    )
    _unpack(
        root, "local", github_tar, root / "role-out", _digest(github_tar), accepted=False
    )
    trailing = root / "trailing.tar"
    _file(trailing, local_tar.read_bytes() + b"trailing payload", 0o600)
    _unpack(
        root, "local", trailing, root / "trailing-out", _digest(trailing), accepted=False
    )
    hidden = root / "gnu-hidden.tar"
    _hidden_gnu_longname(local_tar, hidden)
    _unpack(
        root, "local", hidden, root / "gnu-out", _digest(hidden), accepted=False
    )

    extra_input = root / "extra-input"
    _directory(extra_input, 0o700)
    _evidence(extra_input, ("oracle", "greenlit-release"))
    _file(extra_input / "unexpected", b"x", 0o600)
    _invoke(
        root,
        [
            "pack-local",
            "--input-root",
            str(extra_input),
            "--greenlit-binary",
            str(prepared_out / "candidate/target/release/litci"),
            "--output",
            str(root / "extra.tar"),
            "--expected-source",
            SOURCE,
        ],
        accepted=False,
    )

    attacks: tuple[tuple[str, Callable[[list[tarfile.TarInfo]], None]], ...] = (
        ("duplicate", lambda members: members.append(members[0])),
        ("path", lambda members: setattr(members[-1], "name", "../escape")),
        ("absolute", lambda members: setattr(members[-1], "name", "/escape")),
        (
            "link",
            lambda members: _make_link(
                next(member for member in members if member.name == "binary/litci")
            ),
        ),
        (
            "mode",
            lambda members: setattr(
                next(member for member in members if member.name == "binary/litci"),
                "mode",
                0o644,
            ),
        ),
    )
    for label, mutate in attacks:
        attack = root / f"{label}.tar"
        _rewrite(local_tar, attack, mutate)
        _unpack(
            root,
            "local",
            attack,
            root / f"{label}-out",
            _digest(attack),
            accepted=False,
        )

    bundle_link = root / "bundle-link.tar"
    bundle_link.symlink_to(local_tar)
    _unpack(
        root, "local", bundle_link, root / "symlink-out", local_digest, accepted=False
    )
    symlink_target = root / "symlink-input-target"
    _directory(symlink_target, 0o700)
    _evidence(symlink_target, ("oracle", "greenlit-release"))
    symlink_root = root / "symlink-input"
    symlink_root.symlink_to(symlink_target, target_is_directory=True)
    _invoke(
        root,
        [
            "pack-local",
            "--input-root",
            str(symlink_root),
            "--greenlit-binary",
            str(prepared_out / "candidate/target/release/litci"),
            "--output",
            str(root / "symlink-input.tar"),
            "--expected-source",
            SOURCE,
        ],
        accepted=False,
    )
    linked_file_root = root / "linked-file-input"
    _directory(linked_file_root, 0o700)
    _evidence(linked_file_root, ("oracle", "greenlit-release"))
    linked_file = linked_file_root / "seed-oracle.json"
    real_file = root / "real-seed-oracle.json"
    linked_file.rename(real_file)
    linked_file.symlink_to(real_file)
    _pack_rejected(root, linked_file_root, prepared_out, "linked-file.tar")


def _make_link(member: tarfile.TarInfo) -> None:
    member.type = tarfile.SYMTYPE
    member.linkname = "/tmp/escape"
    member.size = 0


def _pack_rejected(root: Path, evidence: Path, prepared: Path, name: str) -> None:
    _invoke(
        root,
        [
            "pack-local",
            "--input-root",
            str(evidence),
            "--greenlit-binary",
            str(prepared / "candidate/target/release/litci"),
            "--output",
            str(root / name),
            "--expected-source",
            SOURCE,
        ],
        accepted=False,
    )


def _hidden_gnu_longname(source: Path, output: Path) -> None:
    with tarfile.open(source, "r:") as archive:
        offset = archive.getmember("binary/litci").offset
    payload = b"binary/litci\0"
    extension = tarfile.TarInfo("././@LongLink")
    extension.type = tarfile.GNUTYPE_LONGNAME
    extension.mode = 0o600
    extension.size = len(payload)
    header = extension.tobuf(format=tarfile.GNU_FORMAT)
    record = header + payload + b"\0" * (-len(payload) % tarfile.BLOCKSIZE)
    data = source.read_bytes()
    mutated = data[:offset] + record + data[offset:]
    mutated += b"\0" * (-len(mutated) % tarfile.RECORDSIZE)
    _file(output, mutated, 0o600)


def run_cases() -> None:
    previous_umask = os.umask(0o077)
    try:
        with tempfile.TemporaryDirectory(prefix="greenlit-transfer-canary.") as raw:
            root = Path(raw)
            root.chmod(0o700)
            local_tar, local_digest, github_tar, prepared_out = _round_trips(root)
            _negative_cases(root, local_tar, local_digest, github_tar, prepared_out)
    finally:
        os.umask(previous_umask)
