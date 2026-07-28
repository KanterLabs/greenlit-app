"""Rust lexical scanning and target-module discovery."""

from __future__ import annotations

import ast
import re
from pathlib import Path

from .model import (
    PATH_MOD_PATTERN,
    STANDARD_MOD_PATTERN,
    TEST_ATTRIBUTE_PATTERN,
    GateError,
)


def scrub_rust(text: str) -> str:
    """Replace comments and literals with spaces while preserving newlines."""

    result = list(text)
    length = len(text)
    index = 0
    block_depth = 0
    while index < length:
        if block_depth:
            if text.startswith("/*", index):
                result[index] = result[index + 1] = " "
                block_depth += 1
                index += 2
            elif text.startswith("*/", index):
                result[index] = result[index + 1] = " "
                block_depth -= 1
                index += 2
            else:
                if text[index] != "\n":
                    result[index] = " "
                index += 1
            continue

        if text.startswith("//", index):
            while index < length and text[index] != "\n":
                result[index] = " "
                index += 1
            continue
        if text.startswith("/*", index):
            result[index] = result[index + 1] = " "
            block_depth = 1
            index += 2
            continue

        raw = raw_string_start(text, index)
        if raw is not None:
            content_start, terminator = raw
            for position in range(index, content_start):
                result[position] = " "
            end = text.find(terminator, content_start)
            end = length if end < 0 else end + len(terminator)
            while content_start < end:
                if text[content_start] != "\n":
                    result[content_start] = " "
                content_start += 1
            index = end
            continue

        if text[index] == '"':
            index = scrub_quoted(text, result, index, '"')
            continue
        if text[index] == "'" and looks_like_char_literal(text, index):
            index = scrub_quoted(text, result, index, "'")
            continue
        index += 1
    return "".join(result)


def raw_string_start(text: str, index: int) -> tuple[int, str] | None:
    """Return the content offset and terminator for a Rust raw string."""

    if text[index] != "r":
        return None
    cursor = index + 1
    while cursor < len(text) and text[cursor] == "#":
        cursor += 1
    if cursor >= len(text) or text[cursor] != '"':
        return None
    hashes = text[index + 1 : cursor]
    return cursor + 1, '"' + hashes


def looks_like_char_literal(text: str, index: int) -> bool:
    """Distinguish a Rust character literal from a lifetime."""

    cursor = index + 1
    if cursor >= len(text) or text[cursor] == "\n":
        return False
    if text[cursor] == "\\":
        cursor += 2
    else:
        cursor += 1
    return cursor < len(text) and text[cursor] == "'"


def scrub_quoted(text: str, result: list[str], start: int, quote: str) -> int:
    """Blank one ordinary quoted Rust literal."""

    cursor = start
    escaped = False
    while cursor < len(text):
        character = text[cursor]
        if character != "\n":
            result[cursor] = " "
        cursor += 1
        if escaped:
            escaped = False
        elif character == "\\":
            escaped = True
        elif character == quote and cursor > start + 1:
            break
    return cursor


def scrub_comments(text: str) -> str:
    """Replace Rust comments while retaining literals used by `#[path]`."""

    result = list(text)
    index = 0
    block_depth = 0
    while index < len(text):
        if block_depth:
            if text.startswith("/*", index):
                result[index] = result[index + 1] = " "
                block_depth += 1
                index += 2
            elif text.startswith("*/", index):
                result[index] = result[index + 1] = " "
                block_depth -= 1
                index += 2
            else:
                if text[index] != "\n":
                    result[index] = " "
                index += 1
            continue
        if text.startswith("//", index):
            while index < len(text) and text[index] != "\n":
                result[index] = " "
                index += 1
            continue
        if text.startswith("/*", index):
            result[index] = result[index + 1] = " "
            block_depth = 1
            index += 2
            continue
        index += 1
    return "".join(result)


