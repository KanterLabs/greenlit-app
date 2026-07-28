"""Top-level composition for stabilization-governance validation."""

from __future__ import annotations

import sys
from dataclasses import dataclass
from pathlib import Path

from stabilization_check.ledger import parse_phase_statuses, validate_ledger

TRUSTED_REPOSITORY = "KanterLabs/greenlit-app"


@dataclass(frozen=True)
class ValidationResult:
    """The complete checker result."""

    problems: tuple[str, ...]
    defect_count: int
    exception_count: int


def _contained_governance_path(
    path: Path,
    repository: Path,
    label: str,
    problems: list[str],
) -> Path | None:
    """Resolve one governance input beneath the AGENTS repository root."""

    try:
        resolved = path.resolve()
        resolved.relative_to(repository)
    except (OSError, ValueError):
        problems.append(
            f"{path}: {label} must be contained within the AGENTS repository "
            f"root {repository}"
        )
        return None
    return resolved


def validate_paths(
    agents: Path,
    stabilization_ledger: Path,
    parity_exceptions: Path,
) -> ValidationResult:
    """Validate governance files as one consistency boundary."""

    problems: list[str] = []
    repository = agents.resolve().parent
    ledger_path = _contained_governance_path(
        stabilization_ledger,
        repository,
        "stabilization ledger",
        problems,
    )
    parity_path = _contained_governance_path(
        parity_exceptions,
        repository,
        "parity-exception ledger",
        problems,
    )
    phases = parse_phase_statuses(agents, problems)
    defect_count = (
        validate_ledger(ledger_path, phases, repository, problems)
        if ledger_path is not None
        else 0
    )

    exception_count = 0
    if parity_path is not None:
        try:
            from parity_compare.exceptions import ContractError, load_exception_ledger
        except (ImportError, AttributeError) as error:
            problems.append(
                "shared parity-exception authority is unavailable: "
                f"{type(error).__name__}: {error}"
            )
        else:
            try:
                exception_count = len(
                    load_exception_ledger(
                        parity_path,
                        repository_id=TRUSTED_REPOSITORY,
                    ).rows
                )
            except ContractError as error:
                problems.append(str(error))

    return ValidationResult(
        tuple(dict.fromkeys(problems)), defect_count, exception_count
    )


def report_result(result: ValidationResult) -> int:
    """Render a stable command result and return its process status."""

    if result.problems:
        print("check-stabilization-ledger: FAILED", file=sys.stderr)
        for problem in result.problems:
            print(f"  - {problem}", file=sys.stderr)
        return 1
    print(
        "check-stabilization-ledger: OK — "
        f"{result.defect_count} defect row(s), "
        f"{result.exception_count} parity exception row(s)"
    )
    return 0
