"""Canonical raw GitHub boundary fixture for the parity producer gate."""

from __future__ import annotations

import base64
import hashlib
import json
from pathlib import Path


REPOSITORY_ID = "KanterLabs/greenlit-app"
RUN_ID = 987654321
JOB_ID = 123456789


def github_inputs(
    directory: Path,
    workflow: bytes,
    source_commit: str,
) -> dict[str, Path]:
    """Write one exact run, attempt job, content, and plain-text log fixture."""
    run = {
        "id": RUN_ID,
        "name": "Parity seed",
        "event": "push",
        "status": "completed",
        "conclusion": "success",
        "head_sha": source_commit,
        "path": ".github/workflows/parity-seed.yml",
        "run_attempt": 1,
        "run_started_at": "2026-07-28T10:00:00Z",
        "updated_at": "2026-07-28T10:00:04Z",
        "html_url": f"https://github.com/{REPOSITORY_ID}/actions/runs/{RUN_ID}",
        "repository": {"full_name": REPOSITORY_ID},
    }
    step_values = (
        (1, "Set up job", "00", "01"),
        (2, "Emit deterministic output", "01", "02"),
        (3, "Verify shell and filesystem behavior", "02", "03"),
        (4, "Complete job", "03", "04"),
    )
    steps = [
        {
            "name": step_name,
            "status": "completed",
            "conclusion": "success",
            "number": number,
            "started_at": f"2026-07-28T10:00:{started}Z",
            "completed_at": f"2026-07-28T10:00:{completed}Z",
        }
        for number, step_name, started, completed in step_values
    ]
    jobs = {
        "total_count": 1,
        "jobs": [
            {
                "id": JOB_ID,
                "run_id": RUN_ID,
                "url": (
                    f"https://api.github.com/repos/{REPOSITORY_ID}"
                    f"/actions/jobs/{JOB_ID}"
                ),
                "run_url": (
                    f"https://api.github.com/repos/{REPOSITORY_ID}"
                    f"/actions/runs/{RUN_ID}"
                ),
                "html_url": (
                    f"https://github.com/{REPOSITORY_ID}"
                    f"/actions/runs/{RUN_ID}/job/{JOB_ID}"
                ),
                "name": "Shell-only parity seed",
                "status": "completed",
                "conclusion": "success",
                "head_sha": source_commit,
                "run_attempt": 1,
                "started_at": "2026-07-28T10:00:00Z",
                "completed_at": "2026-07-28T10:00:04Z",
                "labels": ["homelab"],
                "steps": steps,
            }
        ],
    }
    content = {
        "type": "file",
        "path": ".github/workflows/parity-seed.yml",
        "encoding": "base64",
        "content": base64.b64encode(workflow).decode("ascii"),
    }
    paths = {
        "run": directory / "run.json",
        "jobs": directory / "jobs.json",
        "content": directory / "content.json",
        "log": directory / "job.log",
    }
    for key, document in (("run", run), ("jobs", jobs), ("content", content)):
        paths[key].write_text(
            json.dumps(document, separators=(",", ":")),
            encoding="utf-8",
        )
    write_job_log(paths["log"])
    return paths


def write_job_log(
    path: Path,
    *,
    duplicate: bool = False,
    masked: bool = False,
) -> None:
    """Write the observed job-log form, including command-echo decoys."""
    digest = hashlib.sha256(b"greenlit\n").hexdigest()
    lines = [
        "2026-07-28T10:00:01Z ##[group]Run printf 'PARITY_OUTPUT seed_value=greenlit'",
        "2026-07-28T10:00:01Z printf 'PARITY_OUTPUT seed_value=greenlit\\n'",
        (
            "2026-07-28T10:00:01Z \u001b[36;1mprintf "
            "'PARITY_OUTPUT seed_value=greenlit\\n'\u001b[0m"
        ),
        "2026-07-28T10:00:01Z PARITY_IDENTITY job=shell step=emit",
        "2026-07-28T10:00:02Z PARITY_IDENTITY job=shell step=verify",
        (
            "2026-07-28T10:00:02Z PARITY_OUTPUT seed_value=***"
            if masked
            else "2026-07-28T10:00:02Z PARITY_OUTPUT seed_value=greenlit"
        ),
        "2026-07-28T10:00:02Z PARITY_CONTEXT github.job=shell",
        "2026-07-28T10:00:02Z PARITY_CONTEXT github.workflow=Parity seed",
        "2026-07-28T10:00:02Z PARITY_CONTEXT runner.arch=X64",
        "2026-07-28T10:00:02Z PARITY_CONTEXT runner.os=Linux",
        "2026-07-28T10:00:02Z PARITY_TEMPORARY_DIRECTORY /runner/_temp/seed",
        f"2026-07-28T10:00:02Z PARITY_PROBE parity-seed-file mode=0644 sha256={digest}",
    ]
    if duplicate:
        lines.insert(6, "2026-07-28T10:00:02Z PARITY_OUTPUT seed_value=greenlit")
    path.write_text("\n".join(lines) + "\n", encoding="utf-8-sig")
