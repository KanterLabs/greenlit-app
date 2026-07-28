"""Bounded decoding of GitHub content and log API responses."""

from __future__ import annotations

import base64
import binascii
from pathlib import Path
from typing import Any

from parity_producer.common import (
    WORKFLOW_PATH,
    ProducerError,
    read_regular_file,
    require_fields,
    require_string,
)


MAX_JOB_LOG_BYTES = 8 * 1024 * 1024


def content_bytes(response: dict[str, Any]) -> bytes:
    """Decode exact workflow bytes returned by GitHub's contents API."""
    require_fields(
        response,
        "GitHub content response",
        {"type", "path", "encoding", "content"},
        allow_extra=True,
    )
    if (
        response["type"] != "file"
        or response["path"] != WORKFLOW_PATH
        or response["encoding"] != "base64"
    ):
        raise ProducerError("GitHub content response is not the exact parity workflow file")
    encoded = require_string(response["content"], "GitHub workflow content")
    compact = "".join(encoded.split())
    try:
        raw = base64.b64decode(compact, validate=True)
    except (ValueError, binascii.Error) as error:
        raise ProducerError("GitHub workflow content is not valid base64") from error
    if not raw or len(raw) > 1024 * 1024:
        raise ProducerError("GitHub parity workflow content is empty or oversized")
    return raw


def job_log_lines(raw: bytes) -> list[str]:
    """Decode one bounded plain-text workflow-job log."""
    if not raw:
        raise ProducerError("GitHub job log is empty")
    if len(raw) > MAX_JOB_LOG_BYTES:
        raise ProducerError("GitHub job log exceeds the expanded safety limit")
    try:
        return raw.decode("utf-8-sig").splitlines()
    except UnicodeDecodeError as error:
        raise ProducerError("GitHub job log is not valid UTF-8") from error


def read_job_log(path: Path) -> bytes:
    """Read an explicitly exported bounded workflow-job log."""
    return read_regular_file(path, "GitHub job-log export", MAX_JOB_LOG_BYTES)
