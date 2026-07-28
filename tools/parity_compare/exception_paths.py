"""Canonical, leaf-scoped JSONPath rules for parity exceptions."""

from __future__ import annotations

import json
import re

from .text import is_safe_markdown_text


_INDEX = r"\[(?:0|[1-9][0-9]*)\]"
_MEMBER = r'\["(?:[^"\\\x00-\x1f]|\\["\\/bfnrt]|\\u[0-9A-Fa-f]{4})*"\]'
_SEGMENT = rf"(?:\.[A-Za-z_][A-Za-z0-9_]*|{_INDEX}|{_MEMBER})"
_JSON_PATH = re.compile(rf"^\${_SEGMENT}+$")
_PROTECTED_ROOTS = (
    "$.schema_version",
    "$.case_id",
    "$.source",
    "$.producer",
    "$.lifecycle",
)
_NORMALIZED_PATHS = {
    "$.run.id",
    "$.run.started_at",
    "$.run.completed_at",
    "$.run.duration_ms",
    "$.run.temporary_directory",
}
_VALUE_TAIL = rf"{_SEGMENT}*"
_EXCEPTIONABLE_PATHS = tuple(
    re.compile(pattern)
    for pattern in (
        rf"^\$\.contexts{_INDEX}\.value{_VALUE_TAIL}$",
        rf"^\$\.outputs{_INDEX}\.value{_VALUE_TAIL}$",
        rf"^\$\.jobs{_INDEX}\.name$",
        rf"^\$\.jobs{_INDEX}\.outputs{_INDEX}\.value{_VALUE_TAIL}$",
        rf"^\$\.jobs{_INDEX}\.steps{_INDEX}\.name$",
        rf"^\$\.jobs{_INDEX}\.steps{_INDEX}\.outputs{_INDEX}"
        rf"\.value{_VALUE_TAIL}$",
        rf"^\$\.filesystem_probes{_INDEX}\.(?:kind|exists|mode|sha256)$",
        rf"^\$\.resource_security_findings{_INDEX}\.(?:category|detail)$",
        rf"^\$\.dynamic_ports{_INDEX}\.(?:container_port|protocol)$",
    )
)
_RECORD_SCOPES = tuple(
    re.compile(pattern)
    for pattern in (
        rf"^(?P<scope>\$\.jobs{_INDEX}\.steps{_INDEX}\.outputs{_INDEX})",
        rf"^(?P<scope>\$\.jobs{_INDEX}\.steps{_INDEX})",
        rf"^(?P<scope>\$\.jobs{_INDEX}\.outputs{_INDEX})",
        rf"^(?P<scope>\$\.jobs{_INDEX})",
        rf"^(?P<scope>\$\.(?:contexts|outputs|filesystem_probes|"
        rf"resource_security_findings|dynamic_ports){_INDEX})",
    )
)


class ExceptionContractError(ValueError):
    """A stable parity-exception ledger contract failure."""


def record_scope(value: str) -> str:
    """Return the semantic record that owns one canonical exception leaf."""

    for pattern in _RECORD_SCOPES:
        match = pattern.match(value)
        if match is not None:
            return match.group("scope")
    raise ExceptionContractError(
        "parity exception: Exact field has no V1 semantic record scope"
    )


def _canonical_json_path(value: str) -> str:
    rendered = "$"
    for match in re.finditer(_SEGMENT, value[1:]):
        segment = match.group(0)
        if segment.startswith('["'):
            try:
                member = json.loads(segment[1:-1])
            except (json.JSONDecodeError, TypeError) as error:
                raise ExceptionContractError(
                    "parity exception: invalid JSONPath member spelling"
                ) from error
            if not isinstance(member, str):
                raise ExceptionContractError(
                    "parity exception: invalid JSONPath member spelling"
                )
            if not is_safe_markdown_text(member):
                raise ExceptionContractError(
                    "parity exception: JSONPath members must be safe "
                    "Markdown plain text"
                )
            if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", member):
                rendered += f".{member}"
            else:
                encoded = json.dumps(
                    member, ensure_ascii=True, separators=(",", ":")
                )
                rendered += f"[{encoded}]"
        else:
            rendered += segment
    return rendered


def validate_exact_path(value: str, label: str = "parity exception") -> None:
    """Require one canonical V1 semantic scalar leaf."""

    if _JSON_PATH.fullmatch(value) is None:
        raise ExceptionContractError(
            f"{label}: Exact field must be one canonical leaf JSONPath"
        )
    if _canonical_json_path(value) != value:
        raise ExceptionContractError(
            f"{label}: Exact field must use canonical comparator JSONPath spelling"
        )
    if any(
        value == root or value.startswith(f"{root}.")
        for root in _PROTECTED_ROOTS
    ):
        raise ExceptionContractError(
            f"{label}: schema, case, source, producer, and lifecycle fields "
            "cannot be excepted"
        )
    if (
        value in _NORMALIZED_PATHS
        or re.fullmatch(
            rf"\$\.jobs{_INDEX}(?:\.steps{_INDEX})?\.duration_ms", value
        )
        or re.fullmatch(rf"\$\.dynamic_ports{_INDEX}\.host_port", value)
    ):
        raise ExceptionContractError(
            f"{label}: normalized-only fields cannot be excepted"
        )
    if not any(pattern.fullmatch(value) for pattern in _EXCEPTIONABLE_PATHS):
        raise ExceptionContractError(
            f"{label}: Exact field must identify a V1 semantic scalar leaf, "
            "not a record, collection, identity, reference, or unknown field"
        )


__all__ = [
    "ExceptionContractError",
    "record_scope",
    "validate_exact_path",
]
