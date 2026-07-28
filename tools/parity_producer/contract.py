"""Fixed semantic shape of the canonical Phase 12 seed."""

from __future__ import annotations

from typing import Any

from parity_producer.common import canonical_observation
from parity_producer.markers import SeedMarkers


EXPECTED_JOB_NAME = "Shell-only parity seed"
EXPECTED_STEPS = (
    ("emit", "Emit deterministic output"),
    ("verify", "Verify shell and filesystem behavior"),
)


def assemble_observation(
    producer: dict[str, Any],
    source: dict[str, Any],
    projection: dict[str, Any],
    markers: SeedMarkers,
) -> dict[str, Any]:
    """Join authoritative lifecycle evidence with in-step contract probes."""
    contexts = [
        {"id": identity, "value": value}
        for identity, value in markers.contexts.items()
    ]
    emit, verify = projection["steps"]
    emit["outputs"] = [{"id": "seed_value", "value": markers.seed_value}]
    verify["outputs"] = []
    job = {
        "id": "shell",
        "name": projection["job_name"],
        "conclusion": projection["job_conclusion"],
        "duration_ms": projection["job_duration_ms"],
        "outputs": [],
        "steps": [emit, verify],
    }
    run = {
        "id": producer["run_id"],
        "started_at": projection["run_started_at"],
        "completed_at": projection["run_completed_at"],
        "duration_ms": projection["run_duration_ms"],
        "conclusion": projection["run_conclusion"],
        "temporary_directory": markers.temporary_directory,
    }
    probe = {
        "id": markers.probe_id,
        "logical_path": "workspace/parity-seed.txt",
        "kind": "file",
        "exists": True,
        "mode": markers.probe_mode,
        "sha256": markers.probe_sha256,
    }
    return canonical_observation(
        producer=producer,
        source=source,
        run=run,
        contexts=contexts,
        job=job,
        lifecycle=projection["lifecycle"],
        probe=probe,
    )
