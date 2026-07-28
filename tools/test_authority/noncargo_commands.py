"""Structured shell command edges for reviewed workflow and harness routes."""

from __future__ import annotations

import re
import shlex
from dataclasses import dataclass
from pathlib import Path

from .model import GateError


ASSIGNMENT = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")
LOCAL_PATH = re.compile(r"(?<![A-Za-z0-9_.-])(tools/[A-Za-z0-9_./-]+)")
CONTROL = {"&", "&&", "(", ")", ";", ";;", "|", "||"}
COMMAND_WRAPPERS = {"run_release_gate", "unshare"}
RESERVED = {
    "!",
    "do",
    "elif",
    "else",
    "if",
    "then",
    "time",
    "until",
    "while",
    "{",
    "}",
}
PYTHON = "/usr/bin/python3"


@dataclass(frozen=True, order=True)
class CommandEdge:
    """One parsed executable/argv edge that reaches a local reviewed command."""

    entrypoint: str
    executable: str
    argv: tuple[str, ...]


def _tokens(path: Path, line: str, line_number: int) -> list[str]:
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
            f"{path}:{line_number}: authority command is not statically "
            f"parseable: {error}"
        ) from error


def _segments(tokens: list[str]) -> list[list[str]]:
    result: list[list[str]] = []
    current: list[str] = []
    for token in tokens:
        if token in CONTROL:
            if current:
                result.append(current)
                current = []
            continue
        current.append(token)
    if current:
        result.append(current)
    return result


def _command(segment: list[str]) -> tuple[str, tuple[str, ...]] | None:
    cursor = 0
    while cursor < len(segment) and (
        segment[cursor] in RESERVED or ASSIGNMENT.match(segment[cursor])
    ):
        cursor += 1
    while cursor < len(segment) and segment[cursor] in {
        "builtin",
        "command",
        "env",
        "exec",
    }:
        cursor += 1
        while cursor < len(segment) and ASSIGNMENT.match(segment[cursor]):
            cursor += 1
    if cursor >= len(segment):
        return None
    executable = segment[cursor]
    arguments: list[str] = []
    for token in segment[cursor + 1 :]:
        if token in {"<", "<<", ">", ">>"}:
            break
        arguments.append(token)
    return executable, tuple(arguments)


def _edge(
    path: Path,
    line_number: int,
    executable: str,
    argv: tuple[str, ...],
) -> CommandEdge | None:
    if executable.startswith("tools/"):
        return CommandEdge(executable, executable, argv)
    if executable in COMMAND_WRAPPERS:
        local_arguments = tuple(
            argument for argument in argv if argument.startswith("tools/")
        )
        if len(local_arguments) > 1:
            raise GateError(
                f"{path}:{line_number}: authority command wrapper has more "
                "than one local executable argument"
            )
        if local_arguments:
            return CommandEdge(local_arguments[0], executable, argv)
    if executable not in {PYTHON, "python3"}:
        return None
    script_index: int | None = None
    for index, argument in enumerate(argv):
        if argument == "--":
            if index + 1 < len(argv):
                script_index = index + 1
            break
        if not argument.startswith("-"):
            script_index = index
            break
    if script_index is None or not argv[script_index].startswith("tools/"):
        return None
    if executable != PYTHON or "-I" not in argv[:script_index]:
        raise GateError(
            f"{path}:{line_number}: local Python command must launch through "
            "explicit /usr/bin/python3 -I before its script argument"
        )
    return CommandEdge(
        argv[script_index],
        executable,
        argv,
    )


def shell_command_edges(path: Path, text: str) -> tuple[CommandEdge, ...]:
    """Extract every statically executable local command/argv edge."""

    lines = text.splitlines()
    edges: list[CommandEdge] = []
    index = 0
    heredoc: tuple[str, bool] | None = None
    array_depth = 0
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
                    f"{path}:{line_number}: authority command continuation "
                    "has no following line"
                )
            logical += " " + lines[index].lstrip()
        if array_depth:
            array_depth += logical.count("(") - logical.count(")")
            if array_depth < 0:
                raise GateError(
                    f"{path}:{line_number}: authority shell array is unbalanced"
                )
            index += 1
            continue
        if re.match(
            r"^\s*(?:readonly\s+)?[A-Za-z_][A-Za-z0-9_]*=\(\s*$",
            logical,
        ):
            array_depth = logical.count("(") - logical.count(")")
            index += 1
            continue
        visible = logical.split("#", 1)[0]
        match = re.search(
            r"<<(?P<tabs>-?)[ \t]*['\"]?"
            r"(?P<name>[A-Za-z_][A-Za-z0-9_]*)['\"]?",
            visible,
        )
        if match is not None:
            heredoc = (match.group("name"), match.group("tabs") == "-")
        if LOCAL_PATH.search(visible) is not None:
            tokens = _tokens(path, logical, line_number)
            line_edges = [
                edge
                for segment in _segments(tokens)
                if (command := _command(segment)) is not None
                if (
                    edge := _edge(
                        path,
                        line_number,
                        command[0],
                        command[1],
                    )
                )
                is not None
            ]
            edges.extend(line_edges)
        index += 1
    if heredoc is not None:
        raise GateError(f"{path}: authority command has an unterminated heredoc")
    if array_depth:
        raise GateError(f"{path}: authority shell array is unterminated")
    return tuple(edges)


def verify_release_wrapper(path: Path, text: str) -> None:
    """Prove the reviewed release wrapper forwards argv and rechecks source."""

    lines = text.splitlines()
    starts = [
        index
        for index, line in enumerate(lines)
        if line == "run_release_gate() {"
    ]
    if len(starts) != 1:
        raise GateError(f"{path}: release authority wrapper is not uniquely defined")
    start = starts[0] + 1
    try:
        end = lines.index("}", start)
    except ValueError as error:
        raise GateError(f"{path}: release authority wrapper is unterminated") from error
    commands: list[tuple[str, tuple[str, ...]]] = []
    for offset, line in enumerate(lines[start:end], start=start + 1):
        tokens = _tokens(path, line, offset)
        for segment in _segments(tokens):
            command = _command(segment)
            if command is not None:
                commands.append(command)
    expected = [
        ("run_authority_python", ("$@",)),
        ("assert_checkout_unchanged", ()),
    ]
    if commands != expected:
        raise GateError(
            f"{path}: release authority wrapper must forward exact argv once "
            "and then recheck the bound checkout"
        )
