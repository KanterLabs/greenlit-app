"""Fail-closed loading and ordering of retained Greenlit events."""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any

from parity_producer.common import (
    ProducerError,
    load_json_bytes,
    read_regular_file,
    require_integer,
    require_object,
    require_string,
)


MAX_JOURNAL_BYTES = 32 * 1024 * 1024
GROUPED_TYPES = (
    "run_started",
    "job_started",
    "step_started",
    "step_finished",
    "job_finished",
    "run_finished",
)
ALLOWED_TYPES = frozenset(
    {
        *GROUPED_TYPES,
        "preparation",
        "compatibility_finding",
        "job_skipped",
        "step_skipped",
        "log",
        "cache_summary",
    }
)


def load_grouped_journal(
    path: Path, run_id: str
) -> tuple[dict[str, list[dict[str, Any]]], list[dict[str, Any]]]:
    """Load one exact-run journal and retain semantic events in raw order."""
    records = _load_records(path)
    grouped = {event_type: [] for event_type in GROUPED_TYPES}
    logs: list[dict[str, Any]] = []
    findings: list[dict[str, Any]] = []
    last_timestamp = -1
    for expected_sequence, record in enumerate(records, 1):
        if record.get("schema_version") != 1:
            raise ProducerError("retained event journal has an unknown schema version")
        sequence = require_integer(record.get("sequence"), "event sequence", 1)
        if sequence != expected_sequence:
            raise ProducerError(
                f"retained event sequence is not contiguous at {sequence}"
            )
        if record.get("run_id") != run_id:
            raise ProducerError("retained event journal mixes run identities")
        timestamp = require_integer(record.get("timestamp_unix_ms"), "event timestamp")
        if timestamp < last_timestamp:
            raise ProducerError("retained event timestamps move backwards")
        last_timestamp = timestamp
        event_type = require_string(record.get("type"), "event type")
        if event_type not in ALLOWED_TYPES:
            raise ProducerError(f"retained event journal has unknown type {event_type!r}")
        if event_type in {"job_skipped", "step_skipped"}:
            raise ProducerError("canonical seed unexpectedly skipped selected work")
        if event_type == "log":
            logs.append(record)
        elif event_type == "compatibility_finding":
            findings.append(record)
        elif event_type in grouped:
            grouped[event_type].append(record)

    if records[0].get("type") != "run_started":
        raise ProducerError("retained run_started is not the first journal record")
    if records[-1].get("type") != "run_finished":
        raise ProducerError("retained run_finished is not the final journal record")
    for event_type, values in grouped.items():
        expected_count = 2 if event_type in {"step_started", "step_finished"} else 1
        if len(values) != expected_count:
            raise ProducerError(
                f"retained seed has {len(values)} {event_type} events; "
                f"expected {expected_count}"
            )
    _validate_compatibility_findings(findings)
    return grouped, logs


def validate_execution_order(
    grouped: dict[str, list[dict[str, Any]]],
    logs: list[dict[str, Any]],
    instance_id: str,
) -> list[str]:
    """Require the observed job, steps, and marker logs to be strictly nested."""
    starts = grouped["step_started"]
    finishes = grouped["step_finished"]
    ordered = [
        grouped["run_started"][0],
        grouped["job_started"][0],
        starts[0],
        finishes[0],
        starts[1],
        finishes[1],
        grouped["job_finished"][0],
        grouped["run_finished"][0],
    ]
    sequences = [
        require_integer(record.get("sequence"), "semantic event sequence", 1)
        for record in ordered
    ]
    if sequences != sorted(sequences) or len(sequences) != len(set(sequences)):
        raise ProducerError("retained semantic lifecycle is not strictly ordered")

    event_ids = [
        require_string(record.get("event_id"), "retained step event identity")
        for record in starts
    ]
    if len(set(event_ids)) != 2:
        raise ProducerError("retained authored steps do not have distinct event identities")
    event_bounds = {
        event_id: (
            require_integer(start.get("sequence"), "retained step start sequence", 1),
            require_integer(finish.get("sequence"), "retained step finish sequence", 1),
        )
        for event_id, start, finish in zip(
            event_ids, starts, finishes, strict=True
        )
    }
    for record in [
        grouped["job_started"][0],
        grouped["job_finished"][0],
        *starts,
        *finishes,
    ]:
        if record.get("instance_id") != instance_id:
            raise ProducerError("retained lifecycle mixes job instance identities")

    lines: list[str] = []
    emit_identity = "PARITY_IDENTITY job=shell step=emit"
    for record in logs:
        if (
            record.get("job_id") != "shell"
            or record.get("instance_id") != instance_id
            or record.get("partial") is not False
        ):
            raise ProducerError("retained marker log is not a complete shell-job record")
        text = require_string(record.get("text"), "retained log text")
        if not text.startswith("PARITY_"):
            raise ProducerError("canonical seed emitted an unexpected non-marker log")
        step_event_id = record.get("step_event_id")
        expected_event_id = event_ids[0] if text == emit_identity else event_ids[1]
        if step_event_id != expected_event_id:
            raise ProducerError("retained parity marker is bound to the wrong step")
        start_sequence, finish_sequence = event_bounds[expected_event_id]
        log_sequence = require_integer(
            record.get("sequence"), "retained marker log sequence", 1
        )
        if not start_sequence < log_sequence < finish_sequence:
            raise ProducerError(
                "retained parity marker is not nested inside its authored step"
            )
        lines.append(text)
    return lines


def _load_records(path: Path) -> list[dict[str, Any]]:
    raw = read_regular_file(
        path,
        "retained event journal",
        MAX_JOURNAL_BYTES,
        required_mode=0o600,
        required_owner=os.geteuid(),
        required_links=1,
    )
    if not raw.endswith(b"\n"):
        raise ProducerError("retained event journal ends with an incomplete record")
    records: list[dict[str, Any]] = []
    for line_number, line in enumerate(raw.splitlines(), 1):
        if not line:
            raise ProducerError(
                f"retained event journal has an empty record at line {line_number}"
            )
        records.append(
            require_object(
                load_json_bytes(line, f"retained event line {line_number}"),
                f"retained event line {line_number}",
            )
        )
    if not records:
        raise ProducerError("retained event journal is empty")
    return records


def _validate_compatibility_findings(records: list[dict[str, Any]]) -> None:
    expected = {
        ("execution.shell", "jobs.shell.steps[0]", "degraded"),
        ("execution.shell", "jobs.shell.steps[1]", "degraded"),
        ("network.external_uncaptured", "run", "degraded"),
        ("runner.profile_self_hosted", "run", "degraded"),
        ("runner.user_root", "run", "degraded"),
        ("runtime.host_kernel", "run", "degraded"),
        ("stabilization.capability-registry.complete", "run", "supported"),
    }
    observed: set[tuple[str, str, str]] = set()
    for record in records:
        identity = (
            require_string(record.get("code"), "compatibility finding code"),
            require_string(record.get("scope"), "compatibility finding scope"),
            require_string(
                record.get("disposition"), "compatibility finding disposition"
            ),
        )
        require_string(record.get("reason"), "compatibility finding reason")
        if identity in observed:
            raise ProducerError("retained compatibility findings contain a duplicate")
        observed.add(identity)
    if observed != expected:
        raise ProducerError("retained compatibility findings differ from canonical seed")
