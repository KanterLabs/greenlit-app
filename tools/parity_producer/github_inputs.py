"""Bounded decoding of GitHub content and log API responses."""

from __future__ import annotations

import base64
import binascii
import io
import zipfile
from pathlib import Path
from typing import Any

from parity_producer.common import (
    WORKFLOW_PATH,
    ProducerError,
    read_regular_file,
    require_fields,
    require_string,
)


MAX_LOG_ARCHIVE_BYTES = 32 * 1024 * 1024
MAX_LOG_BYTES = 64 * 1024 * 1024
MAX_LOG_ENTRIES = 32


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


def log_lines(archive: bytes) -> list[str]:
    """Extract bounded UTF-8 lines from one Actions log ZIP."""
    if len(archive) > MAX_LOG_ARCHIVE_BYTES:
        raise ProducerError("GitHub log archive exceeds the compressed safety limit")
    try:
        zipped = zipfile.ZipFile(io.BytesIO(archive))
    except (zipfile.BadZipFile, OSError) as error:
        raise ProducerError("GitHub log response is not a valid ZIP archive") from error
    infos = zipped.infolist()
    if not infos or len(infos) > MAX_LOG_ENTRIES:
        raise ProducerError("GitHub log archive has an invalid entry count")
    total = 0
    lines: list[str] = []
    for info in sorted(infos, key=lambda item: item.filename):
        if info.is_dir():
            continue
        if info.flag_bits & 0x1:
            raise ProducerError("GitHub log archive contains an encrypted entry")
        total += info.file_size
        if total > MAX_LOG_BYTES:
            raise ProducerError("GitHub log archive exceeds the expanded safety limit")
        try:
            raw = zipped.read(info)
            text = raw.decode("utf-8")
        except (OSError, RuntimeError, UnicodeDecodeError, zipfile.BadZipFile) as error:
            raise ProducerError(
                f"cannot read GitHub log archive entry {info.filename!r}"
            ) from error
        lines.extend(text.splitlines())
    return lines


def read_archive(path: Path) -> bytes:
    """Read an explicitly exported bounded Actions log archive."""
    return read_regular_file(path, "GitHub logs export", MAX_LOG_ARCHIVE_BYTES)
