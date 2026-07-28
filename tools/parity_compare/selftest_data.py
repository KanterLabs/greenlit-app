"""Temporary repository and observation builders for comparator self-tests."""

from __future__ import annotations

import copy
import datetime as dt
import hashlib
from typing import Any


REPOSITORY_ID = "KanterLabs/greenlit-app"
ROLES = ("oracle", "github-actions", "greenlit-release")
WORKFLOW_PATH = ".github/workflows/parity-seed.yml"
WORKFLOW_BYTES = (
    b"name: Parity seed\n\n"
    b"# Canonical Phase 12 local/GitHub shell-only comparison case.\n\n"
    b"on:\n"
    b"  push:\n"
    b"    branches:\n"
    b"      - main\n"
    b"      - \"stabilization/**\"\n"
    b"  workflow_dispatch:\n\n"
    b"permissions: {}\n\n"
    b"jobs:\n"
    b"  shell:\n"
    b"    name: Shell-only parity seed\n"
    b"    runs-on: homelab\n"
    b"    steps:\n"
    b"      - id: emit\n"
    b"        name: Emit deterministic output\n"
    b"        shell: bash\n"
    b"        run: |\n"
    b"          set -euo pipefail\n"
    b"          printf '%s\\n' 'PARITY_IDENTITY job=shell step=emit'\n"
    b"          printf '%s\\n' 'seed_value=greenlit' >> \"${GITHUB_OUTPUT}\"\n"
    b"      - id: verify\n"
    b"        name: Verify shell and filesystem behavior\n"
    b"        shell: bash\n"
    b"        run: |\n"
    b"          set -euo pipefail\n"
    b"          printf '%s\\n' 'PARITY_IDENTITY job=shell step=verify'\n"
    b"          test '${{ steps.emit.outputs.seed_value }}' = 'greenlit'\n"
    b"          printf 'PARITY_OUTPUT seed_value=%s\\n' \\\n"
    b"            '${{ steps.emit.outputs.seed_value }}'\n"
    b"          printf '%s\\n' 'greenlit' > parity-seed.txt\n"
    b"          test \"$(cat parity-seed.txt)\" = 'greenlit'\n"
    b"          mode=\"$(stat -c '%a' parity-seed.txt)\"\n"
    b"          digest=\"$(sha256sum parity-seed.txt | cut -d ' ' -f 1)\"\n"
    b"          printf 'PARITY_CONTEXT github.job=%s\\n' \"${GITHUB_JOB}\"\n"
    b"          printf 'PARITY_CONTEXT github.workflow=%s\\n' \"${GITHUB_WORKFLOW}\"\n"
    b"          printf 'PARITY_CONTEXT runner.arch=%s\\n' \"${RUNNER_ARCH}\"\n"
    b"          printf 'PARITY_CONTEXT runner.os=%s\\n' \"${RUNNER_OS}\"\n"
    b"          printf 'PARITY_TEMPORARY_DIRECTORY %s\\n' \"${RUNNER_TEMP}\"\n"
    b"          printf 'PARITY_PROBE parity-seed-file mode=0%s sha256=%s\\n' \\\n"
    b"            \"${mode}\" \"${digest}\"\n"
)
LEDGER_HEADER = (
    "Exception ID",
    "Case ID",
    "Source commit",
    "Exact field",
    "Authoritative source",
    "Reason and scope",
    "Owner approval",
    "Removal criterion",
    "Status",
)

def _timestamp(base: dt.datetime, milliseconds: int) -> str:
    value = base + dt.timedelta(milliseconds=milliseconds)
    return value.isoformat(timespec="milliseconds").replace("+00:00", "Z")


def release_binary_bytes(source_commit: str) -> bytes:
    """Return a tiny executable exposing the exact release version contract."""

    return (
        b"#!/bin/sh\n"
        b"if [ \"$#\" -eq 1 ] && [ \"$1\" = \"--version\" ]; then\n"
        b"  printf 'litci 0.0.0 (%s)\\n' '"
        + source_commit.encode("ascii")
        + b"'\n"
        b"  exit 0\n"
        b"fi\n"
        b"exit 64\n"
    )


