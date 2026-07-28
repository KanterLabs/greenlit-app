"""Strict Markdown-table parsing shared by stabilization governance checks."""

from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path

from parity_compare.text import read_commonmark_lines

SEPARATOR_CELL = re.compile(r":?-{3,}:?")
FENCE_OPEN = re.compile(
    r"^ {0,3}(?P<fence>`{3,}|~{3,})(?P<info>.*)$"
)
HTML_TYPE_1_START = re.compile(
    r"^<(?:pre|script|style|textarea)(?=[ \t>]|$)",
    re.IGNORECASE,
)
HTML_TYPE_1_END = re.compile(
    r"</(?:pre|script|style|textarea)>",
    re.IGNORECASE,
)
HTML_BLOCK_TAGS = (
    "address", "article", "aside", "base", "basefont", "blockquote", "body",
    "caption", "center", "col", "colgroup", "dd", "details", "dialog", "dir",
    "div", "dl", "dt", "fieldset", "figcaption", "figure", "footer", "form",
    "frame", "frameset", "h1", "h2", "h3", "h4", "h5", "h6", "head",
    "header", "hr", "html", "iframe", "legend", "li", "link", "main", "menu",
    "menuitem", "nav", "noframes", "ol", "optgroup", "option", "p", "param",
    "search", "section", "summary", "table", "tbody", "td", "tfoot", "th",
    "thead", "title", "tr", "track", "ul",
)
HTML_TYPE_6_START = re.compile(
    rf"^</?(?:{'|'.join(HTML_BLOCK_TAGS)})(?=[ \t]|/?>|$)",
    re.IGNORECASE,
)
TAG_NAME = r"[A-Za-z][A-Za-z0-9-]*"
ATTRIBUTE_NAME = r"[A-Za-z_:][A-Za-z0-9_.:-]*"
UNQUOTED_VALUE = r"""[^ \t"'=<>`]+"""
ATTRIBUTE_VALUE = rf"""(?:{UNQUOTED_VALUE}|'[^']*'|"[^"]*")"""
COMPLETE_OPEN_TAG = re.compile(
    rf"^<(?P<tag>{TAG_NAME})"
    rf"(?:[ \t]+{ATTRIBUTE_NAME}(?:[ \t]*=[ \t]*{ATTRIBUTE_VALUE})?)*"
    r"[ \t]*/?>[ \t]*$"
)
COMPLETE_CLOSING_TAG = re.compile(
    rf"^</{TAG_NAME}[ \t]*>[ \t]*$"
)
TYPE_7_OPEN_EXCLUSIONS = frozenset({"pre", "script", "style", "textarea"})


class LedgerFormatError(ValueError):
    """A ledger or governance table is structurally malformed."""


@dataclass(frozen=True)
class TableRow:
    """One parsed Markdown-table data row."""

    line: int
    cells: tuple[str, ...]


@dataclass(frozen=True)
class RawHtmlBlock:
    """One CommonMark raw-HTML block end condition."""

    end: re.Pattern[str] | None

    def ends_on(self, line: str) -> bool:
        """Return whether this line terminates the raw block."""

        return not line.strip() if self.end is None else self.end.search(line) is not None


def split_row(line: str, label: str) -> tuple[str, ...]:
    """Split one pipe-delimited row while honoring Markdown escapes."""

    if line != line.lstrip():
        raise LedgerFormatError(f"{label}: table row must be top-level Markdown")
    stripped = line.strip()
    if not stripped.startswith("|") or not stripped.endswith("|"):
        raise LedgerFormatError(f"{label}: expected a table row enclosed by '|'")

    cells: list[str] = []
    current: list[str] = []
    escaped = False
    for character in stripped[1:-1]:
        if escaped:
            if character in {"|", "\\"}:
                current.append(character)
            else:
                current.extend(("\\", character))
            escaped = False
        elif character == "\\":
            escaped = True
        elif character == "|":
            cells.append("".join(current).strip())
            current.clear()
        else:
            current.append(character)
    if escaped:
        current.append("\\")
    cells.append("".join(current).strip())
    return tuple(cells)


def _block_body(line: str) -> str | None:
    indentation = len(line) - len(line.lstrip(" "))
    return None if indentation > 3 else line[indentation:]


def _fence_start(line: str) -> tuple[str, int] | None:
    match = FENCE_OPEN.match(line)
    if match is None:
        return None
    fence = match.group("fence")
    if fence[0] == "`" and "`" in match.group("info"):
        return None
    return fence[0], len(fence)


def _fence_closes(line: str, fence: tuple[str, int]) -> bool:
    body = _block_body(line)
    if body is None:
        return False
    delimiter, minimum = fence
    match = re.fullmatch(r"(?P<fence>`+|~+)[ \t]*", body)
    return (
        match is not None
        and match.group("fence")[0] == delimiter
        and len(match.group("fence")) >= minimum
    )


