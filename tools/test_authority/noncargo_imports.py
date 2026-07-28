"""Static import and dependency resolution for reviewed non-Cargo sources."""

from __future__ import annotations

import ast
import os
import re
import shlex
import stat
from pathlib import Path, PurePosixPath

from .model import GateError
from .noncargo_fs import SourceTree, stable_directory_identity
from .noncargo_sources import MAX_FILE_BYTES, MAX_FILES, canonical_path


HEREDOC = re.compile(
    r"<<(?P<tabs>-?)[ \t]*(?P<quote>['\"]?)(?P<name>[A-Za-z_][A-Za-z0-9_]*)"
    r"(?P=quote)"
)
DYNAMIC_APIS = frozenset(
    {
        "exec_module", "find_loader", "find_module", "find_spec",
        "import_module", "module_from_spec", "spec_from_file_location",
    }
)
IMPORT_STATE = frozenset(
    {"meta_path", "modules", "path", "path_hooks", "path_importer_cache"}
)
MUTATING_METHODS = frozenset(
    {
        "__delitem__", "__iadd__", "__imul__", "__ior__", "__setitem__",
        "add", "append", "clear", "discard", "extend", "insert", "pop",
        "popitem", "remove", "reverse", "setdefault", "sort", "update",
    }
)


def _reject_extension_candidates(
    tree: SourceTree,
    root: Path,
    directory: str,
    stem: str,
) -> None:
    """Reject local CPython extension candidates without following directories."""

    metadata = tree.lstat(directory)
    if metadata is None:
        return
    _reject_candidate(root, directory, metadata, expected_directory=True)
    descriptor = tree.open_directory(directory)
    before = stable_directory_identity(descriptor)
    entries = 0
    try:
        with os.scandir(descriptor) as children:
            for child in children:
                entries += 1
                if entries > MAX_FILES:
                    raise GateError(
                        f"{root / directory}: local Python candidate inventory "
                        "exceeds traversal limits"
                    )
                if (
                    child.name.endswith(".so")
                    and (
                        child.name == f"{stem}.so"
                        or child.name.startswith(f"{stem}.")
                    )
                ):
                    raise GateError(
                        f"{root / directory / child.name}: compiled or "
                        "sourceless extension-module candidate is forbidden"
                    )
        if stable_directory_identity(descriptor) != before:
            raise GateError(
                f"{root / directory}: local Python candidate inventory "
                "changed during traversal"
            )
    finally:
        os.close(descriptor)


def _reject_candidate(
    root: Path,
    relative: str,
    metadata: os.stat_result | None,
    *,
    expected_directory: bool = False,
) -> bool:
    """Reject unsafe local import candidates and report whether one exists."""

    if metadata is None:
        return False
    if stat.S_ISLNK(metadata.st_mode):
        raise GateError(f"{root / relative}: local Python candidate is a symlink")
    expected = stat.S_ISDIR(metadata.st_mode) if expected_directory else stat.S_ISREG(
        metadata.st_mode
    )
    if not expected:
        raise GateError(
            f"{root / relative}: local Python candidate is a special or "
            "unsupported source node"
        )
    return True


def _module_sources(
    tree: SourceTree,
    root: Path,
    import_root: str,
    parts: tuple[str, ...],
) -> set[Path]:
    """Resolve one local source candidate set and reject sourceless alternatives."""

    sources: set[Path] = set()
    for index in range(1, len(parts) + 1):
        package_relative = "/".join((import_root, *parts[:index]))
        package_path = PurePosixPath(package_relative)
        _reject_extension_candidates(
            tree,
            root,
            package_path.parent.as_posix(),
            package_path.name,
        )
        package_metadata = tree.lstat(package_relative)
        if package_metadata is None:
            continue
        _reject_candidate(
            root,
            package_relative,
            package_metadata,
            expected_directory=True,
        )
        _reject_extension_candidates(tree, root, package_relative, "__init__")
        initializer = f"{package_relative}/__init__.py"
        initializer_metadata = tree.lstat(initializer)
        for suffix in ("pyc", "pyo"):
            bytecode = f"{package_relative}/__init__.{suffix}"
            if tree.lstat(bytecode) is not None:
                raise GateError(
                    f"{root / bytecode}: sourceless or compiled package "
                    "candidate is forbidden"
                )
        if not _reject_candidate(root, initializer, initializer_metadata):
            raise GateError(
                f"{root / package_relative}: namespace or sourceless local "
                "package candidates are forbidden"
            )
        sources.add(root / initializer)

    module_base = "/".join((import_root, *parts))
    module_path = PurePosixPath(module_base)
    _reject_extension_candidates(
        tree,
        root,
        module_path.parent.as_posix(),
        module_path.name,
    )
    module = f"{module_base}.py"
    module_metadata = tree.lstat(module)
    for suffix in ("pyc", "pyo"):
        bytecode = f"{module_base}.{suffix}"
        if tree.lstat(bytecode) is not None:
            raise GateError(
                f"{root / bytecode}: sourceless or compiled module candidate "
                "is forbidden"
            )
    if _reject_candidate(root, module, module_metadata):
        sources.add(root / module)
    return sources


