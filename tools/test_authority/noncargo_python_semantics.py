"""Python-specific semantic bans for reviewed non-Cargo harness sources."""

from __future__ import annotations

import ast
from pathlib import Path

from .model import GateError


DANGEROUS_ATTRIBUTES = {
    "builtins": {"__import__", "open"},
    "os": {
        "_exit",
        "execv",
        "execve",
        "fork",
        "posix_spawn",
        "posix_spawnp",
        "system",
    },
    "pathlib": {"Path"},
    "shutil": {"copy", "copy2", "copyfile", "move", "which"},
    "socket": {"create_connection", "socket"},
    "subprocess": {
        "Popen",
        "call",
        "check_call",
        "check_output",
        "run",
    },
    "sys": {"exit"},
    "time": {"monotonic", "sleep", "time"},
}
IMPORT_STATE = {
    "meta_path",
    "modules",
    "path",
    "path_hooks",
    "path_importer_cache",
}
def _decode(path: Path, raw: bytes) -> str:
    try:
        return raw.decode("utf-8")
    except UnicodeError as error:
        raise GateError(
            f"{path}: reviewed harness source must be UTF-8: {error}"
        ) from error


def _module_aliases(
    tree: ast.Module,
) -> tuple[dict[str, str], dict[str, tuple[str, str]]]:
    modules: dict[str, str] = {}
    members: dict[str, tuple[str, str]] = {}
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            for alias in node.names:
                root = alias.name.split(".", 1)[0]
                modules[alias.asname or root] = root
        elif isinstance(node, ast.ImportFrom) and node.module is not None:
            root = node.module.split(".", 1)[0]
            for alias in node.names:
                members[alias.asname or alias.name] = (root, alias.name)
    return modules, members


def _attribute_identity(
    node: ast.expr,
    modules: dict[str, str],
) -> tuple[str, str] | None:
    if not isinstance(node, ast.Attribute):
        return None
    owner = node.value
    while isinstance(owner, ast.Attribute):
        owner = owner.value
    if not isinstance(owner, ast.Name) or owner.id not in modules:
        return None
    return modules[owner.id], node.attr


def _success_constant(node: ast.expr | None) -> bool:
    return node is None or (
        isinstance(node, ast.Constant)
        and (
            node.value is None
            or (
                isinstance(node.value, (int, float))
                and not isinstance(node.value, bool)
                and node.value == 0
            )
        )
    )


def _success_return(node: ast.expr | None) -> bool:
    return (
        isinstance(node, ast.Constant)
        and isinstance(node.value, (int, float))
        and not isinstance(node.value, bool)
        and node.value == 0
    )


def _terminal_success_returns(tree: ast.Module) -> set[int]:
    allowed: set[int] = set()
    for node in ast.walk(tree):
        if not isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            continue
        if node.body and isinstance(node.body[-1], ast.Return):
            terminal = node.body[-1]
            if _success_return(terminal.value):
                allowed.add(id(terminal))
    return allowed


