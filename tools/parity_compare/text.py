"""Terminal- and governance-safe Unicode text predicates."""

from __future__ import annotations

import unicodedata
from pathlib import Path


_UNSAFE_CATEGORIES = frozenset({"Cc", "Cf", "Cs", "Zl", "Zp"})
_NON_COMMONMARK_LINE_CATEGORIES = frozenset({"Cc", "Cs", "Zl", "Zp"})
_MARKUP_CHARACTERS = frozenset(r"""&<>`*_{}[]()#!~|\\""")


def is_safe_plain_text(value: str) -> bool:
    """Reject control, format, surrogate, and line-separator characters."""

    return all(
        unicodedata.category(character) not in _UNSAFE_CATEGORIES
        for character in value
    )


def is_safe_markdown_text(value: str) -> bool:
    """Require plain Unicode text with no Markdown or HTML syntax."""

    return is_safe_plain_text(value) and not any(
        character in _MARKUP_CHARACTERS for character in value
    )


def read_commonmark_lines(path: Path) -> list[str]:
    """Read strict UTF-8 using only CommonMark CR, LF, or CRLF line endings."""

    text = path.read_bytes().decode("utf-8")
    for character in text:
        if (
            character not in {"\t", "\r", "\n"}
            and unicodedata.category(character) in _NON_COMMONMARK_LINE_CATEGORIES
        ):
            codepoint = f"U+{ord(character):04X}"
            raise ValueError(
                f"{path}: non-CommonMark line/control character {codepoint}"
            )
    normalized = text.replace("\r\n", "\n").replace("\r", "\n")
    return normalized.split("\n")


__all__ = [
    "is_safe_markdown_text",
    "is_safe_plain_text",
    "read_commonmark_lines",
]
