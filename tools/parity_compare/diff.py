"""Normalize and compare validated three-party parity observations."""

from __future__ import annotations

import copy
import json
import re
from dataclasses import dataclass
from decimal import Decimal
from typing import Any, Mapping, Protocol

from parity_compare.exceptions import (
    ExceptionContractError,
    validate_exact_path,
)


class ComparisonContractError(ValueError):
    """An exception row cannot be used safely for this comparison."""


class ExceptionRow(Protocol):
    """The exception metadata consumed by the comparison layer."""

    exception_id: str
    case_id: str
    source_commit: str
    exact_field: str
    authoritative_source: str


class _Missing:
    pass


MISSING = _Missing()
ExceptionKey = tuple[str, str, str]
_NORMALIZED_EXACT_PATHS = {
    "$.producer.role",
    "$.producer.run_id",
    "$.producer.run_attempt",
    "$.producer.run_url",
    "$.producer.binary_sha256",
    "$.producer.capture_method",
    "$.producer.capture_sha256",
    "$.run.id",
    "$.run.started_at",
    "$.run.completed_at",
    "$.run.duration_ms",
    "$.run.temporary_directory",
}
_PROTECTED_PREFIXES = (
    "$.schema_version",
    "$.case_id",
    "$.source",
    "$.producer",
)


@dataclass(frozen=True)
class Mismatch:
    """One exact semantic difference between two normalized observations."""

    path: str
    oracle: Any
    observed: Any


@dataclass(frozen=True)
class ComparisonResult:
    """The unresolved differences and exceptions used by a triple comparison."""

    github_mismatches: tuple[Mismatch, ...]
    greenlit_mismatches: tuple[Mismatch, ...]
    applied_exceptions: tuple[ExceptionRow, ...]

    @property
    def matches(self) -> bool:
        """Return whether both producers match the oracle after valid exceptions."""

        return not self.github_mismatches and not self.greenlit_mismatches


def normalized_observation(observation: dict[str, Any]) -> dict[str, Any]:
    """Copy one validated observation and erase only declared dynamic evidence."""

    result = copy.deepcopy(observation)
    producer = result["producer"]
    producer["role"] = "<verified-producer-role>"
    producer["run_id"] = "<normalized-run-id>"
    producer["run_attempt"] = 0
    producer["run_url"] = "<verified-producer-run-url>"
    producer["binary_sha256"] = "<verified-release-binary>"
    producer["capture_method"] = "<verified-capture-method>"
    producer["capture_sha256"] = "<verified-capture>"

    run = result["run"]
    run["id"] = "<normalized-run-id>"
    run["started_at"] = "<normalized-timestamp>"
    run["completed_at"] = "<normalized-timestamp>"
    run["duration_ms"] = 0
    run["temporary_directory"] = "<normalized-temporary-path>"

    for job in result["jobs"]:
        job["duration_ms"] = 0
        for step in job["steps"]:
            step["duration_ms"] = 0
    for event in result["lifecycle"]:
        event["timestamp"] = "<normalized-timestamp>"
    for port in result["dynamic_ports"]:
        port["host_port"] = 0
    return result


def _json_type(value: Any) -> str:
    if value is MISSING:
        return "missing"
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "boolean"
    if isinstance(value, (int, Decimal)):
        return "number"
    if isinstance(value, str):
        return "string"
    if isinstance(value, list):
        return "array"
    if isinstance(value, dict):
        return "object"
    return f"python:{type(value).__qualname__}"


def _numbers_equal(left: int | Decimal, right: int | Decimal) -> bool:
    left_decimal = left if isinstance(left, Decimal) else Decimal(left)
    right_decimal = right if isinstance(right, Decimal) else Decimal(right)
    return left_decimal == right_decimal


def _field(parent: str, name: str) -> str:
    if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name) is not None:
        return f"{parent}.{name}"
    return f"{parent}[{json.dumps(name, ensure_ascii=True)}]"


def exact_mismatches(
    oracle: Any, observed: Any, path: str = "$"
) -> tuple[Mismatch, ...]:
    """Return every recursive JSON-semantic mismatch in deterministic order."""

    oracle_type = _json_type(oracle)
    observed_type = _json_type(observed)
    if oracle_type != observed_type:
        return (Mismatch(path, oracle, observed),)
    if oracle_type == "object":
        differences: list[Mismatch] = []
        for key in sorted(set(oracle) | set(observed)):
            differences.extend(
                exact_mismatches(
                    oracle.get(key, MISSING),
                    observed.get(key, MISSING),
                    _field(path, key),
                )
            )
        return tuple(differences)
    if oracle_type == "array":
        differences = []
        for index in range(max(len(oracle), len(observed))):
            oracle_value = oracle[index] if index < len(oracle) else MISSING
            observed_value = observed[index] if index < len(observed) else MISSING
            differences.extend(
                exact_mismatches(
                    oracle_value, observed_value, f"{path}[{index}]"
                )
            )
        return tuple(differences)
    if oracle_type == "number":
        if _numbers_equal(oracle, observed):
            return ()
    elif oracle == observed:
        return ()
    return (Mismatch(path, oracle, observed),)


def _forbidden_exception_path(path: str) -> bool:
    if path in _NORMALIZED_EXACT_PATHS:
        return True
    return any(
        path == prefix or path.startswith(f"{prefix}.")
        for prefix in _PROTECTED_PREFIXES
    )


