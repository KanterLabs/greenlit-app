"""Strict JSON and scalar validators for parity observations."""

from __future__ import annotations

import datetime as dt
import json
import re
from decimal import Decimal, InvalidOperation
from pathlib import Path
from typing import Any, Callable

from . import ContractError


IDENTIFIER = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._:/-]*$")
CASE_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
COMMIT = re.compile(r"^[0-9a-f]{40}$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")
MODE = re.compile(r"^0[0-7]{3}$")
REPOSITORY = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
RFC3339 = re.compile(
    r"^(?P<date>[0-9]{4}-[0-9]{2}-[0-9]{2})"
    r"T(?P<time>[0-9]{2}:[0-9]{2}:[0-9]{2})"
    r"(?P<fraction>\.[0-9]{1,6})?(?P<offset>Z|[+-][0-9]{2}:[0-9]{2})$"
)
CONCLUSIONS = frozenset(
    {
        "success",
        "failure",
        "cancelled",
        "skipped",
        "timed_out",
        "neutral",
        "action_required",
        "stale",
        "blocked",
        "preparation-failed",
        "aborted",
    }
)


class JsonInteger(Decimal):
    """An arbitrary-precision number parsed from JSON integer syntax."""


def field(path: str, key: str) -> str:
    """Return the unambiguous JSON path for an object member."""
    if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", key):
        return f"{path}.{key}"
    return f"{path}[{json.dumps(key, ensure_ascii=True)}]"


def _strict_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ContractError(f"$: duplicate JSON object key {key!r}")
        result[key] = value
    return result


def _reject_constant(value: str) -> Any:
    raise ContractError(f"$: non-JSON numeric constant {value!r}")


def _decimal_number(value: str) -> Decimal:
    try:
        number = Decimal(value)
    except InvalidOperation as error:
        raise ContractError(f"$: invalid JSON number {value!r}") from error
    if not number.is_finite():
        raise ContractError(f"$: non-finite JSON number {value!r}")
    return number


def _integer_number(value: str) -> JsonInteger:
    try:
        return JsonInteger(value)
    except InvalidOperation as error:
        raise ContractError(f"$: invalid JSON integer {value!r}") from error


def load_json_document(path: Path, role: str) -> Any:
    """Load one strict JSON document while retaining exact fractional numbers."""
    try:
        raw = path.read_bytes()
    except FileNotFoundError as error:
        raise ContractError(f"{role} observation file is missing: {path}") from error
    except OSError as error:
        raise ContractError(f"cannot read {role} observation {path}: {error}") from error
    return load_json_bytes(raw, role, str(path))


def load_json_bytes(raw: bytes, role: str, source: str) -> Any:
    """Parse exact strict JSON bytes from a securely opened observation."""

    try:
        text = raw.decode("utf-8", errors="strict")
    except UnicodeDecodeError as error:
        raise ContractError(f"{role} observation is not UTF-8: {source}") from error
    if not text.strip():
        raise ContractError(
            f"{role} observation is missing from empty input: {source}"
        )
    try:
        return json.loads(
            text,
            object_pairs_hook=_strict_object,
            parse_constant=_reject_constant,
            parse_float=_decimal_number,
            parse_int=_integer_number,
        )
    except json.JSONDecodeError as error:
        raise ContractError(
            f"{role} observation is not valid JSON at line {error.lineno}, "
            f"column {error.colno}: {error.msg}"
        ) from error
    except RecursionError as error:
        raise ContractError(
            f"{role} observation exceeds supported JSON nesting: {source}"
        ) from error
    except ValueError as error:
        raise ContractError(
            f"{role} observation contains an invalid number: {error}"
        ) from error
    except ContractError as error:
        raise ContractError(f"{role} observation {error}") from error


def require_object(value: Any, path: str, fields: set[str]) -> dict[str, Any]:
    """Require an object with exactly the named fields."""
    if not isinstance(value, dict):
        raise ContractError(f"{path}: expected object")
    unknown = sorted(set(value) - fields)
    if unknown:
        raise ContractError(f"{field(path, unknown[0])}: unknown field")
    missing = sorted(fields - set(value))
    if missing:
        raise ContractError(f"{field(path, missing[0])}: missing observation field")
    return value


