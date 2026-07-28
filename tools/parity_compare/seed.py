"""Fixed semantic contract for the canonical shell-only parity seed."""

from __future__ import annotations

import hashlib
from typing import Any

from . import ContractError


_SEED_FILE_SHA256 = hashlib.sha256(b"greenlit\n").hexdigest()


def validate_seed_contract(root: dict[str, Any]) -> None:
    """Reject incomplete or self-consistently wrong shell-only seed evidence."""

    if root["case_id"] != "shell-only-seed":
        return
    if root["producer"]["runner"] != "homelab":
        raise ContractError(
            "$.producer.runner: shell-only-seed requires homelab"
        )
    if root["source"]["workflow_path"] != ".github/workflows/parity-seed.yml":
        raise ContractError("$.source.workflow_path: shell-only-seed source changed")
    if root["run"]["conclusion"] != "success":
        raise ContractError("$.run.conclusion: shell-only-seed must succeed")
    if root["contexts"] != [
        {"id": "github.job", "value": "shell"},
        {"id": "github.workflow", "value": "Parity seed"},
        {"id": "runner.arch", "value": "X64"},
        {"id": "runner.os", "value": "Linux"},
    ]:
        raise ContractError(
            "$.contexts: shell-only-seed context contract is incomplete"
        )
    if root["outputs"]:
        raise ContractError("$.outputs: shell-only-seed declares no workflow outputs")
    if len(root["jobs"]) != 1:
        raise ContractError("$.jobs: shell-only-seed requires exactly one job")
    job = root["jobs"][0]
    if (
        job["id"] != "shell"
        or job["name"] != "Shell-only parity seed"
        or job["conclusion"] != "success"
        or job["outputs"]
    ):
        raise ContractError("$.jobs[0]: shell-only-seed job result changed")
    expected_steps = (
        (
            "emit",
            "Emit deterministic output",
            [{"id": "seed_value", "value": "greenlit"}],
        ),
        ("verify", "Verify shell and filesystem behavior", []),
    )
    if len(job["steps"]) != len(expected_steps):
        raise ContractError("$.jobs[0].steps: shell-only-seed step set changed")
    for index, (identity, name, outputs) in enumerate(expected_steps):
        step = job["steps"][index]
        if (
            step["id"] != identity
            or step["name"] != name
            or step["outcome"] != "success"
            or step["conclusion"] != "success"
            or step["outputs"] != outputs
        ):
            raise ContractError(
                f"$.jobs[0].steps[{index}]: shell-only-seed step result changed"
            )
    expected_lifecycle = [
        ("run_started", None, None),
        ("job_started", "shell", None),
        ("step_started", "shell", "emit"),
        ("step_completed", "shell", "emit"),
        ("step_started", "shell", "verify"),
        ("step_completed", "shell", "verify"),
        ("job_completed", "shell", None),
        ("run_completed", None, None),
    ]
    lifecycle = [
        (event["kind"], event["job_id"], event["step_id"])
        for event in root["lifecycle"]
    ]
    if lifecycle != expected_lifecycle:
        raise ContractError(
            "$.lifecycle: shell-only-seed lifecycle is incomplete"
        )
    if root["filesystem_probes"] != [
        {
            "id": "parity-seed-file",
            "logical_path": "workspace/parity-seed.txt",
            "kind": "file",
            "exists": True,
            "mode": "0644",
            "sha256": _SEED_FILE_SHA256,
        }
    ]:
        raise ContractError(
            "$.filesystem_probes: shell-only-seed file evidence changed"
        )
    if root["resource_security_findings"]:
        raise ContractError(
            "$.resource_security_findings: shell-only-seed declares no findings"
        )
    if root["dynamic_ports"]:
        raise ContractError(
            "$.dynamic_ports: shell-only-seed declares no dynamic ports"
        )


__all__ = ["validate_seed_contract"]
