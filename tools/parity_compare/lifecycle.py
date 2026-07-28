"""Lifecycle vocabulary, references, ordering, and result consistency."""

from __future__ import annotations

import datetime as dt
from decimal import Decimal, DecimalException
from typing import Any

from . import ContractError
from .values import (
    elapsed_milliseconds,
    require_duration,
    require_identifier,
    require_integer,
    require_nullable_string,
    require_object,
    require_string,
    require_timestamp,
)


LIFECYCLE_KINDS = frozenset(
    {
        "run_started",
        "job_started",
        "job_skipped",
        "step_started",
        "step_skipped",
        "step_completed",
        "job_completed",
        "run_completed",
    }
)
_TIME_TOLERANCE = dt.timedelta(seconds=1)
_DURATION_TOLERANCE = Decimal(1000)
_SUCCESS_LIKE = frozenset({"success", "neutral", "skipped"})


def _nullable_identifier(value: Any, path: str) -> str | None:
    text = require_nullable_string(value, path)
    return None if text is None else require_identifier(text, path)


def validate_lifecycle_record(value: Any, path: str) -> str:
    """Validate one exact lifecycle record and return its identity."""
    record = require_object(
        value, path, {"id", "sequence", "kind", "timestamp", "job_id", "step_id"}
    )
    identity = require_identifier(record["id"], f"{path}.id")
    require_integer(record["sequence"], f"{path}.sequence", 1)
    kind = require_string(record["kind"], f"{path}.kind")
    if kind not in LIFECYCLE_KINDS:
        raise ContractError(f"{path}.kind: unknown lifecycle event kind {kind!r}")
    require_timestamp(record["timestamp"], f"{path}.timestamp")
    job_id = _nullable_identifier(record["job_id"], f"{path}.job_id")
    step_id = _nullable_identifier(record["step_id"], f"{path}.step_id")
    if step_id is not None and job_id is None:
        raise ContractError(f"{path}.job_id: required when step_id is present")
    return identity


def validate_reported_duration(
    reported: Any, started: dt.datetime, completed: dt.datetime, path: str
) -> None:
    """Require a reported duration to agree with timestamp evidence."""
    duration = require_duration(reported, path)
    try:
        disagrees = (
            abs(duration - elapsed_milliseconds(started, completed))
            > _DURATION_TOLERANCE
        )
    except DecimalException as error:
        raise ContractError(
            f"{path}: duration magnitude cannot be compared safely"
        ) from error
    if disagrees:
        raise ContractError(f"{path}: disagrees with lifecycle by more than 1000 ms")


def _transition_choice(
    positions: dict[tuple[str, str | None, str | None], int],
    prefix: str,
    job_id: str,
    step_id: str | None,
    path: str,
) -> tuple[str, tuple[str, str | None, str | None], int, int | None]:
    start = (f"{prefix}_started", job_id, step_id)
    complete = (f"{prefix}_completed", job_id, step_id)
    skip = (f"{prefix}_skipped", job_id, step_id)
    paired = start in positions or complete in positions
    skipped = skip in positions
    if paired == skipped or (paired and not (start in positions and complete in positions)):
        raise ContractError(f"{path}: requires exactly one start/completion pair or skip")
    if skipped:
        return "skip", skip, positions[skip], None
    return "pair", start, positions[start], positions[complete]


def _index_events(
    root: dict[str, Any],
    started: dt.datetime,
    completed: dt.datetime,
) -> tuple[
    dict[tuple[str, str | None, str | None], int],
    list[dt.datetime],
]:
    positions: dict[tuple[str, str | None, str | None], int] = {}
    timestamps: list[dt.datetime] = []
    job_steps = {
        job["id"]: {step["id"] for step in job["steps"]} for job in root["jobs"]
    }
    for index, event in enumerate(root["lifecycle"]):
        path, kind = f"$.lifecycle[{index}]", event["kind"]
        job_id, step_id = event["job_id"], event["step_id"]
        if kind.startswith("run_") and (job_id is not None or step_id is not None):
            raise ContractError(f"{path}: run transition cannot reference job or step")
        if kind.startswith("job_") and (job_id is None or step_id is not None):
            raise ContractError(f"{path}: job transition requires only job_id")
        if kind.startswith("step_") and (job_id is None or step_id is None):
            raise ContractError(f"{path}: step transition requires job_id and step_id")
        if job_id is not None and job_id not in job_steps:
            raise ContractError(f"{path}.job_id: unknown job identity {job_id!r}")
        if step_id is not None and step_id not in job_steps[job_id]:
            raise ContractError(f"{path}.step_id: unknown step identity {step_id!r}")
        key = (kind, job_id, step_id)
        if key in positions:
            raise ContractError(f"{path}: duplicate lifecycle transition {key!r}")
        positions[key] = index
        timestamp = require_timestamp(event["timestamp"], f"{path}.timestamp")
        if timestamp < started - _TIME_TOLERANCE or timestamp > completed + _TIME_TOLERANCE:
            raise ContractError(f"{path}.timestamp: event lies outside run bounds")
        timestamps.append(timestamp)
    if timestamps != sorted(timestamps):
        raise ContractError("$.lifecycle: timestamps must be nondecreasing")
    return positions, timestamps


