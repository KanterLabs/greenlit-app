"""Projection of Greenlit's retained event and result evidence."""

from __future__ import annotations

import os
import stat
from pathlib import Path
from typing import Any

from parity_producer.common import (
    COMMIT,
    MAX_JSON_BYTES,
    RUNNER,
    WORKFLOW_PATH,
    ProducerError,
    load_json_bytes,
    read_regular_file,
    require_fields,
    require_integer,
    require_object,
    require_string,
    sha256_bytes,
    strip_sha256_prefix,
    timestamp_from_unix_ms,
)
from parity_producer.capture import Production, authority_envelope
from parity_producer.contract import (
    EXPECTED_JOB_NAME,
    EXPECTED_STEPS,
    assemble_observation,
)
from parity_producer.markers import parse_markers
from parity_producer.retained_journal import (
    load_grouped_journal,
    validate_execution_order,
)


_RETAINED_CERTIFYING_WITNESS = object()


def project_local_evidence(
    *,
    run_directory: Path,
    binary_sha256: str,
    repository_name: str,
    expected_commit: str,
    expected_workflow_sha256: str,
) -> Production:
    """Project one retained Greenlit run into the canonical seed contract."""
    run_lock_bytes = read_regular_file(
        run_directory / "run-lock.json",
        "retained run lock",
        MAX_JSON_BYTES,
        required_mode=0o600,
        required_owner=os.geteuid(),
        required_links=1,
    )
    result_bytes = read_regular_file(
        run_directory / "result.json",
        "retained result",
        MAX_JSON_BYTES,
        required_mode=0o600,
        required_owner=os.geteuid(),
        required_links=1,
    )
    run_lock = require_object(
        load_json_bytes(run_lock_bytes, "retained run lock"),
        "retained run lock",
    )
    result = require_object(
        load_json_bytes(result_bytes, "retained result"),
        "retained result",
    )
    source = require_object(run_lock.get("source"), "retained run lock source")
    require_fields(
        source,
        "retained run lock source",
        {"commit", "snapshot_digest", "dirty", "workflow_path", "workflow_digest"},
        allow_extra=False,
    )
    commit = require_string(source["commit"], "retained source commit")
    if commit != expected_commit or COMMIT.fullmatch(commit) is None:
        raise ProducerError("retained source commit does not equal the clean checkout HEAD")
    if source["dirty"] is not False:
        raise ProducerError("retained source is dirty and cannot enter same-commit parity")
    if source["workflow_path"] != WORKFLOW_PATH:
        raise ProducerError("retained run selected a different workflow")
    workflow_sha256 = strip_sha256_prefix(
        source["workflow_digest"], "retained workflow digest"
    )
    strip_sha256_prefix(source["snapshot_digest"], "retained snapshot digest")
    if workflow_sha256 != expected_workflow_sha256:
        raise ProducerError("retained workflow bytes do not equal the checkout workflow")
    frozen_workflow = run_directory / "source" / WORKFLOW_PATH
    _require_private_source_directories(run_directory)
    frozen_workflow_bytes = read_regular_file(
        frozen_workflow,
        "frozen parity workflow",
        1024 * 1024,
        required_mode=0o600,
        required_owner=os.geteuid(),
        required_links=1,
    )
    if sha256_bytes(frozen_workflow_bytes) != workflow_sha256:
        raise ProducerError("frozen parity workflow does not match its retained identity")

    require_fields(
        result,
        "retained result",
        {"schema_version", "conclusion", "compatibility", "assurance", "reasons"},
        allow_extra=False,
    )
    if result["schema_version"] != 1:
        raise ProducerError("retained result has an unknown schema version")
    reasons = result["reasons"]
    if (
        not isinstance(reasons, list)
        or not reasons
        or any(not isinstance(reason, str) or not reason for reason in reasons)
    ):
        raise ProducerError("retained result reasons are incomplete")
    if result.get("conclusion") != "passed":
        raise ProducerError("retained local seed conclusion is not passed")
    if result.get("compatibility") != "degraded" or result.get("assurance") != "none":
        raise ProducerError(
            "retained local seed did not record degraded compatibility and assurance none"
        )

    if (
        run_lock.get("schema_version") != 1
        or run_lock.get("event") != "push"
        or run_lock.get("selected_job") != "shell"
        or run_lock.get("offline") is not False
        or run_lock.get("hermetic") is not False
    ):
        raise ProducerError("retained run lock does not identify the canonical seed run")
    runner_map = require_object(run_lock.get("runners"), "retained runner identities")
    if set(runner_map) != {"shell"}:
        raise ProducerError("retained local seed must contain exactly the shell runner")
    runner = require_object(runner_map["shell"], "retained shell runner")
    resolved_runner = require_string(
        runner.get("resolved_label"), "retained resolved runner"
    )
    require_string(runner.get("provider"), "retained runner provider")
    if (
        runner.get("requested_label") != RUNNER
        or resolved_runner != "ubuntu-24.04"
        or runner.get("os") != "linux"
        or runner.get("architecture") != "amd64"
    ):
        raise ProducerError(f"retained local seed did not use runner {RUNNER!r}")

    run_id = run_directory.name
    grouped, log_records = load_grouped_journal(
        run_directory / "events.ndjson", run_id
    )
    projection = _build_projection(grouped, log_records)
    markers = parse_markers(projection["log_lines"], "retained local journal")
    if markers.probe_sha256 != sha256_bytes(b"greenlit\n"):
        raise ProducerError("parity seed probe digest does not match deterministic bytes")

    producer = {
        "role": "greenlit-release",
        "repository": repository_name,
        "runner": RUNNER,
        "run_id": run_id,
        "run_attempt": 1,
        "run_url": None,
        "binary_sha256": binary_sha256,
    }
    contract_source = {
        "repository": repository_name,
        "commit": commit,
        "workflow_path": WORKFLOW_PATH,
        "workflow_sha256": workflow_sha256,
    }
    observation = assemble_observation(producer, contract_source, projection, markers)
    authority = authority_envelope(
        observation,
        "greenlit-release",
        {
            "event": "push",
            "binary_sha256": binary_sha256,
            "frozen_workflow_sha256": workflow_sha256,
            "result_conclusion": "passed",
            "result_compatibility": "degraded",
            "result_assurance": "none",
            "journal_lifecycle": observation["lifecycle"],
            "source_commit": commit,
            "build_source_commit": expected_commit,
            "requested_runner": runner.get("requested_label"),
            "resolved_runner": resolved_runner,
            "reported_durations": projection["reported_durations"],
        },
    )
    return Production(
        observation=observation,
        authority=authority,
        _certifying_witness=_RETAINED_CERTIFYING_WITNESS,
    )


