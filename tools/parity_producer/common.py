"""Shared validation and publication primitives for parity producers."""

from __future__ import annotations

import datetime as dt
import hashlib
import json
import os
import re
import stat
from decimal import Decimal, InvalidOperation
from pathlib import Path
from typing import Any


CASE_ID = "shell-only-seed"
SCHEMA_VERSION = "ParityObservationV1"
CAPTURE_VERSION = "ParityCaptureV1"
WORKFLOW_PATH = ".github/workflows/parity-seed.yml"
RUNNER = "homelab"
AUTHORITATIVE_REPOSITORY = "KanterLabs/greenlit-app"
COMMIT = re.compile(r"^[0-9a-f]{40}$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")
RFC3339 = re.compile(
    r"^(?P<date>[0-9]{4}-[0-9]{2}-[0-9]{2})T"
    r"(?P<time>[0-9]{2}:[0-9]{2}:[0-9]{2})"
    r"(?P<fraction>\.[0-9]{1,9})?"
    r"(?P<offset>Z|[+-][0-9]{2}:[0-9]{2})$"
)
MAX_JSON_BYTES = 8 * 1024 * 1024
MAX_INTEGER = (1 << 63) - 1
MAX_GIT_OUTPUT_BYTES = 32 * 1024 * 1024
GIT_EXECUTABLE = "/usr/bin/git"
GIT_FIXED_OPTIONS = (
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
)


class ProducerError(Exception):
    """A fail-closed producer input or execution error."""


def git_environment() -> dict[str, str]:
    """Return an environment that cannot redirect trusted Git object reads."""
    return {
        "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_NO_REPLACE_OBJECTS": "1",
        "GIT_TERMINAL_PROMPT": "0",
        "HOME": "/",
        "LANG": "C",
        "LC_ALL": "C",
        "PATH": "/usr/bin:/bin",
        "TZ": "UTC",
    }


def strict_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    """Reject duplicate JSON keys while loading API or retained evidence."""
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ProducerError(f"duplicate JSON object key {key!r}")
        result[key] = value
    return result


def reject_constant(value: str) -> None:
    """Reject non-JSON numeric constants accepted by Python by default."""
    raise ProducerError(f"non-JSON numeric constant {value!r}")


def parse_decimal(value: str) -> Decimal:
    """Parse a finite JSON decimal without binary-float overflow."""
    try:
        number = Decimal(value)
    except InvalidOperation as error:
        raise ProducerError(f"invalid JSON number {value!r}") from error
    if not number.is_finite():
        raise ProducerError(f"non-finite JSON number {value!r}")
    return number


def parse_integer(value: str) -> int:
    """Parse a JSON integer without Python's process-global digit limit."""
    return int(parse_decimal(value))


def load_json_bytes(raw: bytes, source: str) -> Any:
    """Load one bounded, strict UTF-8 JSON document."""
    if len(raw) > MAX_JSON_BYTES:
        raise ProducerError(f"{source} exceeds the {MAX_JSON_BYTES}-byte safety limit")
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ProducerError(f"{source} is not UTF-8 JSON") from error
    try:
        return json.loads(
            text,
            object_pairs_hook=strict_object,
            parse_float=parse_decimal,
            parse_int=parse_integer,
            parse_constant=reject_constant,
        )
    except json.JSONDecodeError as error:
        raise ProducerError(
            f"{source} is invalid JSON at line {error.lineno}, "
            f"column {error.colno}: {error.msg}"
        ) from error
    except (ValueError, RecursionError) as error:
        raise ProducerError(f"{source} is invalid JSON: {error}") from error


def load_json(path: Path, source: str) -> Any:
    """Load one strict JSON document from a regular file."""
    raw = read_regular_file(path, source, MAX_JSON_BYTES)
    return load_json_bytes(raw, source)


def read_regular_file(
    path: Path,
    source: str,
    limit: int,
    *,
    required_mode: int | None = None,
    required_owner: int | None = None,
    required_links: int | None = None,
) -> bytes:
    """Read a bounded regular file without following any path symlink."""
    absolute = Path(os.path.abspath(path))
    directory_flags = os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC
    file_flags = os.O_RDONLY | os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        directory_flags |= os.O_NOFOLLOW
        file_flags |= os.O_NOFOLLOW
    parent_descriptor = -1
    descriptor = -1
    try:
        parent_descriptor = os.open(absolute.anchor, directory_flags)
        for part in absolute.parts[1:-1]:
            child = os.open(part, directory_flags, dir_fd=parent_descriptor)
            os.close(parent_descriptor)
            parent_descriptor = child
        descriptor = os.open(
            absolute.parts[-1],
            file_flags,
            dir_fd=parent_descriptor,
        )
    except OSError as error:
        if descriptor >= 0:
            os.close(descriptor)
        if parent_descriptor >= 0:
            os.close(parent_descriptor)
        raise ProducerError(f"cannot open {source} at {absolute}: {error}") from error
    try:
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode):
            raise ProducerError(f"{source} is not a regular file: {absolute}")
        if (
            required_mode is not None
            and stat.S_IMODE(metadata.st_mode) != required_mode
        ):
            raise ProducerError(
                f"{source} must have mode {required_mode:04o}: {absolute}"
            )
        if required_owner is not None and metadata.st_uid != required_owner:
            raise ProducerError(
                f"{source} must be owned by uid {required_owner}: {absolute}"
            )
        if required_links is not None and metadata.st_nlink != required_links:
            raise ProducerError(
                f"{source} must have {required_links} filesystem link: {absolute}"
            )
        if metadata.st_size > limit:
            raise ProducerError(f"{source} exceeds the {limit}-byte safety limit")
        with os.fdopen(descriptor, "rb", closefd=True) as handle:
            descriptor = -1
            raw = handle.read(limit + 1)
    except OSError as error:
        raise ProducerError(f"cannot read {source} at {path}: {error}") from error
    finally:
        if descriptor >= 0:
            os.close(descriptor)
        if parent_descriptor >= 0:
            os.close(parent_descriptor)
    if len(raw) > limit:
        raise ProducerError(f"{source} exceeds the {limit}-byte safety limit")
    return raw


