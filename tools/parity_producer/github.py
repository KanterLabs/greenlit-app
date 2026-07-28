"""Fetch exact GitHub Actions evidence for the canonical parity seed."""

from __future__ import annotations

import os
import re
import stat
from pathlib import Path
from urllib.parse import quote

from parity_producer.bounded_process import run_bounded
from parity_producer.common import (
    AUTHORITATIVE_REPOSITORY,
    COMMIT,
    MAX_JSON_BYTES,
    WORKFLOW_PATH,
    ProducerError,
    load_json,
    load_json_bytes,
    require_integer,
    require_object,
    require_string,
)
from parity_producer.capture import Production
from parity_producer.github_evidence import project_github_evidence
from parity_producer.github_inputs import MAX_JOB_LOG_BYTES, read_job_log
from parity_producer.github_job import exact_job_id
from parity_producer.host_environment import trusted_system_tools


GH_TIMEOUT_SECONDS = 120
TOKEN_DESCRIPTOR = "GREENLIT_GITHUB_PRODUCER_CREDENTIAL_FD"
MAX_TOKEN_BYTES = 64 * 1024


def produce_github(
    repository: str,
    run_id: int,
    trusted_source_commit: str,
    *,
    run_json: Path | None = None,
    jobs_json: Path | None = None,
    content_json: Path | None = None,
    job_log_path: Path | None = None,
    self_test_raw_evidence: bool = False,
    self_test_gh_executable: Path | None = None,
) -> Production:
    """Project either live `gh api` responses or an explicit raw API export."""
    if repository != AUTHORITATIVE_REPOSITORY:
        raise ProducerError(
            f"repository identity must be {AUTHORITATIVE_REPOSITORY!r}"
        )
    if COMMIT.fullmatch(trusted_source_commit) is None:
        raise ProducerError("trusted source commit must be full lowercase SHA")
    raw_paths = (run_json, jobs_json, content_json, job_log_path)
    if self_test_gh_executable is not None and (
        not self_test_raw_evidence or any(path is not None for path in raw_paths)
    ):
        raise ProducerError(
            "the behavior-gate gh executable requires the self-test flag "
            "without raw evidence files"
        )
    if any(path is not None for path in raw_paths):
        if not self_test_raw_evidence:
            raise ProducerError(
                "raw GitHub evidence is restricted to the producer behavior gate; "
                "canonical production must fetch the exact run live"
            )
        if not all(path is not None for path in raw_paths):
            raise ProducerError(
                "raw GitHub mode requires run, jobs, content, and job-log inputs together"
            )
        run = require_object(load_json(run_json, "GitHub run API export"), "GitHub run")
        jobs = require_object(
            load_json(jobs_json, "GitHub jobs API export"), "GitHub jobs response"
        )
        content = require_object(
            load_json(content_json, "GitHub content API export"),
            "GitHub content response",
        )
        job_log = read_job_log(job_log_path)
    else:
        token = _read_github_token()
        gh = (
            _self_test_executable(self_test_gh_executable)
            if self_test_gh_executable is not None
            else trusted_system_tools(("gh",))["gh"]
        )
        run = require_object(
            load_json_bytes(
                _gh_api(
                    token,
                    gh,
                    f"repos/{repository}/actions/runs/{run_id}",
                    output_limit=MAX_JSON_BYTES,
                ),
                "GitHub run API response",
            ),
            "GitHub run API response",
        )
        attempt = require_integer(
            run.get("run_attempt"), "GitHub run attempt", 1
        )
        jobs = require_object(
            load_json_bytes(
                _gh_api(
                    token,
                    gh,
                    f"repos/{repository}/actions/runs/{run_id}"
                    f"/attempts/{attempt}/jobs?per_page=100",
                    output_limit=MAX_JSON_BYTES,
                ),
                "GitHub jobs API response",
            ),
            "GitHub jobs API response",
        )
        job_id = exact_job_id(
            repository=repository,
            run_id=run_id,
            attempt=attempt,
            trusted_source_commit=trusted_source_commit,
            jobs_response=jobs,
        )
        head_sha = require_string(run.get("head_sha"), "GitHub run head SHA")
        content = require_object(
            load_json_bytes(
                _gh_api(
                    token,
                    gh,
                    f"repos/{repository}/contents/{quote(WORKFLOW_PATH, safe='/')}",
                    fields={"ref": head_sha},
                    output_limit=MAX_JSON_BYTES,
                ),
                "GitHub content API response",
            ),
            "GitHub content API response",
        )
        job_log = _gh_api(
            token,
            gh,
            f"repos/{repository}/actions/jobs/{job_id}/logs",
            output_limit=MAX_JOB_LOG_BYTES,
        )

    return project_github_evidence(
        repository=repository,
        requested_run_id=run_id,
        run=run,
        jobs_response=jobs,
        content_response=content,
        job_log=job_log,
        trusted_source_commit=trusted_source_commit,
    )