def _validate_step_transitions(
    job: dict[str, Any],
    job_index: int,
    events: list[dict[str, Any]],
    mode: str,
    job_start: int,
    job_end: int | None,
    positions: dict[tuple[str, str | None, str | None], int],
    timestamps: list[dt.datetime],
    consumed: set[tuple[str, str | None, str | None]],
) -> None:
    job_path, job_id = f"$.jobs[{job_index}]", job["id"]
    previous = -1 if mode == "skip" else job_start
    authored: list[str] = []
    for step_index, step in enumerate(job["steps"]):
        step_path, step_id = f"{job_path}.steps[{step_index}]", step["id"]
        step_mode, step_key, step_start, step_end = _transition_choice(
            positions, "step", job_id, step_id, step_path
        )
        consumed.add(step_key)
        if step_end is not None:
            consumed.add(("step_completed", job_id, step_id))
        terminal = step_start if step_end is None else step_end
        upper = job_start if mode == "skip" else job_end
        if upper is None or not previous < step_start <= terminal < upper:
            raise ContractError(f"{step_path}: lifecycle is not in authored order")
        if mode == "skip" and step_mode != "skip":
            raise ContractError(f"{step_path}: skipped job requires skipped steps")
        if step_mode == "skip":
            if (
                step["outcome"] != "skipped"
                or step["conclusion"] != "skipped"
                or require_duration(step["duration_ms"], f"{step_path}.duration_ms")
            ):
                raise ContractError(f"{step_path}: skipped step requires skipped/zero result")
        else:
            if step["conclusion"] == "skipped":
                raise ContractError(f"{step_path}.conclusion: started step cannot be skipped")
            validate_reported_duration(
                step["duration_ms"],
                timestamps[step_start],
                timestamps[step_end],
                f"{step_path}.duration_ms",
            )
        previous = terminal
        authored.append(step_id)
    observed = [
        event["step_id"]
        for event in events
        if event["job_id"] == job_id
        and event["kind"] in {"step_started", "step_skipped"}
    ]
    if observed != authored:
        raise ContractError(f"{job_path}.steps: lifecycle disagrees with authored order")


def _validate_parent_conclusion(
    conclusion: str,
    child_conclusions: list[str],
    path: str,
) -> None:
    if conclusion == "skipped":
        if any(child != "skipped" for child in child_conclusions):
            raise ContractError(f"{path}: skipped result contains an executed child")
        return
    if conclusion in _SUCCESS_LIKE:
        if any(child not in _SUCCESS_LIKE for child in child_conclusions):
            raise ContractError(f"{path}: contradicts child conclusions")
        return
    if all(child in _SUCCESS_LIKE for child in child_conclusions):
        raise ContractError(f"{path}: non-success result has only successful children")


def validate_lifecycle_semantics(
    root: dict[str, Any], started: dt.datetime, completed: dt.datetime
) -> None:
    """Validate complete lifecycle pairs/skips, references, order, and results."""
    events = root["lifecycle"]
    positions, timestamps = _index_events(root, started, completed)
    run_start, run_end = ("run_started", None, None), ("run_completed", None, None)
    if events[0]["kind"] != "run_started" or positions.get(run_start) != 0:
        raise ContractError("$.lifecycle[0]: run_started must be first")
    if events[-1]["kind"] != "run_completed" or positions.get(run_end) != len(events) - 1:
        raise ContractError("$.lifecycle: run_completed must be final")
    if (
        abs(timestamps[0] - started) > _TIME_TOLERANCE
        or abs(timestamps[-1] - completed) > _TIME_TOLERANCE
    ):
        raise ContractError("$.lifecycle: run transitions disagree with run timestamps")
    if (
        abs(
            elapsed_milliseconds(started, completed)
            - elapsed_milliseconds(timestamps[0], timestamps[-1])
        )
        > _DURATION_TOLERANCE
    ):
        raise ContractError(
            "$.lifecycle: run transition span disagrees with run timestamps"
        )
    validate_reported_duration(
        root["run"]["duration_ms"],
        timestamps[0],
        timestamps[-1],
        "$.run.duration_ms",
    )
    consumed = {run_start, run_end}
    for job_index, job in enumerate(root["jobs"]):
        job_path, job_id = f"$.jobs[{job_index}]", job["id"]
        mode, key, job_start, job_end = _transition_choice(
            positions, "job", job_id, None, job_path
        )
        consumed.add(key)
        if job_end is not None:
            consumed.add(("job_completed", job_id, None))
        if mode == "skip":
            if job["conclusion"] != "skipped" or require_duration(
                job["duration_ms"], f"{job_path}.duration_ms"
            ):
                raise ContractError(f"{job_path}: skipped job requires skipped/zero result")
        elif job["conclusion"] == "skipped" or job_start >= job_end:
            raise ContractError(f"{job_path}: invalid started job result or ordering")
        _validate_step_transitions(
            job,
            job_index,
            events,
            mode,
            job_start,
            job_end,
            positions,
            timestamps,
            consumed,
        )
        if mode == "pair":
            validate_reported_duration(
                job["duration_ms"],
                timestamps[job_start],
                timestamps[job_end],
                f"{job_path}.duration_ms",
            )
        _validate_parent_conclusion(
            job["conclusion"],
            [step["conclusion"] for step in job["steps"]],
            f"{job_path}.conclusion",
        )
    if set(positions) != consumed:
        raise ContractError("$.lifecycle: contains an undeclared transition")
    _validate_parent_conclusion(
        root["run"]["conclusion"],
        [job["conclusion"] for job in root["jobs"]],
        "$.run.conclusion",
    )


__all__ = [
    "LIFECYCLE_KINDS",
    "validate_lifecycle_record",
    "validate_lifecycle_semantics",
    "validate_reported_duration",
]
