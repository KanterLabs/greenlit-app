"""Static process/delegate graph extraction for reviewed Python sources."""

from __future__ import annotations

import ast
from collections import Counter
from pathlib import Path, PurePosixPath

from .model import GateError


PROCESS_NAMES = {"_bound_call", "run_command"}
PROCESS_MEMBERS = {
    ("os", "execv"),
    ("os", "execve"),
    ("subprocess", "Popen"),
    ("subprocess", "call"),
    ("subprocess", "check_call"),
    ("subprocess", "check_output"),
    ("subprocess", "run"),
}


def _literal_parts(node: ast.AST) -> tuple[str, ...]:
    parts: list[str] = []
    current = node
    while isinstance(current, ast.BinOp) and isinstance(current.op, ast.Div):
        right = current.right
        if not isinstance(right, ast.Constant) or not isinstance(right.value, str):
            return ()
        parts[:0] = PurePosixPath(right.value).parts
        current = current.left
    if not parts:
        return ()
    if isinstance(current, ast.Constant) and isinstance(current.value, str):
        parts[:0] = PurePosixPath(current.value).parts
    elif (
        isinstance(current, ast.Call)
        and isinstance(current.func, ast.Name)
        and current.func.id == "Path"
        and len(current.args) == 1
        and isinstance(current.args[0], ast.Constant)
        and isinstance(current.args[0].value, str)
    ):
        parts[:0] = PurePosixPath(current.args[0].value).parts
    return tuple(part for part in parts if part not in {"", "."})


def _candidate_for(
    path: Path,
    parts: tuple[str, ...],
    candidates: set[str],
) -> str | None:
    if len(parts) < 2:
        return None
    matches = [
        candidate
        for candidate in candidates
        if PurePosixPath(candidate).parts[-len(parts) :] == parts
    ]
    if len(matches) > 1:
        raise GateError(
            f"{path}: static local path suffix is ambiguous: "
            f"{'/'.join(parts)!r}"
        )
    return matches[0] if matches else None


def _module_values(tree: ast.Module) -> dict[str, str | tuple[str, ...]]:
    values: dict[str, str | tuple[str, ...]] = {}
    for statement in tree.body:
        if not isinstance(statement, (ast.Assign, ast.AnnAssign)):
            continue
        targets = (
            statement.targets
            if isinstance(statement, ast.Assign)
            else [statement.target]
        )
        if len(targets) != 1 or not isinstance(targets[0], ast.Name):
            continue
        value = statement.value
        if isinstance(value, ast.Constant) and isinstance(value.value, str):
            values[targets[0].id] = value.value
        elif isinstance(value, (ast.Tuple, ast.List)) and all(
            isinstance(item, ast.Constant) and isinstance(item.value, str)
            for item in value.elts
        ):
            values[targets[0].id] = tuple(
                item.value
                for item in value.elts
                if isinstance(item, ast.Constant)
            )
    return values


def _path_bindings(
    path: Path,
    tree: ast.Module,
    candidates: set[str],
) -> dict[str, str]:
    bindings: dict[str, str] = {}
    for node in ast.walk(tree):
        if not isinstance(node, (ast.Assign, ast.AnnAssign)):
            continue
        targets = node.targets if isinstance(node, ast.Assign) else [node.target]
        if len(targets) != 1 or not isinstance(targets[0], ast.Name):
            continue
        candidate = _candidate_for(path, _literal_parts(node.value), candidates)
        if candidate is None:
            continue
        name = targets[0].id
        previous = bindings.setdefault(name, candidate)
        if previous != candidate:
            raise GateError(
                f"{path}: Python path binding {name!r} is not single-valued"
            )
    return bindings


def _scalar(
    path: Path,
    node: ast.AST,
    values: dict[str, str | tuple[str, ...]],
    bindings: dict[str, str],
    candidates: set[str],
) -> str:
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return node.value
    if isinstance(node, ast.Name):
        value = values.get(node.id)
        if isinstance(value, str):
            return value
        return bindings.get(node.id, "<dynamic>")
    if (
        isinstance(node, ast.Attribute)
        and isinstance(node.value, ast.Name)
        and node.value.id == "sys"
        and node.attr == "executable"
    ):
        return "/usr/bin/python3"
    if (
        isinstance(node, ast.Call)
        and isinstance(node.func, ast.Name)
        and node.func.id == "str"
        and len(node.args) == 1
        and not node.keywords
    ):
        return _scalar(path, node.args[0], values, bindings, candidates)
    if isinstance(node, ast.JoinedStr):
        pieces: list[str] = []
        for value in node.values:
            if isinstance(value, ast.Constant) and isinstance(value.value, str):
                pieces.append(value.value)
            elif isinstance(value, ast.FormattedValue):
                pieces.append(
                    _scalar(path, value.value, values, bindings, candidates)
                )
            else:
                pieces.append("<dynamic>")
        return "".join(pieces)
    candidate = _candidate_for(path, _literal_parts(node), candidates)
    return candidate if candidate is not None else "<dynamic>"


def _list_values(
    path: Path,
    node: ast.List,
    values: dict[str, str | tuple[str, ...]],
    bindings: dict[str, str],
    candidates: set[str],
) -> tuple[str, ...]:
    result: list[str] = []
    for item in node.elts:
        if isinstance(item, ast.Starred) and isinstance(item.value, ast.Name):
            expanded = values.get(item.value.id)
            if isinstance(expanded, tuple):
                result.extend(expanded)
                continue
        result.append(_scalar(path, item, values, bindings, candidates))
    return tuple(result)


