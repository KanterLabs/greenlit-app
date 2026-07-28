"""Public-boundary negative canaries for non-Cargo test authority."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from .model import GateError
from .noncargo_schema import POLICY_RELATIVE
from .selftest_noncargo_fs import (
    filesystem_race_canaries,
    fresh_case,
    hostile_environment,
    load_policy,
    refresh_reviewed_digest,
    require_rejection,
)


PYTHON_HARNESS = "tools/tests/check-capability-test-manifest"
SHELL_HARNESS = "tools/test-credential-capability"


def _append_reviewed_source(root: Path, relative: str, payload: str) -> None:
    path = root / relative
    original = path.read_text(encoding="utf-8")
    path.write_text(f"{original.rstrip()}\n{payload}", encoding="utf-8")
    refresh_reviewed_digest(root, relative)


def _source_canaries(
    temporary: Path,
    clean: dict[str, str],
    environment: dict[str, str],
) -> int:
    immediate = "immediate success"
    import_hook = "dynamic imports and import hooks are forbidden"
    cases = (
        (
            "nested-return",
            PYTHON_HARNESS,
            "def _canary(flag: bool) -> int:\n"
            "    if flag:\n"
            "        return 0\n"
            "    return 1\n",
            "nested or early success return",
        ),
        ("python-noarg-exit", PYTHON_HARNESS, "sys.exit()\n", immediate),
        ("python-zero-exit", PYTHON_HARNESS, "sys.exit(0)\n", immediate),
        ("system-exit", PYTHON_HARNESS, "raise SystemExit\n", immediate),
        ("os-zero-exit", PYTHON_HARNESS, "os._exit(0)\n", immediate),
        (
            "module-rebind",
            PYTHON_HARNESS,
            "sys = object()\n",
            "assignment replaces an imported module",
        ),
        (
            "runtime-rebind",
            "tools/live_parity/process.py",
            "subprocess.run = lambda *_args, **_kwargs: None\n",
            "assignment replaces a real runtime boundary",
        ),
        (
            "import-state-rebind", PYTHON_HARNESS, "sys.path_hooks = []\n",
            "Python import state must not be rebound"
        ),
        ("dynamic-import", PYTHON_HARNESS, '__import__("os")\n', import_hook),
        (
            "importlib-hook", PYTHON_HARNESS,
            'import importlib\nimportlib.import_module("os")\n', import_hook
        ),
        (
            "importlib-imported-name", PYTHON_HARNESS,
            "from importlib import import_module\nimport_module('os')\n", import_hook
        ),
        (
            "meta-path-mutator", PYTHON_HARNESS, "sys.meta_path.clear()\n",
            import_hook
        ),
        (
            "path-hooks-mutator", PYTHON_HARNESS,
            "sys.path_hooks.append(object())\n", import_hook
        ),
        ("shell-noarg-exit", SHELL_HARNESS, "exit\n", "no-argument shell exit"),
        ("shell-zero-exit", SHELL_HARNESS, "exit 00\n", "zero shell exit"),
        ("shell-computed-exit", SHELL_HARNESS, "exit $((0))\n",
         "computed-zero shell exit"),
        (
            "commented-heredoc", SHELL_HARNESS,
            "# <<'CANARY'\nexit 0\nCANARY\n", "zero shell exit"
        ),
        (
            "heredoc-body", SHELL_HARNESS,
            "cat <<'CANARY'\nexit 1\nCANARY\nexit 0\n", "zero shell exit"
        ),
    )
    for label, relative, payload, expected in cases:
        root = fresh_case(temporary, clean, label)
        _append_reviewed_source(root, relative, payload)
        require_rejection(root, environment, label, expected, relative)
    return len(cases)


def _write_policy(root: Path, value: dict[str, Any]) -> None:
    (root / POLICY_RELATIVE).write_text(
        json.dumps(value, indent=2, ensure_ascii=True) + "\n",
        encoding="utf-8",
    )


def _null_delegate(policy: dict[str, Any], variant: str) -> None:
    for entry in policy["entries"]:
        for delegate in entry["delegates"]:
            if variant in delegate:
                delegate[variant] = None
                return
    raise GateError(f"reviewed policy has no {variant} delegate for its null canary")


def _schema_canaries(
    temporary: Path,
    clean: dict[str, str],
    environment: dict[str, str],
) -> int:
    cases = (
        ("boolean-version", "schema_version must be 1", "version", True),
        ("nul-path", "source path must be text", "path", "tools/nul\0source"),
        (
            "surrogate-path", "source path must be text", "path",
            "tools/surrogate\ud800source"
        ),
        ("null-target", "target/authority variant must not be null",
         "delegate", "target"),
        ("null-authority", "target/authority variant must not be null",
         "delegate", "authority"),
    )
    for label, expected, kind, value in cases:
        root = fresh_case(temporary, clean, label)
        policy = load_policy(root)
        if kind == "version":
            policy["schema_version"] = value
        elif kind == "path":
            policy["entries"][0]["sources"][0] = value
        else:
            _null_delegate(policy, value)
        _write_policy(root, policy)
        require_rejection(root, environment, label, expected)
    return len(cases)


def non_cargo_semantic_canaries(temporary: Path, clean: dict[str, str]) -> int:
    """Prove non-Cargo bypasses fail through the installed public checker."""

    environment = hostile_environment(temporary)
    return (
        _source_canaries(temporary, clean, environment)
        + _schema_canaries(temporary, clean, environment)
        + filesystem_race_canaries(temporary, clean, environment)
    )
