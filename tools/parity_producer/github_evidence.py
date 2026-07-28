"""Projection of exact GitHub Actions API and log evidence."""

from __future__ import annotations

import datetime as dt
from typing import Any

from parity_producer.common import (
    COMMIT,
    RUNNER,
    WORKFLOW_PATH,
    ProducerError,
    duration_ms,
    format_timestamp,
    parse_timestamp,
    require_fields,
    require_integer,
    require_object,
    require_string,
    sha256_bytes,
)
from parity_producer.capture import Production, authority_envelope
from parity_producer.contract import (
    EXPECTED_JOB_NAME,
    EXPECTED_STEPS,
    assemble_observation,
)
from parity_producer.github_inputs import content_bytes, job_log_lines
from parity_producer.github_job import exact_job_id
from parity_producer.markers import parse_markers


EXPECTED_API_STEPS = (
    "Set up job",
    EXPECTED_STEPS[0][1],
    EXPECTED_STEPS[1][1],
    "Complete job",
)


def project_github_evidence(
    *,
    repository: str,
    requested_run_id: int,
    run: dict[str, Any],
    jobs_response: dict[str, Any],
    content_response: dict[str, Any],
    job_log: bytes,
    trusted_source_commit: str,
) -> Production:
    """Project exact API documents and the corresponding workflow-job log."""
    require_fields(
        run,
        "GitHub run",
        {
            "id",
            "name",
            "event",
            "status",
            "conclusion",
            "head_sha",
            "path",
            "run_attempt",
            "run_started_at",
            "updated_at",
            "html_url",
            "repository",
        },
        allow_extra=True,
    )
    api_run_id = require_integer(run["id"], "GitHub run id", 1)
    if api_run_id != requested_run_id:
        raise ProducerError("GitHub run API identity does not equal the requested run")
    run_id = str(api_run_id)
    commit = require_string(run["head_sha"], "GitHub run head SHA")
    if COMMIT.fullmatch(commit) is None:
        raise ProducerError("GitHub run head SHA is not a full lowercase commit")
    if commit != trusted_source_commit:
        raise ProducerError("GitHub run head SHA differs from trusted source commit")
    repository_object = require_object(run["repository"], "GitHub run repository")
    api_repository = require_string(
        repository_object.get("full_name"), "GitHub repository full name"
    )
    if api_repository != repository:
        raise ProducerError("GitHub run belongs to a different repository")
    workflow_path = require_string(run["path"], "GitHub run workflow path").split("@", 1)[0]
    if workflow_path != WORKFLOW_PATH:
        raise ProducerError("GitHub run executed a different workflow path")
    if (
        run["name"] != "Parity seed"
        or run["event"] != "push"
        or run["status"] != "completed"
        or run["conclusion"] != "success"
    ):
        raise ProducerError("GitHub seed run is not the completed successful push run")
    attempt = require_integer(run["run_attempt"], "GitHub run attempt", 1)
    canonical_url = f"https://github.com/{repository}/actions/runs/{run_id}"
    if run["html_url"] != canonical_url:
        raise ProducerError("GitHub run URL is not canonical for the source repository")
    run_started = parse_timestamp(run["run_started_at"], "GitHub run start")
    run_completed = parse_timestamp(run["updated_at"], "GitHub run completion")

    projection = _project_jobs(
        repository=repository,
        run_id=api_run_id,
        jobs_response=jobs_response,
        commit=commit,
        attempt=attempt,
        run_started=run_started,
        run_completed=run_completed,
    )
    workflow_sha256 = sha256_bytes(content_bytes(content_response))
    markers = parse_markers(job_log_lines(job_log), "GitHub Actions job log")
    if markers.probe_sha256 != sha256_bytes(b"greenlit\n"):
        raise ProducerError("GitHub parity probe digest does not match deterministic bytes")
    producer = {
        "role": "github-actions",
        "repository": repository,
        "runner": RUNNER,
        "run_id": run_id,
        "run_attempt": attempt,
        "run_url": canonical_url,
        "binary_sha256": None,
    }
    source = {
        "repository": repository,
        "commit": commit,
        "workflow_path": WORKFLOW_PATH,
        "workflow_sha256": workflow_sha256,
    }
    observation = assemble_observation(producer, source, projection, markers)
    authority = authority_envelope(
        observation,
        "github-actions",
        {
            "event": "push",
            "head_sha": commit,
            "workflow_sha256": workflow_sha256,
            "run_attempt": attempt,
            "run_url": canonical_url,
            "job_name": projection["job_name"],
            "job_conclusion": projection["job_conclusion"],
            "step_records": projection["steps"],
            "lifecycle_records": projection["lifecycle"],
            "log_marker_identities": [
                {"job": job, "step": step}
                for job, step in markers.identities
            ],
        },
    )
    return Production(observation=observation, authority=authority)