def require_object(value: Any, source: str) -> dict[str, Any]:
    """Require a JSON object."""
    if not isinstance(value, dict):
        raise ProducerError(f"{source} must be a JSON object")
    return value


def require_string(value: Any, source: str) -> str:
    """Require a non-empty string."""
    if not isinstance(value, str) or not value:
        raise ProducerError(f"{source} must be a non-empty string")
    return value


def require_integer(value: Any, source: str, minimum: int = 0) -> int:
    """Require an integer at or above a lower bound."""
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or value < minimum
        or value > MAX_INTEGER
    ):
        raise ProducerError(
            f"{source} must be an integer from {minimum} through {MAX_INTEGER}"
        )
    return value


def require_fields(
    value: dict[str, Any],
    source: str,
    required: set[str],
    *,
    allow_extra: bool,
) -> None:
    """Require named fields and optionally reject undeclared fields."""
    missing = sorted(required - set(value))
    if missing:
        raise ProducerError(f"{source} is missing field {missing[0]!r}")
    if not allow_extra:
        extra = sorted(set(value) - required)
        if extra:
            raise ProducerError(f"{source} has unknown field {extra[0]!r}")


def parse_timestamp(value: Any, source: str) -> dt.datetime:
    """Parse a strict RFC 3339 timestamp into UTC."""
    text = require_string(value, source)
    if RFC3339.fullmatch(text) is None:
        raise ProducerError(f"{source} is not a strict RFC 3339 timestamp")
    normalized = text[:-1] + "+00:00" if text.endswith("Z") else text
    try:
        parsed = dt.datetime.fromisoformat(normalized)
    except ValueError as error:
        raise ProducerError(f"{source} is not a valid timestamp") from error
    return parsed.astimezone(dt.timezone.utc)


def format_timestamp(value: dt.datetime) -> str:
    """Render a UTC timestamp in the canonical RFC 3339 form."""
    utc = value.astimezone(dt.timezone.utc)
    if utc.microsecond:
        return utc.isoformat(timespec="milliseconds").replace("+00:00", "Z")
    return utc.isoformat(timespec="seconds").replace("+00:00", "Z")


def timestamp_from_unix_ms(value: Any, source: str) -> str:
    """Convert a retained non-negative Unix-millisecond timestamp."""
    milliseconds = require_integer(value, source)
    try:
        instant = dt.datetime.fromtimestamp(milliseconds / 1000, tz=dt.timezone.utc)
    except (OverflowError, OSError, ValueError) as error:
        raise ProducerError(f"{source} is outside the supported timestamp range") from error
    return format_timestamp(instant)


def duration_ms(start: dt.datetime, completed: dt.datetime, source: str) -> int:
    """Calculate a non-negative integral duration."""
    delta = completed - start
    value = round(delta.total_seconds() * 1000)
    if value < 0:
        raise ProducerError(f"{source} completes before it starts")
    return value


def sha256_bytes(raw: bytes) -> str:
    """Return a lowercase SHA-256 hex digest."""
    return hashlib.sha256(raw).hexdigest()


def strip_sha256_prefix(value: Any, source: str) -> str:
    """Accept Greenlit's `sha256:` identity and return its canonical hex."""
    text = require_string(value, source)
    digest = text.removeprefix("sha256:")
    if SHA256.fullmatch(digest) is None:
        raise ProducerError(f"{source} is not a SHA-256 identity")
    return digest


def canonical_observation(
    *,
    producer: dict[str, Any],
    source: dict[str, Any],
    run: dict[str, Any],
    contexts: list[dict[str, Any]],
    job: dict[str, Any],
    lifecycle: list[dict[str, Any]],
    probe: dict[str, Any],
) -> dict[str, Any]:
    """Assemble the fixed Phase 12 seed contract."""
    return {
        "schema_version": SCHEMA_VERSION,
        "case_id": CASE_ID,
        "producer": producer,
        "source": source,
        "run": run,
        "contexts": sorted(contexts, key=lambda record: record["id"]),
        "outputs": [],
        "jobs": [job],
        "lifecycle": lifecycle,
        "filesystem_probes": [probe],
        "resource_security_findings": [],
        "dynamic_ports": [],
    }
