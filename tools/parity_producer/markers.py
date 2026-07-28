"""Strict extraction of contract markers emitted by the parity seed."""

from __future__ import annotations

import re
from dataclasses import dataclass

from parity_producer.common import ProducerError, SHA256


GITHUB_LOG_PREFIX = re.compile(
    r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T"
    r"[0-9]{2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]{1,9})?Z "
)
CONTEXT = re.compile(r"^PARITY_CONTEXT ([A-Za-z0-9._-]+)=(.*)$")
OUTPUT = re.compile(r"^PARITY_OUTPUT ([A-Za-z0-9._-]+)=(.*)$")
TEMPORARY = re.compile(r"^PARITY_TEMPORARY_DIRECTORY (/.+)$")
PROBE = re.compile(
    r"^PARITY_PROBE ([A-Za-z0-9._-]+) "
    r"mode=(0[0-7]{3}) sha256=([0-9a-f]{64})$"
)
IDENTITY = re.compile(
    r"^PARITY_IDENTITY job=([A-Za-z0-9._-]+) step=([A-Za-z0-9._-]+)$"
)
EXPECTED_CONTEXTS = {
    "github.job",
    "github.workflow",
    "runner.arch",
    "runner.os",
}


@dataclass(frozen=True)
class SeedMarkers:
    """The exact non-lifecycle values observed inside the seed step."""

    contexts: dict[str, str]
    identities: tuple[tuple[str, str], ...]
    seed_value: str
    temporary_directory: str
    probe_id: str
    probe_mode: str
    probe_sha256: str


def parse_markers(lines: list[str], source: str) -> SeedMarkers:
    """Parse the exact marker set and reject missing or ambiguous evidence."""
    contexts: dict[str, str] = {}
    outputs: dict[str, str] = {}
    temporary_directories: list[str] = []
    probes: list[tuple[str, str, str]] = []
    identities: list[tuple[str, str]] = []

    for raw_line in lines:
        line = raw_line.rstrip("\r\n")
        line = GITHUB_LOG_PREFIX.sub("", line, count=1)
        if not line.startswith("PARITY_"):
            continue
        context = CONTEXT.fullmatch(line)
        if context is not None:
            identity, value = context.groups()
            _insert_unique(contexts, identity, value, source, "context")
            continue
        output = OUTPUT.fullmatch(line)
        if output is not None:
            identity, value = output.groups()
            _insert_unique(outputs, identity, value, source, "output")
            continue
        temporary = TEMPORARY.fullmatch(line)
        if temporary is not None:
            temporary_directories.append(temporary.group(1))
            continue
        probe = PROBE.fullmatch(line)
        if probe is not None:
            probes.append(probe.groups())
            continue
        identity = IDENTITY.fullmatch(line)
        if identity is not None:
            identities.append(identity.groups())
            continue
        raise ProducerError(f"{source} contains a malformed parity marker: {line!r}")

    if set(contexts) != EXPECTED_CONTEXTS:
        missing = sorted(EXPECTED_CONTEXTS - set(contexts))
        extra = sorted(set(contexts) - EXPECTED_CONTEXTS)
        detail = f"missing {missing[0]!r}" if missing else f"unexpected {extra[0]!r}"
        raise ProducerError(f"{source} context markers are incomplete: {detail}")
    if contexts["github.job"] != "shell":
        raise ProducerError(f"{source} observed github.job other than 'shell'")
    if contexts["github.workflow"] != "Parity seed":
        raise ProducerError(f"{source} observed an unexpected workflow name")
    if contexts["runner.arch"] != "X64" or contexts["runner.os"] != "Linux":
        raise ProducerError(f"{source} did not execute on the Linux x86_64 seed runner")
    if outputs != {"seed_value": "greenlit"}:
        raise ProducerError(f"{source} did not observe exactly seed_value=greenlit")
    if identities != [("shell", "emit"), ("shell", "verify")]:
        raise ProducerError(
            f"{source} did not observe the exact authored job/step identity order"
        )
    if len(temporary_directories) != 1:
        raise ProducerError(
            f"{source} must contain exactly one temporary-directory marker"
        )
    if len(probes) != 1:
        raise ProducerError(f"{source} must contain exactly one filesystem probe")
    probe_id, probe_mode, probe_sha256 = probes[0]
    if probe_id != "parity-seed-file":
        raise ProducerError(f"{source} contains an unexpected filesystem probe")
    if probe_mode != "0644":
        raise ProducerError(f"{source} parity seed file mode is not 0644")
    if SHA256.fullmatch(probe_sha256) is None:
        raise ProducerError(f"{source} parity seed file digest is malformed")

    return SeedMarkers(
        contexts=contexts,
        identities=tuple(identities),
        seed_value=outputs["seed_value"],
        temporary_directory=temporary_directories[0],
        probe_id=probe_id,
        probe_mode=probe_mode,
        probe_sha256=probe_sha256,
    )


def _insert_unique(
    target: dict[str, str],
    identity: str,
    value: str,
    source: str,
    kind: str,
) -> None:
    if not value:
        raise ProducerError(f"{source} contains an empty {kind} marker {identity!r}")
    if identity in target:
        raise ProducerError(f"{source} contains duplicate {kind} marker {identity!r}")
    target[identity] = value
