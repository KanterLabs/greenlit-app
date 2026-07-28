"""Public-command runner used by parity comparator self-tests."""

from __future__ import annotations

import copy
import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Any, Callable

from .selftest_data import REPOSITORY_ID, empty_ledger
from .selftest_repo import seal_captures


Document = dict[str, Any]
Documents = list[Document]
Mutation = Callable[[Document], None]
SealedMutation = Callable[[Documents, Path], None]


def set_value(
    document: Document,
    path: tuple[str | int, ...],
    value: Any,
) -> None:
    """Assign one nested self-test value."""

    target: Any = document
    for component in path[:-1]:
        target = target[component]
    target[path[-1]] = value


def changed(
    documents: Documents,
    path: tuple[str | int, ...],
    value: Any,
    index: int = 2,
) -> Documents:
    """Return a triple with one nested value changed."""

    result = copy.deepcopy(documents)
    set_value(result[index], path, value)
    return result


def altered(
    documents: Documents,
    mutation: Mutation,
    index: int = 2,
) -> Documents:
    """Return a triple after one document mutation."""

    result = copy.deepcopy(documents)
    mutation(result[index])
    return result


def renumber(document: Document) -> None:
    """Restore contiguous lifecycle sequence numbers after a mutation."""

    for sequence, event in enumerate(document["lifecycle"], 1):
        event["sequence"] = sequence


class Runner:
    """Invoke the comparator exactly as CI does and collect failures."""

    def __init__(
        self,
        executable: Path,
        temporary: Path,
        repository: Path,
        source_commit: str,
        greenlit_binary: Path,
    ) -> None:
        self.executable = executable
        self.temporary = temporary
        self.repository = repository
        self.source_commit = source_commit
        self.greenlit_binary = greenlit_binary
        self.count = 0
        self.failures: list[str] = []

    def check(
        self,
        label: str,
        documents: Documents,
        expected: int,
        *,
        ledger_text: str | None = None,
        repository_id: str = REPOSITORY_ID,
        source_commit: str | None = None,
        greenlit_binary: Path | None = None,
        repository_root: Path | None = None,
        raw: dict[int, dict[str, str]] | None = None,
        after_seal: SealedMutation | None = None,
        environment: dict[str, str] | None = None,
        fragments: tuple[str, ...] = (),
        forbidden_fragments: tuple[str, ...] = (),
    ) -> None:
        """Seal a triple, invoke the public CLI, and verify its result class."""

        self.count += 1
        prefix = self.temporary / f"case-{self.count:03d}"
        prefix.mkdir()
        capture_root = prefix / "live-evidence"
        try:
            documents = seal_captures(
                self.repository,
                self.source_commit,
                documents,
                capture_root,
                raw,
            )
        except (OSError, ValueError, subprocess.CalledProcessError) as error:
            self.failures.append(f"{label}: capture sealing failed: {error}")
            return
        if after_seal is not None:
            after_seal(documents, capture_root)
        paths = self._write_live_evidence(
            capture_root,
            documents,
            raw,
            label,
        )
        if paths is None:
            return
        ledger = prefix / "exceptions.md"
        ledger.write_text(ledger_text or empty_ledger(), encoding="utf-8")
        options = [
            "--repository-root",
            str(repository_root or self.repository),
            "--repository-id",
            repository_id,
            "--source-commit",
            source_commit or self.source_commit,
            "--greenlit-binary",
            str(greenlit_binary or self.greenlit_binary),
            "--capture-root",
            str(capture_root),
            "--exceptions",
            str(ledger),
        ]
        executable = self.executable
        if documents[0]["case_id"] == "contract-case":
            executable = (
                self.executable.parent
                / "parity_compare"
                / "selftest_entry.py"
            )
        command = [sys.executable, str(executable), *options, *map(str, paths)]
        invocation_environment = os.environ.copy()
        if environment:
            invocation_environment.update(environment)
        try:
            completed = subprocess.run(
                command,
                check=False,
                capture_output=True,
                text=True,
                timeout=30,
                env=invocation_environment,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            self.failures.append(f"{label}: comparator invocation failed: {error}")
            return
        self._record_result(
            label,
            expected,
            fragments,
            forbidden_fragments,
            completed,
        )

    def _write_live_evidence(
        self,
        capture_root: Path,
        documents: Documents,
        raw: dict[int, dict[str, str]] | None,
        label: str,
    ) -> list[Path] | None:
        paths: list[Path] = []
        for index, document in enumerate(documents):
            role = document["producer"]["role"]
            text = json.dumps(document, sort_keys=True, separators=(",", ":"))
            for sentinel, literal in (raw or {}).get(index, {}).items():
                encoded = json.dumps(sentinel)
                if text.count(encoded) != 1:
                    self.failures.append(
                        f"{label}: raw sentinel {sentinel!r} did not occur exactly once"
                    )
                    return None
                text = text.replace(encoded, literal)
            path = capture_root / f"seed-{role}.json"
            path.write_text(text + "\n", encoding="utf-8")
            path.chmod(0o600)
            paths.append(path)
        return paths

    def _record_result(
        self,
        label: str,
        expected: int,
        fragments: tuple[str, ...],
        forbidden_fragments: tuple[str, ...],
        completed: subprocess.CompletedProcess[str],
    ) -> None:
        combined = completed.stdout + completed.stderr
        category = {
            0: "parity match",
            1: "parity mismatch",
            2: "validation error",
        }.get(expected)
        required = (() if category is None else (category,)) + fragments
        if (
            completed.returncode != expected
            or any(item not in combined for item in required)
            or any(item in combined for item in forbidden_fragments)
        ):
            self.failures.append(
                f"{label}: expected exit {expected} with {required!r}; "
                f"forbidden={forbidden_fragments!r}; "
                f"got exit {completed.returncode}, stdout={completed.stdout!r}, "
                f"stderr={completed.stderr!r}"
            )


__all__ = [
    "Document",
    "Documents",
    "Mutation",
    "Runner",
    "altered",
    "changed",
    "renumber",
    "set_value",
]
