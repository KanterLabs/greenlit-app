"""Phase-status and permanent stabilization-defect validation."""

from __future__ import annotations

import os
import re
import subprocess
from pathlib import Path

from parity_compare.text import is_safe_markdown_text
from stabilization_check.markdown import LedgerFormatError, parse_table

PHASE_HEADER = ("Phase", "File", "Status")
STABILIZATION_HEADER = (
    "Defect ID",
    "Severity",
    "Owning phase",
    "User-visible impact",
    "Authoritative test or oracle",
    "Status",
    "Resolving commit",
)

VALID_PHASE_STATUSES = {"completed", "in progress", "not started"}
VALID_SEVERITIES = {"critical", "high", "medium", "low"}
VALID_DEFECT_STATUSES = {"open", "contained", "resolved"}

DEFECT_ID = re.compile(r"GL-STAB-(?P<number>[0-9]{3})")
COMMIT = re.compile(r"[0-9a-f]{7,40}")
PHASE_CELL = re.compile(r"(?P<number>[1-9][0-9]*)\s+—\s+\S.*")
PLACEHOLDER = "—"
COMMIT_CHECK_TIMEOUT_SECONDS = 5
GIT_EXECUTABLE = "/usr/bin/git"


def _repository_root(path: Path) -> Path:
    """Return the nearest worktree root containing the checked ledger."""

    resolved = path.resolve()
    for candidate in resolved.parents:
        if (candidate / ".git").exists():
            return candidate
    return resolved.parent


def require_text(
    value: str,
    path: Path,
    line: int,
    field: str,
    problems: list[str],
) -> bool:
    """Require substantive, trimmed, control-character-free text."""

    if (
        not value
        or value == PLACEHOLDER
        or value != value.strip()
        or not is_safe_markdown_text(value)
    ):
        problems.append(f"{path}:{line}: {field} must be nonempty plain text")
        return False
    return True


def parse_phase_statuses(path: Path, problems: list[str]) -> dict[int, str]:
    """Load stabilization phase numbers and statuses from ``AGENTS.md``."""

    try:
        rows = parse_table(path, "## Phase status", PHASE_HEADER)
    except LedgerFormatError as error:
        problems.append(str(error))
        return {}

    phases: dict[int, str] = {}
    for row in rows:
        phase_cell, _file_cell, status = row.cells
        match = PHASE_CELL.fullmatch(phase_cell)
        if match is None:
            problems.append(
                f"{path}:{row.line}: Phase must be '<positive-number> — <name>'"
            )
            continue
        phase = int(match.group("number"))
        if phase in phases:
            problems.append(f"{path}:{row.line}: duplicate phase {phase}")
            continue
        if status not in VALID_PHASE_STATUSES:
            problems.append(
                f"{path}:{row.line}: invalid phase status {status!r}; "
                f"expected one of {sorted(VALID_PHASE_STATUSES)!r}"
            )
            continue
        phases[phase] = status

    for phase in range(12, 29):
        if phase not in phases:
            problems.append(f"{path}: Phase status table is missing phase {phase}")
    return phases


def _commit_exists_locally(path: Path, commit: str) -> bool:
    repository = _repository_root(path)
    environment = {
        name: value
        for name, value in os.environ.items()
        if not name.startswith("GIT_")
    }
    environment.update(
        {
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_NO_LAZY_FETCH": "1",
            "GIT_OPTIONAL_LOCKS": "0",
            "GIT_TERMINAL_PROMPT": "0",
        }
    )
    completed = subprocess.run(
        [
            GIT_EXECUTABLE,
            "--no-replace-objects",
            "-c",
            f"safe.directory={repository}",
            "-C",
            str(repository),
            "cat-file",
            "-t",
            commit,
        ],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        check=False,
        timeout=COMMIT_CHECK_TIMEOUT_SECONDS,
        env=environment,
    )
    return completed.returncode == 0 and completed.stdout.strip() == "commit"


