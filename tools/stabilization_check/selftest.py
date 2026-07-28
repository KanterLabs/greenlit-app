"""Command-boundary behavior canaries for stabilization governance."""

from __future__ import annotations

import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

from .selftest_extra import extra_parity_cases
from .selftest_markdown import markdown_cases, markdown_valid_cases

GIT_EXECUTABLE = "/usr/bin/git"
SOURCE_COMMIT = "0123456789abcdef0123456789abcdef01234567"
PARITY_HEADER = (
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
VALID_EXCEPTION = (
    "GL-PARITY-001",
    "seed-case",
    SOURCE_COMMIT,
    "$.jobs[0].name",
    "https://github.com/KanterLabs/greenlit-app/actions/runs/7; "
    f"source-commit={SOURCE_COMMIT}",
    "specification-permitted degradation "
    "(greenlit-v0-spec.md#content-and-environment-preparation): "
    "GitHub exposes an intentional "
    "platform-only presentation value for this observed field",
    "Shane 2000-01-01",
    "remove when GitHub and Greenlit expose the same documented "
    "presentation value for this field",
    "active",
)
@dataclass(frozen=True)
class Case:
    """One public-command rejection canary."""

    name: str
    agents: str
    stabilization: str
    parity: str
    expected: tuple[str, ...]
def _row(cells: tuple[str, ...]) -> str:
    return f"| {' | '.join(cells)} |"
def _agents(
    *,
    duplicate_phase: bool = False,
    invalid_phase: bool = False,
    omit_last: bool = False,
) -> str:
    rows: list[str] = []
    for phase in range(12, 29):
        if omit_last and phase == 28:
            continue
        status = (
            "completed"
            if phase == 12
            else "in progress"
            if phase == 13
            else "not started"
        )
        if invalid_phase and phase == 13:
            status = "paused"
        rows.append(
            f"| {phase} — Test phase {phase} | docs/PHASE-{phase}.md | {status} |"
        )
        if duplicate_phase and phase == 12:
            rows.append(
                "| 12 — Duplicate phase | docs/PHASE-12-copy.md | completed |"
            )
    return (
        "# Test governance\n\n"
        "## Phase status\n\n"
        "| Phase | File | Status |\n"
        "|---|---|---|\n"
        f"{chr(10).join(rows)}\n"
    )
def _stabilization(
    resolving_commit: str,
    rows: tuple[tuple[str, ...], ...] | None = None,
) -> str:
    values = rows or (
        (
            "GL-STAB-001",
            "high",
            "12",
            "repaired user-visible impact",
            "compiled behavior-level repair gate",
            "resolved",
            resolving_commit,
        ),
    )
    return (
        "# Greenlit stabilization ledger\n\n"
        "| Defect ID | Severity | Owning phase | User-visible impact | "
        "Authoritative test or oracle | Status | Resolving commit |\n"
        "|---|---|---:|---|---|---|---|\n"
        f"{chr(10).join(_row(value) for value in values)}\n"
    )
def _parity(
    rows: tuple[tuple[str, ...], ...] | None = None,
    *,
    header: tuple[str, ...] = PARITY_HEADER,
) -> str:
    values = rows if rows is not None else (VALID_EXCEPTION,)
    delimiter = tuple("---" for _ in header)
    return (
        "# Greenlit parity-exception ledger\n\n"
        f"{_row(header)}\n"
        f"{_row(delimiter)}\n"
        f"{chr(10).join(_row(value) for value in values)}\n"
    )
def _replace(field: int, value: str) -> tuple[str, ...]:
    cells = list(VALID_EXCEPTION)
    cells[field] = value
    return tuple(cells)
def _run(
    executable: Path,
    agents: Path,
    stabilization: Path,
    parity: Path,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [
            sys.executable,
            str(executable),
            "--agents",
            str(agents),
            "--stabilization-ledger",
            str(stabilization),
            "--parity-exceptions",
            str(parity),
        ],
        capture_output=True,
        text=True,
        check=False,
    )


def _governance_case(valid_parity: str, resolving_commit: str) -> Case:
    rows = (
        (
            "GL-STAB-001",
            "urgent",
            "12",
            "unsafe\u202eimpact",
            "—",
            "open",
            "deadbeef",
        ),
        (
            "GL-STAB-001",
            "high",
            "29",
            "duplicate impact",
            "duplicate oracle",
            "done",
            "—",
        ),
        (
            "GL-STAB-003",
            "low",
            "14",
            "prematurely resolved impact",
            "premature resolution oracle",
            "resolved",
            "—",
        ),
    )
    return Case(
        "governance aggregation",
        _agents(duplicate_phase=True, invalid_phase=True, omit_last=True),
        _stabilization(resolving_commit, rows),
        valid_parity,
        (
            "duplicate phase 12",
            "invalid phase status 'paused'",
            "Phase status table is missing phase 28",
            "duplicate Defect ID 'GL-STAB-001'",
            "invalid Severity 'urgent'",
            "Owning phase must be an integer from 12 through 28",
            "User-visible impact must be nonempty plain text",
            "Authoritative test or oracle must be nonempty plain text",
            "invalid defect Status 'done'",
            "resolved defect requires a 7–40 character",
            "non-resolved defect must use '—'",
            "completed phase 12 may not own a open defect",
            "not-started phase 14 may not own a resolved defect",
        ),
    )


def _create_baseline_commit(root: Path) -> str:
    subprocess.run(
        [GIT_EXECUTABLE, "-C", str(root), "init", "--quiet", "--template="],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
        check=True,
        timeout=10,
    )
    subprocess.run(
        [
            GIT_EXECUTABLE,
            "-C",
            str(root),
            "-c",
            "user.name=Greenlit checker self-test",
            "-c",
            "user.email=checker-self-test@invalid.example",
            "-c",
            "commit.gpgSign=false",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "checker baseline",
        ],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
        check=True,
        timeout=10,
    )
    completed = subprocess.run(
        [GIT_EXECUTABLE, "-C", str(root), "rev-parse", "HEAD"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=True,
        timeout=10,
    )
    return completed.stdout.strip()


def _parity_cases(valid_agents: str, valid_stabilization: str) -> list[Case]:
    legacy_header = tuple(
        cell for cell in PARITY_HEADER if cell != "Source commit"
    )
    duplicate_id = VALID_EXCEPTION
    duplicate_key = _replace(0, "GL-PARITY-002")
    placeholder = tuple("—" for _ in PARITY_HEADER)
    mutations = (
        (
            "short source commit",
            _parity((_replace(2, "deadbeef"),)),
            ("Source commit must be a full lowercase 40-character commit",),
        ),
        (
            "wildcard path",
            _parity((_replace(3, "$.jobs[*].conclusion"),)),
            ("Exact field must be one canonical leaf JSONPath",),
        ),
        (
            "whole record",
            _parity((_replace(3, "$.jobs[0]"),)),
            ("not a record, collection, identity, reference, or unknown field",),
        ),
        (
            "producer laundering",
            _parity((_replace(3, "$.producer.capture_sha256"),)),
            ("schema, case, source, producer, and lifecycle fields cannot be excepted",),
        ),
        (
            "identity laundering",
            _parity((_replace(3, "$.jobs[0].id"),)),
            ("must identify a V1 semantic scalar leaf",),
        ),
        (
            "normalized field",
            _parity((_replace(3, "$.run.duration_ms"),)),
            ("normalized-only fields cannot be excepted",),
        ),
        (
            "unbound authority",
            _parity((_replace(4, "retained run"),)),
            ("Authoritative source must end with",),
        ),
        (
            "defect waiver",
            _parity(
                (
                    _replace(
                        5,
                        "specification-permitted degradation "
                        "(greenlit-v0-spec.md#compatibility-and-result-truth): "
                        "This in-scope defect "
                        "would otherwise fail the parity comparison",
                    ),
                )
            ),
            ("never an in-scope defect",),
        ),
        (
            "agent approval",
            _parity((_replace(6, "Agent 2000-01-01"),)),
            ("Owner approval must be 'Shane YYYY-MM-DD'",),
        ),
        (
            "impossible approval date",
            _parity((_replace(6, "Shane 2000-02-30"),)),
            ("invalid owner approval date",),
        ),
        (
            "future approval",
            _parity((_replace(6, "Shane 9999-01-01"),)),
            ("Owner approval date cannot be in the future",),
        ),
        (
            "permanent removal",
            _parity((_replace(7, "remove when owner discretion decides"),)),
            ("Removal criterion must be 'remove when'",),
        ),
        (
            "invalid status",
            _parity((_replace(8, "waived"),)),
            ("Status must be active or closed",),
        ),
        (
            "duplicate exception id",
            _parity((duplicate_id, duplicate_id)),
            ("duplicate Exception ID GL-PARITY-001",),
        ),
        (
            "duplicate active key",
            _parity((VALID_EXCEPTION, duplicate_key)),
            ("duplicate active case/source/field exception",),
        ),
        (
            "placeholder with history",
            _parity((placeholder, VALID_EXCEPTION)),
            ("remove the all-— placeholder once real rows exist",),
        ),
        (
            "duplicate placeholder",
            _parity((placeholder, placeholder)),
            ("parity ledger has more than one placeholder row",),
        ),
    )
    cases = [
        Case(
            "legacy parity header",
            valid_agents,
            valid_stabilization,
            _parity((VALID_EXCEPTION[:-1],), header=legacy_header),
            ("parity exception ledger must contain exactly one canonical header",),
        )
    ]
    cases.extend(
        Case(name, valid_agents, valid_stabilization, text, expected)
        for name, text, expected in mutations
    )
    cases.extend(
        Case(name, valid_agents, valid_stabilization, text, expected)
        for name, text, expected in extra_parity_cases(PARITY_HEADER, VALID_EXCEPTION)
    )
    return cases


def run_self_test(executable: Path) -> int:
    """Run all behavior canaries through the public executable."""

    failures: list[str] = []
    with tempfile.TemporaryDirectory(prefix="greenlit-ledger-selftest-") as directory:
        root = Path(directory)
        try:
            resolving_commit = _create_baseline_commit(root)
        except (OSError, subprocess.SubprocessError):
            print(
                "check-stabilization-ledger self-test: FAILED\n"
                "  - could not create the local Git commit baseline; "
                "check the Git installation and rerun",
                file=sys.stderr,
            )
            return 1

        nonexistent_commit = "f" * 40
        if nonexistent_commit == resolving_commit:
            nonexistent_commit = "e" * 40

        valid_agents = _agents()
        valid_stabilization = _stabilization(resolving_commit)
        valid_parity = _parity()
        placeholder_parity = _parity((tuple("—" for _ in PARITY_HEADER),))
        cases = [
            _governance_case(valid_parity, resolving_commit),
            Case(
                "nonexistent resolving commit",
                valid_agents,
                _stabilization(nonexistent_commit),
                valid_parity,
                (
                    f"Resolving commit '{nonexistent_commit}' does not name "
                    "an existing commit in the checked repository",
                    "make the commit available locally or correct the ledger row",
                ),
            ),
            *(
                Case(name, valid_agents, stabilization, parity, expected)
                for name, stabilization, parity, expected in markdown_cases(
                    valid_stabilization, valid_parity
                )
            ),
            *_parity_cases(valid_agents, valid_stabilization),
        ]

        agents = root / "AGENTS.md"
        stabilization = root / "STABILIZATION-LEDGER.md"
        parity = root / "PARITY-EXCEPTIONS.md"

        agents.write_text(valid_agents, encoding="utf-8")
        stabilization.write_text(valid_stabilization, encoding="utf-8")
        baselines = (
            ("placeholder baseline", valid_stabilization, placeholder_parity),
            ("active exception baseline", valid_stabilization, valid_parity),
            *markdown_valid_cases(valid_stabilization, valid_parity),
        )
        for name, stabilization_text, parity_text in baselines:
            stabilization.write_text(stabilization_text, encoding="utf-8")
            parity.write_text(parity_text, encoding="utf-8")
            completed = _run(executable, agents, stabilization, parity)
            if completed.returncode != 0:
                failures.append(
                    f"{name}: exit {completed.returncode}; "
                    f"stdout={completed.stdout!r}; stderr={completed.stderr!r}"
                )

        for case in cases:
            agents.write_text(case.agents, encoding="utf-8")
            stabilization.write_text(case.stabilization, encoding="utf-8")
            parity.write_text(case.parity, encoding="utf-8")
            completed = _run(executable, agents, stabilization, parity)
            output = f"{completed.stdout}\n{completed.stderr}"
            missing = [text for text in case.expected if text not in output]
            if completed.returncode != 1 or missing:
                failures.append(
                    f"{case.name}: exit {completed.returncode}, "
                    f"missing={missing!r}; stdout={completed.stdout!r}; "
                    f"stderr={completed.stderr!r}"
                )

    if failures:
        print("check-stabilization-ledger self-test: FAILED", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1
    print(
        "check-stabilization-ledger self-test: OK — "
        f"{len(baselines)} valid baselines and "
        f"{len(cases)} rejection canaries passed"
    )
    return 0