def require_array(value: Any, path: str) -> list[Any]:
    """Require an array."""
    if not isinstance(value, list):
        raise ContractError(f"{path}: expected array")
    return value


def require_string(value: Any, path: str) -> str:
    """Require a non-empty string."""
    if not isinstance(value, str) or not value:
        raise ContractError(f"{path}: expected non-empty string")
    return value


def require_identifier(value: Any, path: str) -> str:
    """Require a stable observation identity."""
    text = require_string(value, path)
    if IDENTIFIER.fullmatch(text) is None:
        raise ContractError(f"{path}: invalid observation identity {text!r}")
    return text


def require_integer(
    value: Any, path: str, minimum: int = 0, maximum: int | None = None
) -> int | JsonInteger:
    """Require a bounded JSON integer."""
    if (
        isinstance(value, bool)
        or not isinstance(value, (int, JsonInteger))
    ):
        raise ContractError(f"{path}: expected integer")
    if value < minimum or (maximum is not None and value > maximum):
        bound = f"{minimum}..{maximum}" if maximum is not None else f">= {minimum}"
        raise ContractError(f"{path}: expected integer in range {bound}")
    return value


def require_duration(value: Any, path: str) -> Decimal:
    """Require an exact, finite, non-negative millisecond duration."""
    if isinstance(value, bool) or not isinstance(value, (int, Decimal)):
        raise ContractError(f"{path}: expected non-negative duration")
    number = Decimal(value)
    if not number.is_finite() or number < 0:
        raise ContractError(f"{path}: expected non-negative finite duration")
    return number


def require_timestamp(value: Any, path: str) -> dt.datetime:
    """Require an offset-bearing RFC 3339 timestamp at microsecond precision."""
    text = require_string(value, path)
    if RFC3339.fullmatch(text) is None:
        raise ContractError(f"{path}: expected canonical RFC 3339 timestamp")
    try:
        parsed = dt.datetime.fromisoformat(text.replace("Z", "+00:00"))
        normalized = parsed.astimezone(dt.timezone.utc)
    except (OverflowError, ValueError) as error:
        raise ContractError(f"{path}: expected canonical RFC 3339 timestamp") from error
    if parsed.tzinfo is None:
        raise ContractError(f"{path}: timestamp must include an offset")
    return normalized


def require_conclusion(value: Any, path: str) -> str:
    """Require a declared GitHub conclusion token."""
    text = require_string(value, path)
    if text not in CONCLUSIONS:
        raise ContractError(f"{path}: unknown conclusion {text!r}")
    return text


def require_nullable_string(value: Any, path: str) -> str | None:
    """Require null or a non-empty string."""
    if value is None:
        return None
    return require_string(value, path)


def validate_identity_records(
    records: list[Any],
    path: str,
    validator: Callable[[Any, str], str],
    *,
    sorted_ids: bool,
) -> None:
    """Validate unique record identities and optionally canonical ordering."""
    identities: list[str] = []
    seen: set[str] = set()
    for index, value in enumerate(records):
        record_path = f"{path}[{index}]"
        identity = validator(value, record_path)
        if identity in seen:
            raise ContractError(f"{record_path}.id: duplicate identity {identity!r}")
        seen.add(identity)
        identities.append(identity)
    if sorted_ids and identities != sorted(identities):
        raise ContractError(f"{path}: observations must be sorted by id")


def elapsed_milliseconds(started: dt.datetime, completed: dt.datetime) -> Decimal:
    """Return exact elapsed milliseconds without a binary floating conversion."""
    elapsed = completed - started
    seconds = elapsed.days * 86_400 + elapsed.seconds
    return Decimal(seconds * 1000) + Decimal(elapsed.microseconds) / Decimal(1000)


__all__ = [
    "CASE_ID",
    "COMMIT",
    "CONCLUSIONS",
    "JsonInteger",
    "MODE",
    "REPOSITORY",
    "SHA256",
    "elapsed_milliseconds",
    "field",
    "load_json_document",
    "require_array",
    "require_conclusion",
    "require_duration",
    "require_identifier",
    "require_integer",
    "require_nullable_string",
    "require_object",
    "require_string",
    "require_timestamp",
    "validate_identity_records",
]