def verify_resolving_commit(
    path: Path,
    line: int,
    commit: str,
    problems: list[str],
) -> None:
    """Require a resolving hash to identify a local commit object."""

    label = f"{path}:{line}"
    try:
        exists = _commit_exists_locally(path, commit)
    except FileNotFoundError:
        problems.append(
            f"{label}: cannot verify Resolving commit {commit!r} because Git "
            "is unavailable; install Git and rerun the checker"
        )
        return
    except subprocess.TimeoutExpired:
        problems.append(
            f"{label}: local verification of Resolving commit {commit!r} "
            f"timed out after {COMMIT_CHECK_TIMEOUT_SECONDS} seconds; "
            "check the repository's local object database and rerun the checker"
        )
        return
    except OSError:
        problems.append(
            f"{label}: cannot start Git to verify Resolving commit {commit!r}; "
            "check the local Git installation and rerun the checker"
        )
        return

    if not exists:
        problems.append(
            f"{label}: Resolving commit {commit!r} does not name an existing "
            "commit in the checked repository; make the commit available "
            "locally or correct the ledger row"
        )


def validate_ledger(
    path: Path,
    phases: dict[int, str],
    problems: list[str],
) -> int:
    """Validate every stabilization-defect row and return the row count."""

    try:
        rows = parse_table(
            path, "# Greenlit stabilization ledger", STABILIZATION_HEADER
        )
    except LedgerFormatError as error:
        problems.append(str(error))
        return 0

    if not rows:
        problems.append(f"{path}: stabilization ledger must contain at least one defect")
        return 0

    seen_ids: dict[str, int] = {}
    for row in rows:
        (
            defect_id,
            severity,
            phase_raw,
            impact,
            oracle,
            status,
            resolving_commit,
        ) = row.cells
        label = f"{path}:{row.line}"

        match = DEFECT_ID.fullmatch(defect_id)
        if match is None or int(match.group("number")) == 0:
            problems.append(
                f"{label}: Defect ID {defect_id!r} must be "
                "GL-STAB-NNN with NNN nonzero"
            )
        elif defect_id in seen_ids:
            problems.append(
                f"{label}: duplicate Defect ID {defect_id!r}; "
                f"first appears at {path}:{seen_ids[defect_id]}"
            )
        else:
            seen_ids[defect_id] = row.line

        if severity not in VALID_SEVERITIES:
            problems.append(
                f"{label}: invalid Severity {severity!r}; "
                f"expected one of {sorted(VALID_SEVERITIES)!r}"
            )

        phase = int(phase_raw) if re.fullmatch(r"[0-9]+", phase_raw) else None
        if phase is None or not 12 <= phase <= 28:
            problems.append(f"{label}: Owning phase must be an integer from 12 through 28")
        elif phase not in phases:
            problems.append(f"{label}: Owning phase {phase} is absent from AGENTS.md")

        require_text(impact, path, row.line, "User-visible impact", problems)
        require_text(
            oracle, path, row.line, "Authoritative test or oracle", problems
        )

        if status not in VALID_DEFECT_STATUSES:
            problems.append(
                f"{label}: invalid defect Status {status!r}; "
                f"expected one of {sorted(VALID_DEFECT_STATUSES)!r}"
            )
        if status == "resolved":
            if COMMIT.fullmatch(resolving_commit) is None:
                problems.append(
                    f"{label}: resolved defect requires a 7–40 character "
                    "lowercase hexadecimal Resolving commit"
                )
            else:
                verify_resolving_commit(
                    path,
                    row.line,
                    resolving_commit,
                    problems,
                )
        elif status in {"open", "contained"} and resolving_commit != PLACEHOLDER:
            problems.append(
                f"{label}: non-resolved defect must use {PLACEHOLDER!r} "
                "for Resolving commit"
            )

        if phase is not None and phase in phases:
            phase_status = phases[phase]
            if phase_status == "completed" and status in {"open", "contained"}:
                problems.append(
                    f"{label}: completed phase {phase} may not own a {status} defect"
                )
            if phase_status == "not started" and status == "resolved":
                problems.append(
                    f"{label}: not-started phase {phase} may not own a resolved defect"
                )
    return len(rows)