def _process_call(node: ast.Call) -> bool:
    function = node.func
    if isinstance(function, ast.Name):
        return function.id in PROCESS_NAMES
    return (
        isinstance(function, ast.Attribute)
        and isinstance(function.value, ast.Name)
        and (function.value.id, function.attr) in PROCESS_MEMBERS
    )


def _selected_lists(tree: ast.Module) -> set[ast.List]:
    calls = [node for node in ast.walk(tree) if isinstance(node, ast.Call)]
    processes = [node for node in calls if _process_call(node)]
    direct: set[ast.List] = set()
    used_names: set[str] = set()
    factories: set[str] = set()
    for call in processes:
        arguments = (*call.args, *(keyword.value for keyword in call.keywords))
        for argument in arguments:
            direct.update(
                node for node in ast.walk(argument) if isinstance(node, ast.List)
            )
            used_names.update(
                node.id
                for node in ast.walk(argument)
                if isinstance(node, ast.Name) and isinstance(node.ctx, ast.Load)
            )
            factories.update(
                node.func.id
                for node in ast.walk(argument)
                if isinstance(node, ast.Call) and isinstance(node.func, ast.Name)
            )
    for node in ast.walk(tree):
        if (
            isinstance(node, ast.Assign)
            and len(node.targets) == 1
            and isinstance(node.targets[0], ast.Name)
            and node.targets[0].id in used_names
            and isinstance(node.value, ast.List)
        ):
            direct.add(node.value)
        elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            if node.name in factories:
                direct.update(
                    child.value
                    for child in ast.walk(node)
                    if isinstance(child, ast.Return)
                    and isinstance(child.value, ast.List)
                )
    return direct


def _verify_bound_call(path: Path, tree: ast.Module) -> None:
    calls = [
        node
        for node in ast.walk(tree)
        if isinstance(node, ast.Call)
        and isinstance(node.func, ast.Name)
        and node.func.id == "_bound_call"
    ]
    if not calls:
        return
    definitions = [
        node
        for node in tree.body
        if isinstance(node, ast.FunctionDef) and node.name == "_bound_call"
    ]
    if len(definitions) != 1:
        raise GateError(f"{path}: Python delegate wrapper is not uniquely defined")
    definition = definitions[0]
    if "command" not in {argument.arg for argument in definition.args.args}:
        raise GateError(f"{path}: Python delegate wrapper lacks command argv")
    forwarding = [
        node
        for node in ast.walk(definition)
        if isinstance(node, ast.Call)
        and isinstance(node.func, ast.Name)
        and node.func.id == "run_command"
        and node.args
        and isinstance(node.args[0], ast.Name)
        and node.args[0].id == "command"
    ]
    assignments = [
        node
        for node in ast.walk(definition)
        if isinstance(node, (ast.Assign, ast.AnnAssign, ast.AugAssign))
        and any(
            isinstance(target, ast.Name) and target.id == "command"
            for target in (
                node.targets if isinstance(node, ast.Assign) else [node.target]
            )
        )
    ]
    if len(forwarding) != 1 or assignments or any(
        isinstance(node, ast.Return) for node in ast.walk(definition)
    ):
        raise GateError(
            f"{path}: Python delegate wrapper must forward exact command argv once"
        )


def _python_script(values: tuple[str, ...], candidates: set[str]) -> str | None:
    if not values or values[0] != "/usr/bin/python3":
        return None
    index = 1
    options: list[str] = []
    while index < len(values) and values[index].startswith("-"):
        option = values[index]
        options.append(option)
        index += 1
        if option in {"-c", "-m"}:
            return None
    if index >= len(values) or values[index] not in candidates:
        return None
    if "-I" not in options or "-B" not in options:
        return f"!unisolated:{values[index]}"
    return values[index]


def _bash_script(values: tuple[str, ...], candidates: set[str]) -> str | None:
    if not values or values[0] != "/usr/bin/bash":
        return None
    try:
        command_index = values.index("-c")
    except ValueError:
        return None
    if command_index + 2 >= len(values):
        return None
    command = values[command_index + 1]
    matches = [
        value
        for index, value in enumerate(values[command_index + 2 :])
        if value in candidates
        and index > 0
        and (f'"${index}"' in command or f"${index}" in command)
    ]
    if len(matches) > 1:
        raise GateError("isolated Bash boundary binds multiple local scripts")
    return matches[0] if matches else None


def python_command_references(
    path: Path,
    raw: bytes,
    candidates: set[str],
) -> Counter[str]:
    """Extract local scripts only from statically reached process call lists."""

    try:
        tree = ast.parse(raw, filename=str(path))
    except (SyntaxError, UnicodeError) as error:
        raise GateError(
            f"{path}: Python source is not statically parseable: {error}"
        ) from error
    _verify_bound_call(path, tree)
    values = _module_values(tree)
    bindings = _path_bindings(path, tree, candidates)
    commands: Counter[str] = Counter()
    for node in _selected_lists(tree):
        items = _list_values(path, node, values, bindings, candidates)
        target = _python_script(items, candidates)
        if target is not None and target.startswith("!unisolated:"):
            raise GateError(
                f"{path}: local Python delegate is not explicitly launched "
                f"through /usr/bin/python3 -I -B: "
                f"{target.removeprefix('!unisolated:')}"
            )
        if target is None:
            target = _bash_script(items, candidates)
        if target is not None:
            commands[target] += 1
    return commands
