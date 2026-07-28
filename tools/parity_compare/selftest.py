"""Command-boundary behavior matrix for the parity comparator."""

from __future__ import annotations

import copy
import datetime as dt
import subprocess
import sys
import tempfile
from pathlib import Path

from .selftest_audit import (
    exception_audit_matrix,
    schema_audit_matrix,
    workflow_audit_matrix,
)
from .selftest_data import (
    exception_ledger,
    observation_triple,
    skipped_triple,
)
from .selftest_live import live_boundary_matrix, version_process_matrix
from .selftest_repo import binary_canary_paths, initialize_repository
from .selftest_provenance import provenance_audit_matrix
from .selftest_runner import (
    Document,
    Documents,
    Mutation,
    Runner,
    altered as _altered,
    changed as _changed,
    renumber as _renumber,
)

def _semantic_matrix(runner: Runner, base: Documents) -> None:
    changes = (
        ("context", ("contexts", 0, "value"), "other"),
        ("workflow output", ("outputs", 0, "value"), "other"),
        ("step output", ("jobs", 0, "steps", 0, "outputs", 0, "value"), "other"),
        ("filesystem", ("filesystem_probes", 0, "sha256"), "b" * 64),
        ("resource finding", ("resource_security_findings", 0, "detail"), "different"),
        ("container port", ("dynamic_ports", 0, "container_port"), 8081),
    )
    for label, path, value in changes:
        runner.check(f"{label} semantic canary", _changed(base, path, value), 1)

    def failed_step(document: Document) -> None:
        step = document["jobs"][0]["steps"][1]
        step["outcome"] = step["conclusion"] = "failure"
        document["jobs"][0]["conclusion"] = "failure"
        document["run"]["conclusion"] = "failure"

    failed = _altered(base, failed_step)
    runner.check("intentional step conclusion mismatch", failed, 1, fragments=(
        "$.jobs[0].steps[1].conclusion",
    ))

def _schema_matrix(runner: Runner, base: Documents) -> None:
    invalid_time = _changed(
        base, ("run", "started_at"), "2026-W31-2T00:00:00Z"
    )
    runner.check("strict RFC3339", invalid_time, 2)
    chronology = _changed(
        base, ("run", "completed_at"), "2026-07-27T00:00:00Z"
    )
    runner.check("run chronology", chronology, 2)
    runner.check(
        "unknown root field",
        _altered(base, lambda value: value.__setitem__("undeclared", True)),
        2,
    )
    runner.check(
        "unknown nested field",
        _altered(
            base,
            lambda value: value["jobs"][0]["steps"][0].__setitem__(
                "undeclared", True
            ),
        ),
        2,
    )
    unknown_version = _changed(base, ("schema_version",), "ParityObservationV2")
    runner.check("unknown schema version", unknown_version, 2)
    runner.check(
        "duplicate record identity",
        _altered(
            base,
            lambda value: value["contexts"].append(
                copy.deepcopy(value["contexts"][0])
            ),
        ),
        2,
    )
    missing = _altered(base, lambda value: value["outputs"].clear())
    runner.check("missing record is exact mismatch", missing, 1)
    empty_steps = _altered(base, lambda value: value["jobs"][0]["steps"].clear())
    runner.check("nonempty step collection", empty_steps, 2)
    inconsistent = _changed(base, ("jobs", 0, "conclusion"), "failure")
    runner.check("run conclusion consistency", inconsistent, 2)
    numeric = copy.deepcopy(base)
    for document in numeric:
        document["outputs"][0]["value"] = "__EXACT_DECIMAL__"
    runner.check(
        "overflow-scale decimal comparison",
        numeric,
        1,
        raw={
            0: {"__EXACT_DECIMAL__": "1E+999"},
            1: {"__EXACT_DECIMAL__": "1E+999"},
            2: {"__EXACT_DECIMAL__": "1E+1000"},
        },
        fragments=("$.outputs[0].value",),
    )
    nonfinite = copy.deepcopy(base)
    nonfinite[2]["outputs"][0]["value"] = "__NONFINITE__"
    runner.check("non-finite numeric rejection", nonfinite, 2, raw={
        2: {"__NONFINITE__": "NaN"}
    })
    huge_duration = copy.deepcopy(base)
    huge_duration[2]["run"]["duration_ms"] = "__HUGE_DURATION__"
    runner.check(
        "duration arithmetic overflow is validation",
        huge_duration,
        2,
        raw={2: {"__HUGE_DURATION__": "1e1000000"}},
        fragments=("duration magnitude",),
    )
    extreme_offset = _changed(
        base,
        ("run", "started_at"),
        "9999-12-31T23:59:59-23:59",
    )
    runner.check("timestamp UTC conversion overflow", extreme_offset, 2)
    huge = copy.deepcopy(base)
    for document in huge:
        document["outputs"][0]["value"] = "__HUGE_INTEGER__"
    integer = "7" * 5000
    runner.check(
        "arbitrary-precision integer comparison",
        huge,
        0,
        raw={
            0: {"__HUGE_INTEGER__": integer},
            1: {"__HUGE_INTEGER__": integer},
            2: {"__HUGE_INTEGER__": integer},
        },
    )
    lexical_integer = copy.deepcopy(base)
    lexical_integer[2]["dynamic_ports"][0]["container_port"] = "__INTEGER_FLOAT__"
    runner.check(
        "integer fields require JSON integer syntax",
        lexical_integer,
        2,
        raw={2: {"__INTEGER_FLOAT__": "8080.0"}},
    )
    unsafe_text = _changed(base, ("outputs", 0, "value"), "unsafe\u009bvalue")
    runner.check(
        "mismatch diagnostics escape terminal controls",
        unsafe_text,
        1,
        fragments=("\\u009b",),
        forbidden_fragments=("\u009b",),
    )