def _validate_active_row(key: ExceptionKey, row: ExceptionRow) -> None:
    if key != (row.case_id, row.source_commit, row.exact_field):
        raise ComparisonContractError(
            f"{row.exception_id}: active exception lookup key does not match row metadata"
        )
    if _forbidden_exception_path(row.exact_field):
        raise ComparisonContractError(
            f"{row.exception_id}: identity, provenance, and normalized fields "
            "cannot be excepted"
        )
    try:
        validate_exact_path(row.exact_field, row.exception_id)
    except ExceptionContractError as error:
        raise ComparisonContractError(str(error)) from error


def _validate_exception_authority(
    row: ExceptionRow,
    github: dict[str, Any],
) -> None:
    expected = (
        f"{github['producer']['run_url']}; "
        f"source-commit={github['source']['commit']}"
    )
    if row.authoritative_source != expected:
        raise ComparisonContractError(
            f"{row.exception_id}: active authority must equal the validated "
            "GitHub Actions run URL for this comparison"
        )


def _exception_can_apply(mismatch: Mismatch) -> bool:
    oracle_type = _json_type(mismatch.oracle)
    return (
        oracle_type == _json_type(mismatch.observed)
        and oracle_type in {"null", "boolean", "number", "string"}
    )


def _validate_triple_identity(
    oracle: dict[str, Any],
    github: dict[str, Any],
    greenlit: dict[str, Any],
) -> None:
    documents = (oracle, github, greenlit)
    if len({document["case_id"] for document in documents}) != 1:
        raise ComparisonContractError(
            "all parity producers must identify the same comparison case"
        )
    if any(document["source"] != oracle["source"] for document in documents[1:]):
        raise ComparisonContractError(
            "all parity producers must identify the same immutable source"
        )
    repositories = {
        document["producer"]["repository"] for document in documents
    }
    if len(repositories) != 1:
        raise ComparisonContractError(
            "all parity producers must identify the same repository"
        )
    runners = {document["producer"]["runner"] for document in documents}
    if len(runners) != 1:
        raise ComparisonContractError(
            "all parity producers must identify the same requested runner"
        )


def compare_triple(
    oracle: dict[str, Any],
    github: dict[str, Any],
    greenlit: dict[str, Any],
    active_exceptions: Mapping[ExceptionKey, ExceptionRow],
) -> ComparisonResult:
    """Compare oracle/GitHub without waivers, then oracle/Greenlit with waivers."""

    _validate_triple_identity(oracle, github, greenlit)
    normalized_oracle = normalized_observation(oracle)
    normalized_github = normalized_observation(github)
    normalized_greenlit = normalized_observation(greenlit)
    github_differences = exact_mismatches(normalized_oracle, normalized_github)
    greenlit_differences = exact_mismatches(normalized_oracle, normalized_greenlit)

    case_id = oracle["case_id"]
    source_commit = oracle["source"]["commit"]
    current_rows: dict[ExceptionKey, ExceptionRow] = {}
    for key, row in active_exceptions.items():
        _validate_active_row(key, row)
        if row.case_id != case_id:
            continue
        if row.source_commit != source_commit:
            raise ComparisonContractError(
                f"{row.exception_id}: active exception for case {case_id!r} is "
                "bound to another source commit"
            )
        _validate_exception_authority(row, github)
        current_rows[key] = row

    unresolved: list[Mismatch] = []
    applied: list[ExceptionRow] = []
    applied_keys: set[ExceptionKey] = set()
    for mismatch in greenlit_differences:
        key = (case_id, source_commit, mismatch.path)
        row = current_rows.get(key)
        if row is None:
            unresolved.append(mismatch)
            continue
        if not _exception_can_apply(mismatch):
            raise ComparisonContractError(
                f"{row.exception_id}: exceptions apply only to present scalar "
                "leaf mismatches with the same JSON type"
            )
        applied.append(row)
        applied_keys.add(key)

    stale = sorted(set(current_rows) - applied_keys)
    if stale:
        rows = ", ".join(current_rows[key].exception_id for key in stale)
        raise ComparisonContractError(
            f"active parity exceptions are stale for this observation: {rows}"
        )
    # A drifting GitHub capture cannot authorize or apply a Greenlit exception.
    if github_differences:
        return ComparisonResult(github_differences, (), ())
    return ComparisonResult((), tuple(unresolved), tuple(applied))


def describe_value(value: Any) -> str:
    """Render JSON values, exact decimals, and missing sentinels deterministically."""

    if value is MISSING:
        return "<missing>"
    if isinstance(value, Decimal):
        return str(value)
    if isinstance(value, dict):
        return "{" + ",".join(
            f"{json.dumps(key, ensure_ascii=True)}:{describe_value(value[key])}"
            for key in sorted(value)
        ) + "}"
    if isinstance(value, list):
        return "[" + ",".join(describe_value(item) for item in value) + "]"
    return json.dumps(value, ensure_ascii=True, separators=(",", ":"))


def render_mismatch(mismatch: Mismatch, observed_label: str) -> str:
    """Render one labeled mismatch for stable command-line diagnostics."""

    return (
        f"{observed_label} {mismatch.path}: "
        f"oracle={describe_value(mismatch.oracle)} "
        f"{observed_label}={describe_value(mismatch.observed)}"
    )
