"""Semantic bans for reviewed non-Cargo capability harness sources."""

from __future__ import annotations

import re
import shlex
from pathlib import Path

from .model import GateError
from .noncargo_python_semantics import validate_python_semantics


CONTROL_TOKENS = {"&", "&&", "(", ")", ";", ";;", "|", "||"}
STATUS_NAMES = {"cleanup_status", "original_status", "status"}
HEREDOC_PATTERN = re.compile(
    r"<<(?P<tabs>-?)[ \t]*(?P<quote>['\"]?)"
    r"(?P<delimiter>[A-Za-z_][A-Za-z0-9_]*)(?P=quote)"
)


def _decode(path: Path, raw: bytes) -> str:
    try:
        return raw.decode("utf-8")
    except UnicodeError as error:
        raise GateError(
            f"{path}: reviewed harness source must be UTF-8: {error}"
        ) from error


def _shell_tokens(path: Path, line: str, line_number: int) -> list[str]:
    lexer = shlex.shlex(
        line,
        posix=True,
        punctuation_chars=";&|()<>",
    )
    lexer.whitespace_split = True
    lexer.commenters = "#"
    try:
        return list(lexer)
    except ValueError as error:
        raise GateError(
            f"{path}:{line_number}: invalid reviewed shell syntax: {error}"
        ) from error


def _before_shell_comment(line: str) -> str:
    """Return shell text before an unquoted token-leading comment."""

    single = False
    double = False
    escaped = False
    for index, character in enumerate(line):
        if escaped:
            escaped = False
            continue
        if character == "\\" and not single:
            escaped = True
            continue
        if character == "'" and not double:
            single = not single
            continue
        if character == '"' and not single:
            double = not double
            continue
        if (
            character == "#"
            and not single
            and not double
            and (index == 0 or line[index - 1].isspace())
        ):
            return line[:index]
    return line


def _without_heredoc_bodies(path: Path, source: str) -> str:
    lines = source.splitlines()
    result: list[str] = []
    pending: list[tuple[str, bool]] = []
    for line_number, line in enumerate(lines, start=1):
        if pending:
            delimiter, strip_tabs = pending[0]
            candidate = line.lstrip("\t") if strip_tabs else line
            if candidate == delimiter:
                pending.pop(0)
            result.append("")
            continue
        visible = _before_shell_comment(line)
        for match in HEREDOC_PATTERN.finditer(visible):
            pending.append(
                (
                    match.group("delimiter"),
                    match.group("tabs") == "-",
                )
            )
        result.append(line)
    if pending:
        raise GateError(f"{path}: reviewed shell source has an unterminated heredoc")
    return "\n".join(result)


def _status_passthrough(argument: str, source: str) -> bool:
    match = re.fullmatch(r"\$\{?([A-Za-z_][A-Za-z0-9_]*)\}?", argument)
    if match is None or match.group(1) not in STATUS_NAMES:
        return False
    name = match.group(1)
    return re.search(
        rf"(?:^|[;\n])\s*(?:local\s+)?{re.escape(name)}="
        r"['\"]?\$\?['\"]?",
        source,
    ) is not None


def _shell_exit_failure(
    path: Path,
    line_number: int,
    argument_tokens: list[str],
    source: str,
) -> None:
    if not argument_tokens:
        raise GateError(
            f"{path}:{line_number}: non-Cargo harness immediate success: "
            "no-argument shell exit can self-skip a required capability"
        )
    first = argument_tokens[0]
    if re.fullmatch(r"[+]?0+", first):
        raise GateError(
            f"{path}:{line_number}: non-Cargo harness immediate success: "
            "zero shell exit can self-skip a required capability"
        )
    if (
        len(argument_tokens) >= 4
        and argument_tokens[:4] == ["$", "((", "0", "))"]
    ):
        raise GateError(
            f"{path}:{line_number}: non-Cargo harness immediate success: "
            "computed-zero shell exit can self-skip a required capability"
        )
    if re.fullmatch(r"[+]?[1-9][0-9]*", first):
        return
    if _status_passthrough(first, source):
        return
    raise GateError(
        f"{path}:{line_number}: shell exit status is not a fixed nonzero "
        "failure or a directly preserved command status"
    )


def _inspect_shell_tokens(
    path: Path,
    relative: str,
    line_number: int,
    tokens: list[str],
    source: str,
    previous: str,
    next_line: str,
) -> None:
    for index, token in enumerate(tokens):
        if token == "trap" and index + 1 < len(tokens):
            action = tokens[index + 1]
            nested = _shell_tokens(path, action, line_number)
            _inspect_shell_tokens(
                path,
                relative,
                line_number,
                nested,
                action,
                "",
                "",
            )
        if token != "exit":
            continue
        stop = index + 1
        arguments: list[str] = []
        while stop < len(tokens) and tokens[stop] not in CONTROL_TOKENS:
            arguments.append(tokens[stop])
            stop += 1
        allowed_finalize = (
            relative == "tools/release-check"
            and arguments == ["0"]
            and previous == "finalize_release"
            and next_line == "fi"
        )
        if not allowed_finalize:
            _shell_exit_failure(path, line_number, arguments, source)


def _shell_semantics(path: Path, relative: str, raw: bytes) -> None:
    """Reject shell success exits without trusting comments or heredoc bodies."""

    source = _decode(path, raw)
    analyzed = _without_heredoc_bodies(path, source)
    if relative == "tools/release-check":
        expected = "\n  finalize_release\n  exit 0\nfi"
        if analyzed.count(expected) != 1:
            raise GateError(
                f"{path}: release finalize success boundary changed shape"
            )
        analyzed = analyzed.replace(
            expected,
            "\n  finalize_release\n  exit 1\nfi",
            1,
        )
    tokens = _shell_tokens(path, analyzed, 1)
    _inspect_shell_tokens(
        path,
        relative,
        1,
        tokens,
        source,
        "",
        "",
    )


def validate_semantics(path: Path, relative: str, raw: bytes, language: str) -> None:
    """Reject explicit self-skip and runtime-substitution constructs."""

    if language == "shell":
        _shell_semantics(path, relative, raw)
        return
    validate_python_semantics(path, relative, raw)