def _project_jobs(
    *,
    repository: str,
    run_id: int,
    jobs_response: dict[str, Any],
    commit: str,
    attempt: int,
    run_started: dt.datetime,
    run_completed: dt.datetime,
) -> dict[str, Any]:
    exact_job_id(
        repository=repository,
        run_id=run_id,
        attempt=attempt,
        trusted_source_commit=commit,
        jobs_response=jobs_response,
    )
    jobs = jobs_response["jobs"]
    job = require_object(jobs[0], "GitHub shell job")
    require_fields(
        job,
        "GitHub shell job",
        {
            "name",
            "id",
            "run_id",
            "url",
            "status",
            "conclusion",
            "head_sha",
            "run_attempt",
            "started_at",
            "completed_at",
            "labels",
            "steps",
        },
        allow_extra=True,
    )
    if (
        job["name"] != EXPECTED_JOB_NAME
        or job["status"] != "completed"
        or job["conclusion"] != "success"
        or job["head_sha"] != commit
        or job["run_attempt"] != attempt
    ):
        raise ProducerError("GitHub shell job identity or successful conclusion differs")
    labels = job["labels"]
    if not isinstance(labels, list) or not all(isinstance(value, str) for value in labels):
        raise ProducerError("GitHub shell job labels are malformed")
    if RUNNER not in labels:
        raise ProducerError(f"GitHub shell job did not run on {RUNNER!r}")
    job_started = parse_timestamp(job["started_at"], "GitHub job start")
    job_completed = parse_timestamp(job["completed_at"], "GitHub job completion")
    authored = _authored_steps(job["steps"], job_started, job_completed)
    if not run_started <= job_started <= job_completed <= run_completed:
        raise ProducerError("GitHub run and job timestamps are not strictly nested")

    steps: list[dict[str, Any]] = []
    authored_times: list[tuple[dt.datetime, dt.datetime]] = []
    lifecycle = [
        _lifecycle("run-started", 1, "run_started", run_started, None, None),
        _lifecycle("job-shell-started", 2, "job_started", job_started, "shell", None),
    ]
    sequence = 3
    for step, identity, name in authored:
        if step["status"] != "completed" or step["conclusion"] != "success":
            raise ProducerError(f"GitHub seed step {identity!r} did not succeed")
        started = parse_timestamp(step["started_at"], f"GitHub {identity} start")
        completed = parse_timestamp(step["completed_at"], f"GitHub {identity} completion")
        authored_times.append((started, completed))
        conclusion = require_string(
            step["conclusion"], f"GitHub {identity} conclusion"
        )
        steps.append(
            {
                "id": identity,
                "name": name,
                "outcome": conclusion,
                "conclusion": conclusion,
                "duration_ms": duration_ms(
                    started, completed, f"GitHub {identity} duration"
                ),
            }
        )
        lifecycle.extend(
            [
                _lifecycle(
                    f"step-{identity}-started",
                    sequence,
                    "step_started",
                    started,
                    "shell",
                    identity,
                ),
                _lifecycle(
                    f"step-{identity}-completed",
                    sequence + 1,
                    "step_completed",
                    completed,
                    "shell",
                    identity,
                ),
            ]
        )
        sequence += 2
    if not (
        job_started
        <= authored_times[0][0]
        <= authored_times[0][1]
        <= authored_times[1][0]
        <= authored_times[1][1]
        <= job_completed
    ):
        raise ProducerError("GitHub authored-step timestamps are not strictly ordered")
    lifecycle.extend(
        [
            _lifecycle(
                "job-shell-completed",
                sequence,
                "job_completed",
                job_completed,
                "shell",
                None,
            ),
            _lifecycle(
                "run-completed",
                sequence + 1,
                "run_completed",
                run_completed,
                None,
                None,
            ),
        ]
    )
    return {
        "run_started_at": format_timestamp(run_started),
        "run_completed_at": format_timestamp(run_completed),
        "run_duration_ms": duration_ms(
            run_started, run_completed, "GitHub run duration"
        ),
        "run_conclusion": "success",
        "job_name": EXPECTED_JOB_NAME,
        "job_conclusion": "success",
        "job_duration_ms": duration_ms(
            job_started, job_completed, "GitHub job duration"
        ),
        "steps": steps,
        "lifecycle": lifecycle,
        "log_lines": [],
    }


