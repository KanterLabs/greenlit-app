"""Static local-path graph extraction for reviewed non-Cargo sources."""

from __future__ import annotations

import re
import shlex
from collections import Counter
from pathlib import Path

from .model import GateError


LOCAL_TOKEN = re.compile(r"(?:^|/workspace/)(tools/[A-Za-z0-9_./-]+)$")
ARRAY_START = re.compile(
    r"^\s*(?:readonly\s+)?(?P<name>[A-Za-z_][A-Za-z0-9_]*)=\(\s*$"
)


def _shell_tokens(path: Path, line: str, line_number: int) -> tuple[str, ...]:
    lexer = shlex.shlex(line, posix=True, punctuation_chars=";&|()<>")
    lexer.whitespace_split = True
    lexer.commenters = "#"
    try:
        return tuple(lexer)
    except ValueError as error:
        raise GateError(
            f"{path}:{line_number}: authority shell is not statically "
            f"parseable: {error}"
        ) from error


def _local_token(token: str) -> str | None:
    match = LOCAL_TOKEN.search(token)
    return match.group(1) if match is not None else None


def shell_local_references(
    path: Path,
    raw: bytes,
) -> tuple[Counter[str], dict[str, tuple[str, ...]]]:
    """Return tokenized local paths and literal shell-array bindings."""

    try:
        text = raw.decode("utf-8")
    except UnicodeError as error:
        raise GateError(f"{path}: shell source must be UTF-8: {error}") from error
    references: Counter[str] = Counter()
    arrays: dict[str, list[str]] = {}
    active: str | None = None
    heredoc: tuple[str, bool] | None = None
    lines = text.splitlines()
    index = 0
    while index < len(lines):
        line_number = index + 1
        line = lines[index]
        if heredoc is not None:
            delimiter, strip_tabs = heredoc
            candidate = line.lstrip("\t") if strip_tabs else line
            if candidate == delimiter:
                heredoc = None
            index += 1
            continue
        logical = line
        while logical.rstrip().endswith("\\"):
            logical = logical.rstrip()[:-1]
            index += 1
            if index >= len(lines):
                raise GateError(
                    f"{path}:{line_number}: authority shell continuation "
                    "has no following line"
                )
            logical += " " + lines[index].lstrip()
        visible = logical.split("#", 1)[0]
        heredoc_match = re.search(
            r"<<(?P<tabs>-?)[ \t]*['\"]?"
            r"(?P<name>[A-Za-z_][A-Za-z0-9_]*)['\"]?",
            visible,
        )
        if heredoc_match is not None:
            heredoc = (
                heredoc_match.group("name"),
                heredoc_match.group("tabs") == "-",
            )
        if active is None:
            match = ARRAY_START.match(logical)
            if match is not None:
                active = match.group("name")
                if active in arrays:
                    raise GateError(
                        f"{path}:{line_number}: shell array {active!r} is repeated"
                    )
                arrays[active] = []
                index += 1
                continue
        elif logical.strip() == ")":
            active = None
            index += 1
            continue
        if "tools/" in visible or active is not None:
            for token in _shell_tokens(path, logical, line_number):
                local = _local_token(token)
                if local is not None:
                    references[local] += 1
                    if active is not None:
                        arrays[active].append(local)
        index += 1
    if active is not None:
        raise GateError(f"{path}: shell array {active!r} is unterminated")
    if heredoc is not None:
        raise GateError(f"{path}: authority shell has an unterminated heredoc")
    return references, {
        name: tuple(values)
        for name, values in arrays.items()
    }