def _provenance_matrix(runner: Runner, base: Documents) -> None:
    runner.check("trusted repository mismatch", base, 2, repository_id="Other/repo")
    runner.check("trusted source commit mismatch", base, 2, source_commit="f" * 40)
    changes = (
        ("observation source commit", ("source", "commit"), "f" * 40),
        ("workflow path", ("source", "workflow_path"), ".github/workflows/missing.yml"),
        ("workflow digest", ("source", "workflow_sha256"), "f" * 64),
        ("producer run binding", ("producer", "run_id"), "999999"),
        ("cross-producer runner", ("producer", "runner"), "another-runner"),
    )
    for label, path, value in changes:
        runner.check(label, _changed(base, path, value), 2)
    swapped = copy.deepcopy(base)
    swapped[0], swapped[1] = swapped[1], swapped[0]
    runner.check("position-bound producer role", swapped, 2)
    corrupt = lambda values, _capture_root: values[2]["producer"].__setitem__(
        "capture_sha256", "f" * 64
    )
    runner.check("capture digest", base, 2, after_seal=corrupt)
    labels = (
        "missing release binary",
        "release binary digest",
        "release binary symlink",
        "non-executable release binary",
    )
    paths = binary_canary_paths(runner.temporary, runner.greenlit_binary)
    for label, path in zip(labels, paths, strict=True):
        runner.check(label, base, 2, greenlit_binary=path)
    version_process_matrix(runner, base)

def _lifecycle_matrix(runner: Runner, base: Documents) -> None:
    def missing_pair(document: Document) -> None:
        del document["lifecycle"][5]
        _renumber(document)

    def reordered(document: Document) -> None:
        document["lifecycle"][2], document["lifecycle"][4] = (
            document["lifecycle"][4],
            document["lifecycle"][2],
        )
        _renumber(document)

    runner.check("missing lifecycle pair", _altered(base, missing_pair), 2)
    runner.check("lifecycle authored order", _altered(base, reordered), 2)
    unknown_kind = _changed(base, ("lifecycle", 2, "kind"), "step_retried")
    runner.check("unknown lifecycle kind", unknown_kind, 2)
    unknown = _changed(base, ("lifecycle", 2, "job_id"), "missing-job")
    runner.check("lifecycle unknown reference", unknown, 2)
    skipped = skipped_triple(base)
    runner.check("valid skipped lifecycle", skipped, 0)
    invalid_skip = _changed(
        skipped, ("jobs", 0, "steps", 0, "conclusion"), "success"
    )
    runner.check("invalid skipped result", invalid_skip, 2)


def _seed_and_filesystem_matrix(
    runner: Runner, base: Documents, seed: Documents
) -> None:
    runner.check("valid seed required collections", seed, 0)
    relabeled = copy.deepcopy(seed)
    for document in relabeled:
        document["case_id"] = "renamed-seed"
    runner.check(
        "seed semantics cannot be bypassed by case relabeling",
        relabeled,
        2,
        fragments=("fixed 'shell-only-seed' contract",),
    )
    mutations: tuple[tuple[str, Mutation], ...] = (
        ("seed contexts", lambda value: value["contexts"].pop()),
        (
            "seed workflow outputs",
            lambda value: value["outputs"].append(
                {"id": "unexpected", "value": "value"}
            ),
        ),
        (
            "seed job outputs",
            lambda value: value["jobs"][0]["outputs"].append(
                {"id": "unexpected", "value": "value"}
            ),
        ),
        (
            "seed step outputs",
            lambda value: value["jobs"][0]["steps"][0]["outputs"].clear(),
        ),
        ("seed probe collection", lambda value: value["filesystem_probes"].clear()),
        (
            "seed findings",
            lambda value: value["resource_security_findings"].extend(
                copy.deepcopy(base[2]["resource_security_findings"])
            ),
        ),
        (
            "seed ports",
            lambda value: value["dynamic_ports"].extend(
                copy.deepcopy(base[2]["dynamic_ports"])
            ),
        ),
    )
    for label, mutation in mutations:
        runner.check(label, _altered(seed, mutation), 2)
    filesystem = (
        ("missing file digest", ("filesystem_probes", 0, "sha256"), None),
        ("missing file mode", ("filesystem_probes", 0, "mode"), None),
        ("unstable logical path", ("filesystem_probes", 0, "logical_path"), "../escape"),
    )
    for label, path, value in filesystem:
        runner.check(label, _changed(base, path, value), 2)