def _require_private_source_directories(run_directory: Path) -> None:
    current = run_directory
    for part in ("source", ".github", "workflows"):
        current /= part
        try:
            metadata = current.lstat()
        except OSError as error:
            raise ProducerError(
                f"cannot inspect retained source directory {current}: {error}"
            ) from error
        if (
            not stat.S_ISDIR(metadata.st_mode)
            or stat.S_IMODE(metadata.st_mode) != 0o700
            or metadata.st_uid != os.geteuid()
        ):
            raise ProducerError(
                "retained source directory must be current-user mode 0700: "
                f"{current}"
            )


def _build_projection(
    grouped: dict[str, list[dict[str, Any]]],
    log_records: list[dict[str, Any]],
) -> dict[str, Any]:
    run_started = grouped["run_started"][0]
    run_finished = grouped["run_finished"][0]
    job_started = grouped["job_started"][0]
    job_finished = grouped["job_finished"][0]
    instance_id = require_string(
        job_started.get("instance_id"), "retained job instance identity"
    )
    if (
        job_started.get("job_id") != "shell"
        or job_finished.get("job_id") != "shell"
        or job_finished.get("instance_id") != instance_id
    ):
        raise ProducerError("retained seed job lifecycle does not identify shell exactly")
    job_name = require_string(job_started.get("display"), "retained job display")
    if job_name != EXPECTED_JOB_NAME or job_finished.get("display") != job_name:
        raise ProducerError("retained seed job name changed")
    if job_finished.get("conclusion") != "success":
        raise ProducerError("retained shell job did not succeed")

    starts = grouped["step_started"]
    finishes = grouped["step_finished"]
    log_lines = validate_execution_order(grouped, log_records, instance_id)
    steps: list[dict[str, Any]] = []
    lifecycle: list[dict[str, Any]] = [
        _lifecycle("run-started", 1, "run_started", run_started, None, None),
        _lifecycle("job-shell-started", 2, "job_started", job_started, "shell", None),
    ]
    sequence = 3
    for index, (step_id, step_name) in enumerate(EXPECTED_STEPS):
        started = starts[index]
        finished = finishes[index]
        if (
            started.get("job_id") != "shell"
            or finished.get("job_id") != "shell"
            or started.get("step_id") != step_id
            or finished.get("step_id") != step_id
            or started.get("label") != step_name
            or finished.get("label") != step_name
            or started.get("event_id") != finished.get("event_id")
            or started.get("index") != index
            or finished.get("index") != index
        ):
            raise ProducerError(f"retained lifecycle does not identify step {step_id!r}")
        outcome = require_string(finished.get("outcome"), f"{step_id} outcome")
        conclusion = require_string(finished.get("conclusion"), f"{step_id} conclusion")
        if outcome != "success" or conclusion != "success":
            raise ProducerError(f"retained seed step {step_id!r} did not succeed")
        steps.append(
            {
                "id": step_id,
                "name": step_name,
                "outcome": outcome,
                "conclusion": conclusion,
                "duration_ms": _event_duration(
                    started, finished, f"{step_id} lifecycle duration"
                ),
            }
        )
        lifecycle.extend(
            [
                _lifecycle(
                    f"step-{step_id}-started",
                    sequence,
                    "step_started",
                    started,
                    "shell",
                    step_id,
                ),
                _lifecycle(
                    f"step-{step_id}-completed",
                    sequence + 1,
                    "step_completed",
                    finished,
                    "shell",
                    step_id,
                ),
            ]
        )
        sequence += 2
    lifecycle.extend(
        [
            _lifecycle(
                "job-shell-completed",
                sequence,
                "job_completed",
                job_finished,
                "shell",
                None,
            ),
            _lifecycle(
                "run-completed",
                sequence + 1,
                "run_completed",
                run_finished,
                None,
                None,
            ),
        ]
    )
    if run_finished.get("conclusion") != "Passed":
        raise ProducerError("retained terminal event did not record Passed")
    return {
        "run_started_at": timestamp_from_unix_ms(
            run_started.get("timestamp_unix_ms"), "run start timestamp"
        ),
        "run_completed_at": timestamp_from_unix_ms(
            run_finished.get("timestamp_unix_ms"), "run completion timestamp"
        ),
        "run_duration_ms": _event_duration(
            run_started, run_finished, "run lifecycle duration"
        ),
        "run_conclusion": "success",
        "job_name": job_name,
        "job_conclusion": "success",
        "job_duration_ms": _event_duration(
            job_started, job_finished, "job lifecycle duration"
        ),
        "steps": steps,
        "lifecycle": lifecycle,
        "log_lines": log_lines,
        "reported_durations": {
            "run_elapsed_ms": require_integer(
                run_finished.get("elapsed_ms"), "reported run elapsed duration"
            ),
            "job_duration_ms": require_integer(
                job_finished.get("duration_ms"), "reported job duration"
            ),
            "step_duration_ms": [
                require_integer(value.get("duration_ms"), "reported step duration")
                for value in finishes
            ],
        },
    }


def _lifecycle(
    identity: str,
    sequence: int,
    kind: str,
    record: dict[str, Any],
    job_id: str | None,
    step_id: str | None,
) -> dict[str, Any]:
    return {
        "id": identity,
        "sequence": sequence,
        "kind": kind,
        "timestamp": timestamp_from_unix_ms(
            record.get("timestamp_unix_ms"), f"{identity} timestamp"
        ),
        "job_id": job_id,
        "step_id": step_id,
    }


def _event_duration(
    started: dict[str, Any],
    completed: dict[str, Any],
    source: str,
) -> int:
    start = require_integer(started.get("timestamp_unix_ms"), f"{source} start")
    end = require_integer(completed.get("timestamp_unix_ms"), f"{source} completion")
    if end < start:
        raise ProducerError(f"{source} completes before it starts")
    return end - start
