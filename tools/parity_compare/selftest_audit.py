"""Regression canaries for independently audited parity false-green paths."""

from __future__ import annotations

import copy
import datetime as dt
import hashlib
import subprocess
from pathlib import Path
from typing import Callable

from .selftest_data import (
    WORKFLOW_PATH,
    exception_ledger,
    observation_triple,
    release_binary_bytes,
)
from .selftest_repo import (
    initialize_repository,
)
from .selftest_runner import (
    Document,
    Documents,
    Runner,
    altered,
    changed,
    renumber,
)


def schema_audit_matrix(runner: Runner, base: Documents) -> None:
    """Reject precision, topology, hierarchy, and recursion contradictions."""

    precision = changed(
        base,
        ("run", "started_at"),
        "2026-07-28T02:00:00.0000009Z",
    )
    runner.check("unsupported timestamp precision", precision, 2)

    def failed_with_successful_steps(document: Document) -> None:
        document["run"]["conclusion"] = "failure"
        document["jobs"][0]["conclusion"] = "failure"

    runner.check(
        "failed job with only successful steps",
        altered(base, failed_with_successful_steps),
        2,
    )
    runner.check(
        "skipped run with successful job",
        changed(base, ("run", "conclusion"), "skipped"),
        2,
    )

    def contradictory_run_span(document: Document) -> None:
        started = dt.datetime.fromisoformat(
            document["lifecycle"][0]["timestamp"].replace("Z", "+00:00")
        )
        completed = dt.datetime.fromisoformat(
            document["lifecycle"][-1]["timestamp"].replace("Z", "+00:00")
        )
        document["lifecycle"][0]["timestamp"] = (
            (started - dt.timedelta(seconds=1)).isoformat().replace("+00:00", "Z")
        )
        document["lifecycle"][-1]["timestamp"] = (
            (completed + dt.timedelta(seconds=1))
            .isoformat()
            .replace("+00:00", "Z")
        )

    runner.check(
        "run duration contradicts lifecycle endpoints",
        altered(base, contradictory_run_span),
        2,
    )

    def absent_ancestor(document: Document) -> None:
        document["filesystem_probes"].insert(
            0,
            {
                "id": "workspace-root",
                "logical_path": "workspace",
                "kind": "absent",
                "exists": False,
                "mode": None,
                "sha256": None,
            },
        )

    runner.check(
        "existing probe beneath absent ancestor",
        altered(base, absent_ancestor),
        2,
    )

    def file_ancestor(document: Document) -> None:
        document["filesystem_probes"].insert(
            0,
            {
                "id": "workspace-root",
                "logical_path": "workspace",
                "kind": "file",
                "exists": True,
                "mode": "0644",
                "sha256": "a" * 64,
            },
        )

    runner.check(
        "existing probe beneath file ancestor",
        altered(base, file_ancestor),
        2,
    )
    recursive = copy.deepcopy(base)
    recursive[2]["outputs"][0]["value"] = "__DEEPLY_NESTED__"
    runner.check(
        "deep JSON recursion is validation",
        recursive,
        2,
        raw={2: {"__DEEPLY_NESTED__": "[" * 2000 + "0" + "]" * 2000}},
    )