def _gh_api(
    token: str,
    executable: str,
    endpoint: str,
    fields: dict[str, str] | None = None,
    *,
    output_limit: int,
) -> bytes:
    command = [
        executable,
        "api",
        "--hostname",
        "github.com",
        "--method",
        "GET",
        endpoint,
    ]
    for key, value in sorted((fields or {}).items()):
        command.extend(["--field", f"{key}={value}"])
    environment = {
        "GH_PROMPT_DISABLED": "1",
        "GH_TOKEN": token,
        "HOME": "/",
        "LANG": "C",
        "LC_ALL": "C",
        "PATH": "/usr/bin:/bin",
        "TZ": "UTC",
    }
    result = run_bounded(
        command,
        label="GitHub API evidence request",
        environment=environment,
        timeout_seconds=GH_TIMEOUT_SECONDS,
        stdout_limit=output_limit,
        stderr_limit=64 * 1024,
    )
    if result.returncode != 0:
        diagnostic = result.stderr[:4096].decode("utf-8", errors="replace")
        diagnostic = diagnostic.replace(token, "[redacted]")
        diagnostic = re.sub(
            r"(?i)(token|secret|password|credential)=[^\s]+",
            r"\1=[redacted]",
            diagnostic,
        )
        raise ProducerError(
            "GitHub API evidence request failed"
            + (f": {' '.join(diagnostic.split())}" if diagnostic.strip() else "")
        )
    return result.stdout


def _read_github_token() -> str:
    descriptor_value = os.environ.pop(TOKEN_DESCRIPTOR, None)
    if descriptor_value is None or not descriptor_value.isdecimal():
        raise ProducerError(
            "live GitHub parity requires its private credential descriptor"
        )
    descriptor = int(descriptor_value, 10)
    chunks: list[bytes] = []
    remaining = MAX_TOKEN_BYTES + 1
    try:
        os.set_inheritable(descriptor, False)
        while remaining > 0:
            chunk = os.read(descriptor, min(4096, remaining))
            if not chunk:
                break
            chunks.append(chunk)
            remaining -= len(chunk)
    except OSError as error:
        raise ProducerError(
            f"could not read private GitHub credential: {error}"
        ) from error
    finally:
        try:
            os.close(descriptor)
        except OSError:
            pass
    raw = b"".join(chunks)
    if not raw or len(raw) > MAX_TOKEN_BYTES:
        raise ProducerError("private GitHub credential is empty or oversized")
    try:
        return raw.decode("utf-8", errors="strict")
    except UnicodeDecodeError as error:
        raise ProducerError("private GitHub credential is not UTF-8") from error


def _self_test_executable(path: Path) -> str:
    if not path.is_absolute():
        raise ProducerError("behavior-gate gh executable must be absolute")
    try:
        metadata = path.lstat()
    except OSError as error:
        raise ProducerError(
            f"cannot inspect behavior-gate gh executable {path}: {error}"
        ) from error
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_mode & 0o111 == 0:
        raise ProducerError(
            "behavior-gate gh executable must be a regular executable"
        )
    return str(path)