def _source_package(relative: str, import_roots: tuple[str, ...]) -> tuple[str, ...]:
    source = PurePosixPath(relative)
    for root in import_roots:
        base = PurePosixPath(root)
        if source.is_relative_to(base):
            parts = source.relative_to(base).parts
            if len(parts) == 1:
                return ()
            return parts[:-1]
    raise GateError(f"{relative}: source is outside its declared Python import roots")


def _targets_import_state(target: ast.expr) -> bool:
    if isinstance(target, (ast.Tuple, ast.List)):
        return any(_targets_import_state(item) for item in target.elts)
    value = target.value if isinstance(target, ast.Subscript) else target
    return (
        isinstance(value, ast.Attribute)
        and isinstance(value.value, ast.Name)
        and value.value.id == "sys"
        and value.attr in IMPORT_STATE
    )


def _is_path_bootstrap(target: ast.expr, value: ast.expr) -> bool:
    return (
        isinstance(target, ast.Subscript)
        and isinstance(target.value, ast.Attribute)
        and isinstance(target.value.value, ast.Name)
        and target.value.value.id == "sys"
        and target.value.attr == "path"
        and isinstance(target.slice, ast.Slice)
        and target.slice.lower is None
        and isinstance(target.slice.upper, ast.Constant)
        and target.slice.upper.value == 0
        and target.slice.step is None
        and isinstance(value, ast.List)
        and bool(value.elts)
    )


def _mutates_import_state(function: ast.expr) -> bool:
    return (
        isinstance(function, ast.Attribute)
        and function.attr in MUTATING_METHODS
        and isinstance(function.value, ast.Attribute)
        and isinstance(function.value.value, ast.Name)
        and function.value.value.id == "sys"
        and function.value.attr in IMPORT_STATE
    )


