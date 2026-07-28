"""Closed YAML subset parser for capability-governed workflow jobs."""

from __future__ import annotations

import hashlib
import re
from dataclasses import dataclass
from pathlib import Path

from cargo_test_manifest import GateError


JOB_HEADER = re.compile(r"^  ([A-Za-z0-9_-]+):$")
STEP_START = re.compile(r"^      -(?: (?P<first>.*))?$")
STEP_KEY = re.compile(r"^        (?P<key>[A-Za-z0-9_-]+):(?: (?P<value>.*))?$")


@dataclass(frozen=True)
class WorkflowStep:
    """One fully parsed workflow step in exact source order."""

    ordinal: int
    name: str | None
    kind: str
    command: str
    block_sha256: str


def job_block(lines: list[str], job: str, path: Path) -> list[str]:
    """Return one exact top-level job body."""

    matches = [
        index
        for index, line in enumerate(lines)
        if (match := JOB_HEADER.fullmatch(line)) and match.group(1) == job
    ]
    if len(matches) != 1:
        raise GateError(f"{path}: expected exactly one job id {job!r}")
    start = matches[0] + 1
    end = len(lines)
    for index in range(start, len(lines)):
        if JOB_HEADER.fullmatch(lines[index]) or (
            lines[index] and not lines[index].startswith((" ", "#"))
        ):
            end = index
            break
    return lines[start:end]


def scalar(block: list[str], key: str, path: Path, job: str) -> str:
    prefix = f"    {key}: "
    values = [line[len(prefix) :] for line in block if line.startswith(prefix)]
    if len(values) != 1 or not values[0]:
        raise GateError(f"{path}: job {job!r} must declare exactly one {key}")
    return values[0]


def needs(block: list[str], path: Path, job: str) -> list[str]:
    inline = [
        line[len("    needs: ") :]
        for line in block
        if line.startswith("    needs: ")
    ]
    headers = [index for index, line in enumerate(block) if line == "    needs:"]
    if len(inline) + len(headers) != 1:
        raise GateError(f"{path}: job {job!r} must declare exactly one needs")
    if inline:
        return [inline[0]]
    start = headers[0] + 1
    result: list[str] = []
    for line in block[start:]:
        if line.startswith("      - "):
            result.append(line[len("      - ") :])
            continue
        if line.strip() and not line.startswith("      "):
            break
    if not result:
        raise GateError(f"{path}: job {job!r} has an empty needs list")
    return result


def _fold(lines: list[str]) -> str:
    result = ""
    for index, line in enumerate(lines):
        if index == 0:
            result = line
        elif not line or not lines[index - 1]:
            result += "\n" + line
        else:
            result += " " + line
    return result


def _block_scalar(
    step: list[str],
    index: int,
    marker: str,
    path: Path,
    job: str,
) -> str:
    content: list[str] = []
    for line in step[index + 1 :]:
        if line and not line.startswith("          "):
            break
        content.append(line[10:] if line else "")
    while content and not content[-1]:
        content.pop()
    if not content:
        raise GateError(f"{path}: job {job!r} step has an empty scalar block")
    value = _fold(content) if marker.startswith(">") else "\n".join(content)
    if marker in {"|", ">"}:
        value += "\n"
    return value


def _mapping(step: list[str], path: Path, job: str) -> dict[str, str]:
    result: dict[str, str] = {}
    first = STEP_START.fullmatch(step[0])
    if first is None:
        raise GateError(f"{path}: job {job!r} has a malformed step start")
    candidates: list[tuple[int, str]] = []
    if first.group("first"):
        candidates.append((0, "        " + first.group("first")))
    candidates.extend(
        (index, line)
        for index, line in enumerate(step[1:], start=1)
        if STEP_KEY.fullmatch(line)
    )
    for index, line in candidates:
        match = STEP_KEY.fullmatch(line)
        if match is None:
            raise GateError(f"{path}: job {job!r} has an unsupported step mapping")
        key = match.group("key")
        if key in result:
            raise GateError(f"{path}: job {job!r} step repeats key {key!r}")
        value = match.group("value") or ""
        if key == "run" and value in {"|", "|-", ">", ">-"}:
            value = _block_scalar(step, index, value, path, job)
        result[key] = value
    return result


def steps(block: list[str], path: Path, job: str) -> list[WorkflowStep]:
    """Parse every named or unnamed step in full ordered multiplicity."""

    headers = [index for index, line in enumerate(block) if line == "    steps:"]
    if len(headers) != 1:
        raise GateError(f"{path}: job {job!r} must contain exactly one steps list")
    start = headers[0] + 1
    end = len(block)
    for index in range(start, len(block)):
        line = block[index]
        if line.strip() and not line.startswith(("      ", "        ", "          ")):
            end = index
            break
    source = block[start:end]
    starts = [
        index for index, line in enumerate(source) if STEP_START.fullmatch(line)
    ]
    if not starts:
        raise GateError(f"{path}: job {job!r} has no steps")
    result: list[WorkflowStep] = []
    names: set[str] = set()
    for ordinal, item_start in enumerate(starts):
        item_end = starts[ordinal + 1] if ordinal + 1 < len(starts) else len(source)
        raw_block = source[item_start:item_end]
        mapping = _mapping(raw_block, path, job)
        name = mapping.get("name") or None
        if name is not None:
            if name in names:
                raise GateError(f"{path}: job {job!r} repeats step name {name!r}")
            names.add(name)
        kinds = [key for key in ("run", "uses") if mapping.get(key)]
        if len(kinds) != 1:
            raise GateError(
                f"{path}: job {job!r} step {ordinal} must contain exactly one "
                "nonempty run or uses"
            )
        kind = kinds[0]
        digest = hashlib.sha256(
            ("\n".join(raw_block) + "\n").encode("utf-8")
        ).hexdigest()
        result.append(
            WorkflowStep(
                ordinal=ordinal,
                name=name,
                kind=kind,
                command=mapping[kind],
                block_sha256=digest,
            )
        )
    return result