def exception_audit_matrix(runner: Runner, base: Documents) -> None:
    """Reject malformed, overbroad, stale, and premature exception authority."""

    valid = exception_ledger(runner.source_commit, "$.outputs[0].value")
    malformed = valid.rstrip("\n")
    malformed = malformed[:-1] + "\n"
    runner.check("exception row missing trailing pipe", base, 2, ledger_text=malformed)
    runner.check(
        "exception backslash is not discarded",
        base,
        2,
        ledger_text=valid.replace("Shane ", "Sh\\ane ", 1),
    )
    nonexistent_anchor = valid.replace(
        "#content-and-environment-preparation",
        "#not-a-real-spec-anchor",
    )
    runner.check(
        "exception nonexistent spec anchor",
        base,
        2,
        ledger_text=nonexistent_anchor,
    )
    conclusion = exception_ledger(
        runner.source_commit,
        "$.jobs[0].conclusion",
    )
    runner.check(
        "conclusion exception cannot hide failure truth",
        base,
        2,
        ledger_text=conclusion,
    )
    nested = copy.deepcopy(base)
    for document in nested:
        document["outputs"][0]["value"] = {"id": "original"}
    nested[2]["outputs"][0]["value"]["id"] = "excepted"
    nested_exception = exception_ledger(
        runner.source_commit,
        "$.outputs[0].value.id",
    )
    runner.check(
        "semantic value member named id remains exceptable",
        nested,
        0,
        ledger_text=nested_exception,
    )
    bracket = exception_ledger(
        runner.source_commit,
        '$.outputs[0].value["id"]',
    )
    runner.check(
        "exception path must use comparator canonical spelling",
        nested,
        2,
        ledger_text=bracket,
        fragments=("canonical comparator JSONPath spelling",),
    )
    punctuated = copy.deepcopy(base)
    for document in punctuated:
        document["outputs"][0]["value"] = {"foo-bar": "original"}
    punctuated[2]["outputs"][0]["value"]["foo-bar"] = "excepted"
    punctuated_exception = exception_ledger(
        runner.source_commit,
        '$.outputs[0].value["foo-bar"]',
    )
    runner.check(
        "canonical bracket member exception applies",
        punctuated,
        0,
        ledger_text=punctuated_exception,
    )
    unicode_member = copy.deepcopy(base)
    for document in unicode_member:
        document["outputs"][0]["value"] = {"é": "original"}
    unicode_member[2]["outputs"][0]["value"]["é"] = "excepted"
    unicode_exception = exception_ledger(
        runner.source_commit,
        '$.outputs[0].value["\\u00e9"]',
    )
    runner.check(
        "escaped Unicode member exception applies",
        unicode_member,
        0,
        ledger_text=unicode_exception,
    )

    first = exception_ledger(
        runner.source_commit,
        "$.resource_security_findings[0].category",
    )
    second = exception_ledger(
        runner.source_commit,
        "$.resource_security_findings[0].detail",
    ).splitlines()[-1].replace("GL-PARITY-001", "GL-PARITY-002", 1)
    aggregate = first + second + "\n"

    def two_leaf_drift(document: Document) -> None:
        finding = document["resource_security_findings"][0]
        finding["category"] = "host-network"
        finding["detail"] = "host LAN accepted workflow traffic"

    runner.check(
        "multiple leaves cannot waive an aggregate record",
        altered(base, two_leaf_drift),
        2,
        ledger_text=aggregate,
    )
    github_only = changed(base, ("contexts", 0, "value"), "other", index=1)
    runner.check(
        "stale exception is checked before GitHub mismatch gate",
        github_only,
        2,
        ledger_text=valid,
    )
    both = changed(base, ("outputs", 0, "value"), "excepted")
    both = changed(both, ("contexts", 0, "value"), "other", index=1)
    runner.check(
        "GitHub gate reports no premature Greenlit result",
        both,
        1,
        ledger_text=valid,
        fragments=("github-actions $.contexts[0].value",),
        forbidden_fragments=("greenlit-release $.outputs[0].value",),
    )


def _git(repository: Path, *arguments: str) -> str:
    result = subprocess.run(
        ["git", "-C", str(repository), *arguments],
        check=True,
        capture_output=True,
        text=True,
    )
    return result.stdout.strip()


def _workflow_case(
    parent: Runner,
    label: str,
    transform: Callable[[bytes], bytes],
    case_id: str = "shell-only-seed",
) -> None:
    repository = parent.temporary / f"hostile-{label.replace(' ', '-')}"
    _, captures, _ = initialize_repository(repository)
    workflow = repository / WORKFLOW_PATH
    mutated = transform(workflow.read_bytes())
    workflow.write_bytes(mutated)
    _git(repository, "add", WORKFLOW_PATH)
    _git(repository, "commit", "-m", f"test: {label}")
    source_commit = _git(repository, "rev-parse", "HEAD")
    binary = repository.parent / f"{repository.name}-candidate" / "release" / "litci"
    binary.parent.mkdir(parents=True)
    binary.write_bytes(release_binary_bytes(source_commit))
    binary.chmod(0o755)
    documents = observation_triple(source_commit, captures, case_id)
    digest = hashlib.sha256(mutated).hexdigest()
    binary_digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    for document in documents:
        document["source"]["workflow_sha256"] = digest
    documents[2]["producer"]["binary_sha256"] = binary_digest
    child = Runner(
        parent.executable,
        parent.temporary,
        repository,
        source_commit,
        binary,
    )
    child.count = parent.count
    child.check(label, documents, 2)
    parent.count = child.count
    parent.failures.extend(child.failures)


def workflow_audit_matrix(runner: Runner) -> None:
    """Bind observations to exact committed trigger, step, and run declarations."""

    def extra_step(raw: bytes) -> bytes:
        return raw + (
            b"      - id: hidden-failure\n"
            b"        name: Hidden failure\n"
            b"        shell: bash\n"
            b"        run: |\n"
            b"          exit 1\n"
        )

    _workflow_case(runner, "extra committed workflow step", extra_step)
    _workflow_case(
        runner,
        "case relabel cannot bypass workflow declarations",
        extra_step,
        "contract-case",
    )

    def remove_branch(raw: bytes) -> bytes:
        return raw.replace(b"      - \"stabilization/**\"\n", b"", 1)

    _workflow_case(runner, "scoped push trigger drift", remove_branch)


__all__ = [
    "exception_audit_matrix",
    "schema_audit_matrix",
    "workflow_audit_matrix",
]