def python_dependencies(
    root: Path,
    relative: str,
    raw: bytes,
    import_roots: tuple[str, ...],
    authority_imports: tuple[str, ...],
    *,
    allow_path_bootstrap: bool = False,
) -> tuple[set[Path], set[str]]:
    """Resolve repository-local imports from one Python source."""

    path = root / relative
    try:
        tree = ast.parse(raw.decode("utf-8"), filename=str(path))
    except (UnicodeError, SyntaxError) as error:
        raise GateError(f"{path}: invalid reviewed Python source: {error}") from error
    package = _source_package(relative, import_roots)
    dependencies: set[Path] = set()
    used_authorities: set[str] = set()
    dynamic_names = {"__import__", "eval", "exec"}
    for node in ast.walk(tree):
        if (
            isinstance(node, ast.ImportFrom)
            and (node.module or "").split(".", 1)[0] == "importlib"
        ):
            dynamic_names.update(
                alias.asname or alias.name
                for alias in node.names
                if alias.name in DYNAMIC_APIS
            )
    with SourceTree(root) as source_tree:
        for node in ast.walk(tree):
            if isinstance(node, ast.Call):
                function = node.func
                dynamic = (
                    isinstance(function, ast.Name)
                    and function.id in dynamic_names
                ) or (
                    isinstance(function, ast.Attribute)
                    and function.attr in DYNAMIC_APIS
                )
                if dynamic or _mutates_import_state(function):
                    raise GateError(
                        f"{path}:{node.lineno}: dynamic imports and import hooks "
                        "are forbidden in reviewed harnesses"
                    )
            if isinstance(node, (ast.Assign, ast.AnnAssign, ast.AugAssign, ast.Delete)):
                targets: list[ast.expr]
                if isinstance(node, ast.Assign):
                    targets = list(node.targets)
                elif isinstance(node, ast.Delete):
                    targets = list(node.targets)
                else:
                    targets = [node.target]
                for target in targets:
                    if (
                        allow_path_bootstrap
                        and isinstance(node, ast.Assign)
                        and _is_path_bootstrap(target, node.value)
                    ):
                        continue
                    if _targets_import_state(target):
                        raise GateError(
                            f"{path}:{node.lineno}: Python import-hook rebinding "
                            "is forbidden in reviewed harnesses"
                        )
            candidates: list[tuple[str, ...]] = []
            if isinstance(node, ast.Import):
                for alias in node.names:
                    matches = [
                        prefix
                        for prefix in authority_imports
                        if alias.name == prefix
                        or alias.name.startswith(prefix + ".")
                    ]
                    if matches:
                        used_authorities.update(matches)
                        continue
                    candidates.append(tuple(alias.name.split(".")))
            elif isinstance(node, ast.ImportFrom):
                name = node.module or ""
                if node.level == 0:
                    matches = [
                        prefix
                        for prefix in authority_imports
                        if name == prefix or name.startswith(prefix + ".")
                    ]
                    if matches:
                        used_authorities.update(matches)
                        continue
                if node.level:
                    retained = len(package) - node.level + 1
                    if retained < 0:
                        raise GateError(
                            f"{path}:{node.lineno}: relative import escapes its root"
                        )
                    module = package[:retained]
                else:
                    module = ()
                if node.module:
                    module += tuple(node.module.split("."))
                candidates.append(module)
                candidates.extend(
                    module + tuple(alias.name.split("."))
                    for alias in node.names
                    if alias.name != "*"
                )
            for parts in candidates:
                for import_root in import_roots:
                    dependencies.update(
                        _module_sources(
                            source_tree,
                            root,
                            import_root,
                            parts,
                        )
                    )
    return dependencies, used_authorities


def shell_dependencies(root: Path, relative: str, raw: bytes) -> set[Path]:
    """Resolve literal local `source` commands outside embedded heredocs."""

    path = root / relative
    try:
        lines = raw.decode("utf-8").splitlines()
    except UnicodeError as error:
        raise GateError(
            f"{path}: reviewed shell source must be UTF-8: {error}"
        ) from error
    dependencies: set[Path] = set()
    heredoc: tuple[str, bool] | None = None
    with SourceTree(root) as tree:
        for line_number, line in enumerate(lines, start=1):
            if heredoc is not None:
                delimiter, strip_tabs = heredoc
                candidate = line.lstrip("\t") if strip_tabs else line
                if candidate == delimiter:
                    heredoc = None
                continue
            match = HEREDOC.search(line)
            if match is not None:
                heredoc = (match.group("name"), match.group("tabs") == "-")
            stripped = line.lstrip()
            if not stripped.startswith(("source ", ". ")):
                continue
            try:
                words = shlex.split(stripped, comments=True, posix=True)
            except ValueError:
                words = []
            if (
                len(words) != 2
                or words[0] not in {"source", "."}
                or words[1] in {"=", "\\"}
                or any(character in words[1] for character in "$*?[`")
            ):
                if stripped.startswith(("source =", ". \\")):
                    continue
                raise GateError(
                    f"{path}:{line_number}: shell source must name one "
                    "literal canonical file"
                )
            operand = words[1]
            candidate = (
                PurePosixPath(operand)
                if operand.startswith("tools/")
                else PurePosixPath(relative).parent / operand
            )
            dependency = canonical_path(
                candidate.as_posix(),
                f"{path}:{line_number}",
            )
            tree.read_regular(dependency, MAX_FILE_BYTES)
            dependencies.add(root / dependency)
    return dependencies
