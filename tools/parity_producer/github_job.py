"""Attempt-bound identity checks for the canonical GitHub workflow job."""

from __future__ import annotations

from typing import Any

from parity_producer.common import (
    ProducerError,
    require_fields,
    require_integer,
    require_object,
)
from parity_producer.contract import EXPECTED_JOB_NAME


def exact_job_id(
    *,
    repository: str,
    run_id: int,
    attempt: int,
    trusted_source_commit: str,
    jobs_response: dict[str, Any],
) -> int:
    """Bind the sole attempt job before requesting its plain-text log."""
    require_fields(
        jobs_response,
        "GitHub jobs response",
        {"total_count", "jobs"},
        allow_extra=True,
    )
    total_count = require_integer(
        jobs_response["total_count"], "GitHub jobs total count", 0
    )
    jobs = jobs_response["jobs"]
    if not isinstance(jobs, list):
        raise ProducerError("GitHub jobs response jobs must be an array")
    if total_count != 1 or len(jobs) != 1:
        raise ProducerError("canonical GitHub seed must have exactly one job")
    job = require_object(jobs[0], "GitHub shell job")
    require_fields(
        job,
        "GitHub shell job",
        {
            "head_sha",
            "html_url",
            "id",
            "name",
            "run_attempt",
            "run_id",
            "run_url",
            "url",
        },
        allow_extra=True,
    )
    job_id = require_integer(job["id"], "GitHub shell job id", 1)
    api_run_url = f"https://api.github.com/repos/{repository}/actions/runs/{run_id}"
    api_job_url = f"https://api.github.com/repos/{repository}/actions/jobs/{job_id}"
    html_job_url = (
        f"https://github.com/{repository}/actions/runs/{run_id}/job/{job_id}"
    )
    if (
        require_integer(job["run_id"], "GitHub shell job run id", 1) != run_id
        or require_integer(job["run_attempt"], "GitHub shell job attempt", 1)
        != attempt
        or job["head_sha"] != trusted_source_commit
        or job["name"] != EXPECTED_JOB_NAME
        or job["run_url"] != api_run_url
        or job["url"] != api_job_url
        or job["html_url"] != html_job_url
    ):
        raise ProducerError("GitHub shell job does not bind the requested run attempt")
    return job_id