class _PythonSemanticVisitor(ast.NodeVisitor):
    """Reject success shortcuts and replacement of imported/runtime authority."""

    def __init__(
        self,
        path: Path,
        modules: dict[str, str],
        members: dict[str, tuple[str, str]],
        allowed_returns: set[int],
    ) -> None:
        self.path = path
        self.modules = modules
        self.members = members
        self.allowed_returns = allowed_returns

    def _identity(self, node: ast.expr) -> tuple[str, str] | None:
        if isinstance(node, ast.Name):
            return self.members.get(node.id)
        return _attribute_identity(node, self.modules)

    def _reject_target(self, target: ast.expr, line: int) -> None:
        if isinstance(target, (ast.Tuple, ast.List)):
            for item in target.elts:
                self._reject_target(item, line)
            return
        if isinstance(target, ast.Name) and (
            target.id in self.modules or target.id in self.members
        ):
            raise GateError(
                f"{self.path}:{line}: non-Cargo runtime substitute: "
                "assignment replaces an imported module or runtime binding"
            )
        identity = self._identity(target)
        if (
            identity is not None
            and identity[1] in DANGEROUS_ATTRIBUTES.get(identity[0], set())
        ):
            raise GateError(
                f"{self.path}:{line}: non-Cargo runtime substitute: "
                "assignment replaces a real runtime boundary"
            )
        if (
            isinstance(target, ast.Attribute)
            and isinstance(target.value, ast.Name)
            and self.modules.get(target.value.id) == "sys"
            and target.attr in IMPORT_STATE
        ):
            raise GateError(
                f"{self.path}:{line}: Python import state must not be rebound"
            )

    def visit_Return(self, node: ast.Return) -> None:
        if (
            _success_return(node.value)
            and id(node) not in self.allowed_returns
        ):
            raise GateError(
                f"{self.path}:{node.lineno}: non-Cargo harness immediate success: "
                "nested or early success return can self-skip a capability"
            )
        self.generic_visit(node)

    def _success_exit(self, call: ast.Call) -> bool:
        identity = self._identity(call.func)
        named = isinstance(call.func, ast.Name) and call.func.id in {
            "SystemExit",
            "exit",
            "quit",
        }
        exiting = named or identity in {("os", "_exit"), ("sys", "exit")}
        return exiting and (
            not call.args or (len(call.args) == 1 and _success_constant(call.args[0]))
        )

    def visit_Call(self, node: ast.Call) -> None:
        if self._success_exit(node):
            raise GateError(
                f"{self.path}:{node.lineno}: non-Cargo harness immediate success: "
                "zero or no-argument process exit can self-skip a capability"
            )
        if isinstance(node.func, ast.Name) and node.func.id in {"setattr", "delattr"}:
            if node.args:
                target = node.args[0]
                if isinstance(target, ast.Name) and target.id in self.modules:
                    raise GateError(
                        f"{self.path}:{node.lineno}: non-Cargo runtime substitute: "
                        "dynamic attribute replacement targets an imported module"
                    )
        if (
            isinstance(node.func, ast.Attribute)
            and node.func.attr in {"patch", "patch.object"}
        ) or (isinstance(node.func, ast.Name) and node.func.id == "patch"):
            raise GateError(
                f"{self.path}:{node.lineno}: non-Cargo runtime substitute: "
                "runtime patching is forbidden"
            )
        self.generic_visit(node)

    def visit_Raise(self, node: ast.Raise) -> None:
        if isinstance(node.exc, ast.Name) and node.exc.id == "SystemExit":
            raise GateError(
                f"{self.path}:{node.lineno}: non-Cargo harness immediate success: "
                "no-argument SystemExit can self-skip a capability"
            )
        self.generic_visit(node)

    def visit_Assign(self, node: ast.Assign) -> None:
        for target in node.targets:
            self._reject_target(target, node.lineno)
        self.generic_visit(node)

    def visit_AnnAssign(self, node: ast.AnnAssign) -> None:
        self._reject_target(node.target, node.lineno)
        self.generic_visit(node)

    def visit_AugAssign(self, node: ast.AugAssign) -> None:
        self._reject_target(node.target, node.lineno)
        self.generic_visit(node)

    def visit_NamedExpr(self, node: ast.NamedExpr) -> None:
        self._reject_target(node.target, node.lineno)
        self.generic_visit(node)

    def visit_Delete(self, node: ast.Delete) -> None:
        for target in node.targets:
            self._reject_target(target, node.lineno)
        self.generic_visit(node)


def validate_python_semantics(path: Path, _relative: str, raw: bytes) -> None:
    """Reject Python self-skip and runtime-substitution constructs."""

    try:
        tree = ast.parse(_decode(path, raw), filename=str(path))
    except SyntaxError as error:
        raise GateError(f"{path}: invalid reviewed Python source: {error}") from error
    modules, members = _module_aliases(tree)
    visitor = _PythonSemanticVisitor(
        path,
        modules,
        members,
        _terminal_success_returns(tree),
    )
    visitor.visit(tree)