def rust_path_literal(literal: str, path: Path) -> str:
    """Decode the ordinary/raw Rust string forms accepted by `#[path]`."""

    if literal.startswith("r"):
        quote = literal.find('"')
        hashes = literal[1:quote]
        terminator = '"' + hashes
        if quote < 1 or not literal.endswith(terminator):
            raise GateError(f"{path}: malformed raw #[path] literal")
        return literal[quote + 1 : -len(terminator)]
    try:
        value = ast.literal_eval(literal)
    except (SyntaxError, ValueError) as error:
        raise GateError(f"{path}: malformed #[path] literal {literal!r}") from error
    if not isinstance(value, str) or not value:
        raise GateError(f"{path}: #[path] must name a nonempty string")
    return value


def module_sources(root: Path) -> set[Path]:
    """Follow one target's ordinary and explicit Rust module declarations."""

    discovered: set[Path] = set()
    pending: list[tuple[Path, Path]] = [(root, root.parent)]
    while pending:
        path, module_directory = pending.pop()
        if path in discovered:
            continue
        raw, scrubbed = read_source(path)
        discovered.add(path)
        comments_scrubbed = scrub_comments(raw)
        explicit_names: set[str] = set()
        explicit_count = len(
            re.findall(r"#\s*\[\s*path\b", comments_scrubbed, re.MULTILINE)
        )
        matches = list(PATH_MOD_PATTERN.finditer(comments_scrubbed))
        if explicit_count != len(matches):
            raise GateError(
                f"{path}: every #[path] module declaration must use a supported "
                "ordinary or raw string literal"
            )
        for match in matches:
            explicit_names.add(match.group("name"))
            relative = rust_path_literal(match.group("literal"), path)
            resolved = resolve_source(path.parent / relative)
            next_directory = (
                resolved.parent
                if resolved.name == "mod.rs"
                else resolved.parent / resolved.stem
            )
            pending.append((resolved, next_directory))

        for match in STANDARD_MOD_PATTERN.finditer(scrubbed):
            name = match.group(1)
            if name in explicit_names:
                continue
            flat = module_directory / f"{name}.rs"
            nested = module_directory / name / "mod.rs"
            choices = [candidate for candidate in (flat, nested) if candidate.exists()]
            if len(choices) > 1:
                raise GateError(
                    f"{path}: module {name!r} has both {flat} and {nested}"
                )
            if not choices:
                continue
            resolved = resolve_source(choices[0])
            next_directory = (
                resolved.parent
                if resolved.name == "mod.rs"
                else resolved.parent / resolved.stem
            )
            pending.append((resolved, next_directory))
    return discovered


def matching_brace(text: str, opening: int) -> int | None:
    """Return the closing brace for one scrubbed Rust block."""

    depth = 0
    for index in range(opening, len(text)):
        if text[index] == "{":
            depth += 1
        elif text[index] == "}":
            depth -= 1
            if depth == 0:
                return index
    return None


def test_bodies(scrubbed: str) -> list[tuple[str, int, str]]:
    """Return each attributed test name, offset, and scrubbed body."""

    bodies: list[tuple[str, int, str]] = []
    for attribute in TEST_ATTRIBUTE_PATTERN.finditer(scrubbed):
        signature = re.search(
            r"\b(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)[^;{]*\{",
            scrubbed[attribute.end() :],
            re.MULTILINE,
        )
        if signature is None:
            continue
        start = attribute.end() + signature.start()
        opening = attribute.end() + signature.end() - 1
        closing = matching_brace(scrubbed, opening)
        if closing is None:
            continue
        bodies.append((signature.group(1), start, scrubbed[opening + 1 : closing]))
    return bodies


def read_source(path: Path) -> tuple[str, str]:
    """Read one regular UTF-8 Rust source and its scrubbed form."""

    if path.is_symlink() or not path.is_file():
        raise GateError(f"{path}: Rust source must be a regular non-symlink file")
    try:
        raw = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise GateError(f"could not read UTF-8 Rust source {path}: {error}") from error
    return raw, scrub_rust(raw)


def resolve_source(path: Path) -> Path:
    """Resolve and validate a selected source without following a leaf link."""

    if path.is_symlink() or not path.is_file():
        raise GateError(f"{path}: Rust source must be a regular non-symlink file")
    try:
        return path.resolve(strict=True)
    except OSError as error:
        raise GateError(f"could not resolve Rust source {path}: {error}") from error
