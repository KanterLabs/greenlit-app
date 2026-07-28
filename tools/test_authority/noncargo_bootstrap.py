"""AST authority for isolated reviewed Python command entry points."""

from __future__ import annotations

import ast
from pathlib import Path, PurePosixPath

from .model import GateError


INTERPRETER = "/usr/bin/python3"
PYCACHE_SINK = "/proc/self/fd/greenlit-impossible-pycache"


def _attribute(node: ast.AST, *parts: str) -> bool:
    current = node
    for part in reversed(parts[1:]):
        if not isinstance(current, ast.Attribute) or current.attr != part:
            return False
        current = current.value
    return isinstance(current, ast.Name) and current.id == parts[0]


def _isolated_test(node: ast.AST) -> bool:
    return (
        isinstance(node, ast.UnaryOp)
        and isinstance(node.op, ast.Not)
        and _attribute(node.operand, "sys", "flags", "isolated")
    )


def _reexec_call(statement: ast.stmt) -> ast.Call | None:
    if not isinstance(statement, ast.Expr) or not isinstance(
        statement.value,
        ast.Call,
    ):
        return None
    call = statement.value
    return call if _attribute(call.func, "os", "execve") else None


def _slice_one(node: ast.AST) -> bool:
    return (
        isinstance(node, ast.Subscript)
        and _attribute(node.value, "sys", "argv")
        and isinstance(node.slice, ast.Slice)
        and isinstance(node.slice.lower, ast.Constant)
        and node.slice.lower.value == 1
        and node.slice.upper is None
        and node.slice.step is None
    )


def _verify_reexec(path: Path, branch: ast.If) -> None:
    if len(branch.body) != 1 or (call := _reexec_call(branch.body[0])) is None:
        raise GateError(
            f"{path}: isolated bootstrap must only re-exec the fixed interpreter"
        )
    if len(call.args) != 3 or call.keywords:
        raise GateError(f"{path}: isolated bootstrap execve shape differs")
    executable, arguments, environment = call.args
    if (
        not isinstance(executable, ast.Constant)
        or executable.value != INTERPRETER
        or not _attribute(environment, "os", "environ")
        or not isinstance(arguments, ast.List)
        or len(arguments.elts) != 5
    ):
        raise GateError(f"{path}: isolated bootstrap execve binding differs")
    expected = (INTERPRETER, "-I", "-B")
    for node, value in zip(arguments.elts[:3], expected, strict=True):
        if not isinstance(node, ast.Constant) or node.value != value:
            raise GateError(f"{path}: isolated bootstrap interpreter argv differs")
    if not isinstance(arguments.elts[3], ast.Name) or arguments.elts[3].id != "__file__":
        raise GateError(f"{path}: isolated bootstrap must execute __file__")
    tail = arguments.elts[4]
    if not isinstance(tail, ast.Starred) or not _slice_one(tail.value):
        raise GateError(f"{path}: isolated bootstrap must preserve argv[1:]")


def _assignment(
    statement: ast.stmt,
    owner: str,
    member: str,
) -> ast.expr | None:
    if (
        not isinstance(statement, ast.Assign)
        or len(statement.targets) != 1
        or not _attribute(statement.targets[0], owner, member)
    ):
        return None
    return statement.value


def _path_assignment(statement: ast.stmt) -> bool:
    if not isinstance(statement, ast.Assign) or len(statement.targets) != 1:
        return False
    target = statement.targets[0]
    return (
        isinstance(target, ast.Subscript)
        and _attribute(target.value, "sys", "path")
        and isinstance(target.slice, ast.Slice)
        and target.slice.lower is None
        and isinstance(target.slice.upper, ast.Constant)
        and target.slice.upper.value == 0
        and target.slice.step is None
        and isinstance(statement.value, ast.List)
        and bool(statement.value.elts)
    )


def _symlink_guard(statement: ast.stmt) -> bool:
    return (
        isinstance(statement, ast.If)
        and isinstance(statement.test, ast.Call)
        and isinstance(statement.test.func, ast.Attribute)
        and statement.test.func.attr == "is_symlink"
        and not statement.test.args
        and not statement.test.keywords
    )


def _local_modules(
    sources: tuple[str, ...],
    import_roots: tuple[str, ...],
) -> set[str]:
    result: set[str] = set()
    for source_text in sources:
        source = PurePosixPath(source_text)
        for root_text in import_roots:
            root = PurePosixPath(root_text)
            if source.is_relative_to(root):
                parts = source.relative_to(root).parts
                if parts:
                    result.add(parts[0].removesuffix(".py"))
    return result


def _local_import_lines(tree: ast.Module, modules: set[str]) -> list[int]:
    result: list[int] = []
    for statement in tree.body:
        if isinstance(statement, ast.Import):
            if any(alias.name.split(".", 1)[0] in modules for alias in statement.names):
                result.append(statement.lineno)
        elif (
            isinstance(statement, ast.ImportFrom)
            and statement.module is not None
            and statement.module.split(".", 1)[0] in modules
        ):
            result.append(statement.lineno)
    return result


def validate_isolated_entrypoint(
    path: Path,
    raw: bytes,
    sources: tuple[str, ...],
    import_roots: tuple[str, ...],
) -> None:
    """Prove isolation, no-bytecode state, symlink guard, and import ordering."""

    if not raw.startswith(b"#!/usr/bin/python3 -I\n"):
        raise GateError(
            f"{path}: reviewed Python entry point must use the explicit "
            "isolated /usr/bin/python3 shebang"
        )
    try:
        tree = ast.parse(raw, filename=str(path))
    except (SyntaxError, UnicodeError) as error:
        raise GateError(f"{path}: isolated Python bootstrap is invalid: {error}") from error
    isolated = [
        statement
        for statement in tree.body
        if isinstance(statement, ast.If) and _isolated_test(statement.test)
    ]
    if len(isolated) != 1:
        raise GateError(f"{path}: isolated bootstrap must have one re-exec branch")
    _verify_reexec(path, isolated[0])
    dont_write = [
        statement
        for statement in tree.body
        if (
            (value := _assignment(statement, "sys", "dont_write_bytecode"))
            is not None
            and isinstance(value, ast.Constant)
            and value.value is True
        )
    ]
    pycache = [
        statement
        for statement in tree.body
        if (
            (value := _assignment(statement, "sys", "pycache_prefix"))
            is not None
            and isinstance(value, ast.Constant)
            and value.value == PYCACHE_SINK
        )
    ]
    path_ready = [statement for statement in tree.body if _path_assignment(statement)]
    symlink = [statement for statement in tree.body if _symlink_guard(statement)]
    if not all(len(items) == 1 for items in (dont_write, pycache, path_ready, symlink)):
        raise GateError(
            f"{path}: isolated bootstrap state, symlink guard, or import root differs"
        )
    ordered = [
        isolated[0].lineno,
        dont_write[0].lineno,
        pycache[0].lineno,
        symlink[0].lineno,
        path_ready[0].lineno,
    ]
    if ordered != sorted(ordered):
        raise GateError(f"{path}: isolated Python bootstrap operations are out of order")
    local_imports = _local_import_lines(
        tree,
        _local_modules(sources, import_roots),
    )
    if any(line < path_ready[0].lineno for line in local_imports):
        raise GateError(
            f"{path}: local import occurs before explicit source-only import roots"
        )