def _raw_html_start(line: str) -> RawHtmlBlock | None:
    body = _block_body(line)
    if body is None:
        return None
    if HTML_TYPE_1_START.match(body):
        return RawHtmlBlock(HTML_TYPE_1_END)
    for start, end in (
        ("<!--", re.compile(r"-->")),
        ("<?", re.compile(r"\?>")),
        ("<![CDATA[", re.compile(r"\]\]>")),
    ):
        if body.startswith(start):
            return RawHtmlBlock(end)
    if re.match(r"^<![A-Za-z]", body):
        return RawHtmlBlock(re.compile(r">"))
    if HTML_TYPE_6_START.match(body):
        return RawHtmlBlock(None)
    open_tag = COMPLETE_OPEN_TAG.fullmatch(body)
    if (
        open_tag is not None
        and open_tag.group("tag").casefold() not in TYPE_7_OPEN_EXCLUSIONS
    ) or COMPLETE_CLOSING_TAG.fullmatch(body):
        return RawHtmlBlock(None)
    return None


def _visible_top_level(lines: list[str]) -> set[int]:
    visible: set[int] = set()
    fence: tuple[str, int] | None = None
    raw_html: RawHtmlBlock | None = None
    for index, line in enumerate(lines):
        if fence is not None:
            if _fence_closes(line, fence):
                fence = None
            continue
        if raw_html is not None:
            if raw_html.ends_on(line):
                raw_html = None
            continue
        fence = _fence_start(line)
        if fence is not None:
            continue
        raw_html = _raw_html_start(line)
        if raw_html is not None:
            if raw_html.ends_on(line):
                raw_html = None
            continue
        if line == line.lstrip():
            visible.add(index)
    return visible


def parse_table(
    path: Path,
    heading: str,
    expected_header: tuple[str, ...],
) -> list[TableRow]:
    """Read the one exact table below ``heading`` from ``path``."""

    try:
        lines = read_commonmark_lines(path)
    except (OSError, UnicodeError) as error:
        raise LedgerFormatError(
            f"{path}: could not read UTF-8 text: {error}"
        ) from error
    except ValueError as error:
        raise LedgerFormatError(str(error)) from error

    visible = _visible_top_level(lines)
    heading_indexes = [
        index for index, line in enumerate(lines) if line == heading
    ]
    if len(heading_indexes) != 1:
        raise LedgerFormatError(
            f"{path}: expected exactly one {heading!r} heading; "
            f"found {len(heading_indexes)}"
        )
    heading_index = heading_indexes[0]
    if heading_index not in visible or (
        heading.startswith("# ") and heading_index != 0
    ):
        raise LedgerFormatError(
            f"{path}: {heading!r} must be a visible top-level Markdown heading"
        )

    index = heading_index + 1
    while index < len(lines) and not lines[index].lstrip().startswith("|"):
        if lines[index].lstrip().startswith("#"):
            raise LedgerFormatError(f"{path}: {heading!r} has no table")
        index += 1
    if index >= len(lines):
        raise LedgerFormatError(f"{path}: {heading!r} has no table")

    header = split_row(lines[index], f"{path}:{index + 1}")
    if index not in visible:
        raise LedgerFormatError(
            f"{path}:{index + 1}: table must be visible top-level Markdown"
        )
    if index == 0 or lines[index - 1].strip():
        raise LedgerFormatError(
            f"{path}:{index + 1}: table header must be preceded by a blank "
            "CommonMark line"
        )
    if header != expected_header:
        raise LedgerFormatError(
            f"{path}:{index + 1}: table header is {header!r}; "
            f"expected {expected_header!r}"
        )
    index += 1
    if index >= len(lines):
        raise LedgerFormatError(f"{path}: table below {heading!r} has no separator")
    separator = split_row(lines[index], f"{path}:{index + 1}")
    if index not in visible:
        raise LedgerFormatError(
            f"{path}:{index + 1}: table must be visible top-level Markdown"
        )
    if len(separator) != len(expected_header) or any(
        SEPARATOR_CELL.fullmatch(cell) is None for cell in separator
    ):
        raise LedgerFormatError(
            f"{path}:{index + 1}: malformed Markdown table separator"
        )

    rows: list[TableRow] = []
    index += 1
    while index < len(lines) and lines[index].lstrip().startswith("|"):
        if index not in visible:
            raise LedgerFormatError(
                f"{path}:{index + 1}: table must be visible top-level Markdown"
            )
        cells = split_row(lines[index], f"{path}:{index + 1}")
        if len(cells) != len(expected_header):
            raise LedgerFormatError(
                f"{path}:{index + 1}: row has {len(cells)} cells; "
                f"expected {len(expected_header)}"
            )
        rows.append(TableRow(index + 1, cells))
        index += 1
    if index < len(lines) and lines[index].strip():
        raise LedgerFormatError(
            f"{path}:{index + 1}: non-table content immediately follows the table rows"
        )
    next_heading = next(
        (
            offset
            for offset in range(index, len(lines))
            if lines[offset].startswith("#")
        ),
        len(lines),
    )
    if any(lines[offset].lstrip().startswith("|") for offset in range(index, next_heading)):
        raise LedgerFormatError(
            f"{path}: table rows must form one contiguous visible Markdown table"
        )
    return rows