def _exception_matrix(runner: Runner, base: Documents) -> None:
    mismatch = _changed(base, ("outputs", 0, "value"), "excepted")
    valid = exception_ledger(runner.source_commit, "$.outputs[0].value")
    runner.check(
        "exact present local scalar exception",
        mismatch,
        0,
        ledger_text=valid,
        fragments=("GL-PARITY-001",),
    )
    github_mismatch = _changed(
        mismatch, ("outputs", 0, "value"), "excepted", index=1
    )
    runner.check(
        "oracle GitHub mismatch is never exceptable",
        github_mismatch,
        1,
        ledger_text=valid,
    )
    typed = _changed(base, ("outputs", 0, "value"), 7)
    runner.check("exception type laundering", typed, 2, ledger_text=valid)
    unrelated = exception_ledger(
        runner.source_commit,
        "$.outputs[0].value",
        case_id="other-case",
        authority=(
            "https://github.com/KanterLabs/greenlit-app/actions/runs/999; "
            f"source-commit={runner.source_commit}"
        ),
    )
    runner.check(
        "unrelated active case authority is ignored",
        base,
        0,
        ledger_text=unrelated,
    )
    tomorrow = (
        dt.datetime.now(dt.timezone.utc).date() + dt.timedelta(days=1)
    ).isoformat()
    invalid_specs = (
        (
            "future approval",
            "$.outputs[0].value",
            {"approval": tomorrow},
        ),
        (
            "in-scope reason",
            "$.outputs[0].value",
            {"reason": "specification-permitted degradation "
                "(greenlit-v0-spec.md#compatibility-and-result-truth): "
                "this is an in-scope defect "
                "that requires implementation repair"},
        ),
        (
            "shallow reason",
            "$.outputs[0].value",
            {"reason": "explicit non-goal "
                "(greenlit-v0-spec.md#explicit-non-goals): too short"},
        ),
        (
            "indefinite removal",
            "$.outputs[0].value",
            {"removal": "remove when never"},
        ),
        ("whole record exception", "$.outputs[0]", {}),
        ("missing record exception", "$.outputs[9].value", {}),
        ("non-leaf exception", "$.outputs", {}),
        ("raw control exception member", '$.outputs[0].value["bad\tkey"]', {}),
        ("protected case identity", "$.case_id", {}),
        ("protected source", "$.source.commit", {}),
        ("protected producer", "$.producer.runner", {}),
        ("wrong exception source commit", "$.outputs[0].value", {"row_commit": "f" * 40}),
        ("wrong authority binding", "$.outputs[0].value", {"authority_commit": "f" * 40}),
        ("wrong authority repository", "$.outputs[0].value", {
            "authority": "https://github.com/Other/repo/actions/runs/1; "
            f"source-commit={runner.source_commit}"}),
        ("wrong current Actions run", "$.outputs[0].value", {
            "authority": "https://github.com/KanterLabs/greenlit-app/"
            f"actions/runs/999; source-commit={runner.source_commit}"}),
    )
    for label, path, options in invalid_specs:
        ledger = exception_ledger(runner.source_commit, path, **options)
        runner.check(label, mismatch, 2, ledger_text=ledger)
    runner.check("stale active exception", base, 2, ledger_text=valid)

def run_self_test(executable: Path) -> int:
    """Exercise the public comparator command against isolated committed evidence."""

    with tempfile.TemporaryDirectory(prefix="compare-parity-self-test-") as raw:
        temporary = Path(raw)
        repository = temporary / "repository"
        try:
            source_commit, captures, binary = initialize_repository(repository)
        except (OSError, subprocess.CalledProcessError) as error:
            print(f"compare-parity self-test setup failed: {error}", file=sys.stderr)
            return 1
        base = observation_triple(source_commit, captures)
        seed = observation_triple(source_commit, captures, "shell-only-seed")
        runner = Runner(executable.resolve(), temporary, repository, source_commit, binary)
        runner.check("positive triple with every normalization", base, 0)
        _semantic_matrix(runner, base)
        _schema_matrix(runner, base)
        _provenance_matrix(runner, base)
        _lifecycle_matrix(runner, base)
        _seed_and_filesystem_matrix(runner, base, seed)
        _exception_matrix(runner, base)
        schema_audit_matrix(runner, base)
        provenance_audit_matrix(runner, base)
        live_boundary_matrix(runner, base)
        workflow_audit_matrix(runner)
        exception_audit_matrix(runner, base)
        if runner.failures:
            for failure in runner.failures:
                print(f"self-test failure: {failure}", file=sys.stderr)
            summary = f"compare-parity self-test failed: {len(runner.failures)} "
            summary += f"of {runner.count} checks failed"
            print(summary, file=sys.stderr)
            status = 1
        else:
            print(f"compare-parity self-test passed ({runner.count} command checks)")
            status = 0
    return status

__all__ = ["run_self_test"]