def build_observation(
    source_commit: str,
    capture_digests: dict[tuple[str, str], str],
    role: str,
    case_id: str = "contract-case",
) -> dict[str, Any]:
    """Build one valid V1 observation with role-specific normalized evidence."""

    role_index = ROLES.index(role)
    scale = role_index + 1
    base = dt.datetime(2026, 7, 28, role_index, tzinfo=dt.timezone.utc)
    offsets = [0, 10, 20, 30, 40, 60, 70, 80]
    times = [_timestamp(base, offset * scale) for offset in offsets]
    run_id = str((role_index + 1) * 101)
    producer = {
        "role": role,
        "repository": REPOSITORY_ID,
        "runner": "homelab",
        "run_id": run_id,
        "run_attempt": 2 if role == "github-actions" else 1,
        "run_url": (
            f"https://github.com/{REPOSITORY_ID}/actions/runs/{run_id}"
            if role == "github-actions"
            else None
        ),
        "binary_sha256": (
            hashlib.sha256(release_binary_bytes(source_commit)).hexdigest()
            if role == "greenlit-release"
            else None
        ),
        "capture_method": {
            "oracle": "direct-oracle",
            "github-actions": "github-api-logs",
            "greenlit-release": "retained-evidence",
        }[role],
        "capture_sha256": capture_digests[(case_id, role)],
    }
    events = (
        ("run_started", None, None),
        ("job_started", "shell", None),
        ("step_started", "shell", "emit"),
        ("step_completed", "shell", "emit"),
        ("step_started", "shell", "verify"),
        ("step_completed", "shell", "verify"),
        ("job_completed", "shell", None),
        ("run_completed", None, None),
    )
    seed = case_id == "shell-only-seed"
    contexts = [
        {"id": "github.job", "value": "shell"},
        {"id": "github.workflow", "value": "Parity seed"},
        {"id": "runner.arch", "value": "X64"},
        {"id": "runner.os", "value": "Linux"},
    ]
    return {
        "schema_version": "ParityObservationV1",
        "case_id": case_id,
        "producer": producer,
        "source": {
            "repository": REPOSITORY_ID,
            "commit": source_commit,
            "workflow_path": WORKFLOW_PATH,
            "workflow_sha256": hashlib.sha256(WORKFLOW_BYTES).hexdigest(),
        },
        "run": {
            "id": run_id,
            "started_at": times[0],
            "completed_at": times[-1],
            "duration_ms": offsets[-1] * scale,
            "conclusion": "success",
            "temporary_directory": f"/tmp/parity-{role}",
        },
        "contexts": contexts,
        "outputs": [] if seed else [{"id": "workflow_value", "value": "greenlit"}],
        "jobs": [
            {
                "id": "shell",
                "name": "Shell-only parity seed" if seed else "Contract shell",
                "conclusion": "success",
                "duration_ms": 60 * scale,
                "outputs": [],
                "steps": [
                    {
                        "id": "emit",
                        "name": "Emit deterministic output",
                        "outcome": "success",
                        "conclusion": "success",
                        "duration_ms": 10 * scale,
                        "outputs": [{"id": "seed_value", "value": "greenlit"}],
                    },
                    {
                        "id": "verify",
                        "name": "Verify shell and filesystem behavior",
                        "outcome": "success",
                        "conclusion": "success",
                        "duration_ms": 20 * scale,
                        "outputs": [],
                    },
                ],
            }
        ],
        "lifecycle": [
            {
                "id": f"transition-{index:02d}",
                "sequence": index,
                "kind": kind,
                "timestamp": times[index - 1],
                "job_id": job_id,
                "step_id": step_id,
            }
            for index, (kind, job_id, step_id) in enumerate(events, 1)
        ],
        "filesystem_probes": [
            {
                "id": "parity-seed-file",
                "logical_path": "workspace/parity-seed.txt",
                "kind": "file",
                "exists": True,
                "mode": "0644",
                "sha256": hashlib.sha256(b"greenlit\n").hexdigest(),
            }
        ],
        "resource_security_findings": (
            []
            if seed
            else [
                {
                    "id": "network-containment",
                    "category": "network",
                    "outcome": "contained",
                    "detail": "host LAN was unreachable",
                }
            ]
        ),
        "dynamic_ports": (
            []
            if seed
            else [
                {
                    "id": "service-http",
                    "container_port": 8080,
                    "host_port": 41000 + role_index,
                    "protocol": "tcp",
                }
            ]
        ),
    }

