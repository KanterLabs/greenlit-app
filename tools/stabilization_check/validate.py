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


def validate_paths(
    agents: Path,
    stabilization_ledger: Path,
    parity_exceptions: Path,
) -> ValidationResult:
    """Validate governance files as one consistency boundary."""

    problems: list[str] = []
    phases = parse_phase_statuses(agents, problems)
    defect_count = validate_ledger(stabilization_ledger, phases, problems)

    exception_count = 0
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
                    parity_exceptions,
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