def _authored_steps(
    value: Any,
    job_started: dt.datetime,
    job_completed: dt.datetime,
) -> list[tuple[dict[str, Any], str, str]]:
    if not isinstance(value, list):
        raise ProducerError("GitHub job steps must be an array")
    authored: list[tuple[dict[str, Any], str, str]] = []
    numbers: list[int] = []
    previous_completed: dt.datetime | None = None
    expected_by_name = {name: identity for identity, name in EXPECTED_STEPS}
    for index, raw_step in enumerate(value):
        step = require_object(raw_step, f"GitHub job step {index}")
        require_fields(
            step,
            f"GitHub job step {index}",
            {
                "name",
                "status",
                "conclusion",
                "number",
                "started_at",
                "completed_at",
            },
            allow_extra=True,
        )
        name = require_string(step["name"], f"GitHub job step {index} name")
        numbers.append(
            require_integer(step["number"], f"GitHub job step {index} number", 1)
        )
        if step["status"] != "completed" or step["conclusion"] != "success":
            raise ProducerError(f"GitHub job step {name!r} did not complete successfully")
        started = parse_timestamp(
            step["started_at"], f"GitHub job step {index} start"
        )
        completed = parse_timestamp(
            step["completed_at"], f"GitHub job step {index} completion"
        )
        if completed < started:
            raise ProducerError(f"GitHub job step {name!r} completes before it starts")
        if previous_completed is not None and started < previous_completed:
            raise ProducerError("GitHub job API steps overlap or move backwards")
        previous_completed = completed
        if name in expected_by_name:
            authored.append((step, expected_by_name[name], name))
    names = [
        require_string(
            require_object(raw_step, f"GitHub job step {index}").get("name"),
            f"GitHub job step {index} name",
        )
        for index, raw_step in enumerate(value)
    ]
    if tuple(names) != EXPECTED_API_STEPS or numbers != [1, 2, 3, 4]:
        raise ProducerError("GitHub job API step sequence differs from the seed")
    if not (
        job_started
        <= parse_timestamp(value[0]["started_at"], "GitHub setup step start")
        and parse_timestamp(
            value[-1]["completed_at"], "GitHub completion step completion"
        )
        <= job_completed
    ):
        raise ProducerError("GitHub API step sequence extends outside the shell job")
    if [(identity, name) for _, identity, name in authored] != list(EXPECTED_STEPS):
        raise ProducerError("GitHub job authored-step order differs from the seed")
    return authored


def _lifecycle(
    identity: str,
    sequence: int,
    kind: str,
    timestamp: dt.datetime,
    job_id: str | None,
    step_id: str | None,
) -> dict[str, Any]:
    return {
        "id": identity,
        "sequence": sequence,
        "kind": kind,
        "timestamp": format_timestamp(timestamp),
        "job_id": job_id,
        "step_id": step_id,
    }