def observation_triple(
    source_commit: str,
    captures: dict[tuple[str, str], str],
    case_id: str = "contract-case",
) -> list[dict[str, Any]]:
    """Return independent oracle, GitHub Actions, and Greenlit documents."""

    return [build_observation(source_commit, captures, role, case_id) for role in ROLES]


def skipped_triple(observations: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Convert a valid triple to the canonical all-skipped lifecycle form."""

    result = copy.deepcopy(observations)
    for document in result:
        job = document["jobs"][0]
        job["conclusion"], job["duration_ms"] = "skipped", 0
        for step in job["steps"]:
            step["outcome"] = step["conclusion"] = "skipped"
            step["duration_ms"] = 0
        old = document["lifecycle"]
        transitions = (
            ("run_started", None, None, old[0]["timestamp"]),
            ("step_skipped", "shell", "emit", old[2]["timestamp"]),
            ("step_skipped", "shell", "verify", old[4]["timestamp"]),
            ("job_skipped", "shell", None, old[6]["timestamp"]),
            ("run_completed", None, None, old[7]["timestamp"]),
        )
        document["lifecycle"] = [
            {
                "id": f"skip-transition-{index:02d}",
                "sequence": index,
                "kind": kind,
                "timestamp": timestamp,
                "job_id": job_id,
                "step_id": step_id,
            }
            for index, (kind, job_id, step_id, timestamp) in enumerate(transitions, 1)
        ]
    return result


def empty_ledger() -> str:
    """Return a canonical ledger with no real exception rows."""

    header = "| " + " | ".join(LEDGER_HEADER) + " |\n"
    delimiter = "|" + "|".join("---" for _ in LEDGER_HEADER) + "|\n"
    placeholder = "| " + " | ".join("—" for _ in LEDGER_HEADER) + " |\n"
    return "# Greenlit parity-exception ledger\n\n" + header + delimiter + placeholder


def exception_ledger(
    source_commit: str,
    path: str,
    *,
    case_id: str = "contract-case",
    row_commit: str | None = None,
    authority_commit: str | None = None,
    approval: str | None = None,
    reason: str | None = None,
    removal: str | None = None,
    authority: str | None = None,
) -> str:
    """Return one active, source-bound exception row with optional corruptions."""

    row_source = row_commit or source_commit
    authority_source = authority_commit or row_source
    today = dt.datetime.now(dt.timezone.utc).date().isoformat()
    authority_value = authority or (
        f"https://github.com/{REPOSITORY_ID}/actions/runs/202; "
        f"source-commit={authority_source}"
    )
    cells = (
        "GL-PARITY-001",
        case_id,
        row_source,
        path,
        authority_value,
        reason
        or "specification-permitted degradation "
        "(greenlit-v0-spec.md#content-and-environment-preparation): "
        "upstream presentation detail is outside the declared command behavior surface",
        f"Shane {approval or today}",
        removal
        or "remove when the upstream presentation becomes part of the declared behavior contract",
        "active",
    )
    header = "| " + " | ".join(LEDGER_HEADER) + " |\n"
    delimiter = "|" + "|".join("---" for _ in LEDGER_HEADER) + "|\n"
    return (
        "# Greenlit parity-exception ledger\n\n"
        + header
        + delimiter
        + "| "
        + " | ".join(cells)
        + " |\n"
    )
