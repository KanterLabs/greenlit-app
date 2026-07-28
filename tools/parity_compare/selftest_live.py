"""Command canaries for private live evidence and exact-clean source binding."""

from __future__ import annotations

import copy
import hashlib
from pathlib import Path

from .capture_claims import semantic_sha256
from .selftest_repo import rewrite_capture
from .selftest_runner import Documents, Runner


def live_boundary_matrix(runner: Runner, base: Documents) -> None:
    """Reject unsafe live roots, capture links, event drift, and dirty source."""

    def public_root(_documents: Documents, capture_root: Path) -> None:
        capture_root.chmod(0o755)

    runner.check(
        "live capture root must remain private",
        base,
        2,
        after_seal=public_root,
    )

    def linked_capture(documents: Documents, capture_root: Path) -> None:
        role = documents[0]["producer"]["role"]
        case_id = documents[0]["case_id"]
        path = capture_root / "captures" / f"{case_id}-{role}.json"
        path.unlink()
        path.symlink_to(f"{case_id}-github-actions.json")

    runner.check(
        "live capture symlink is rejected",
        base,
        2,
        after_seal=linked_capture,
    )

    def extra_file(
        _documents: Documents,
        capture_root: Path,
    ) -> None:
        path = capture_root / "unbound-evidence"
        path.write_text("extra\n", encoding="utf-8")
        path.chmod(0o600)

    runner.check(
        "live root rejects extra evidence files",
        base,
        2,
        after_seal=extra_file,
        fragments=("exactly its fixed parity evidence files",),
    )

    def event_drift(capture: dict[str, object]) -> None:
        authority = capture.get("authority")
        if not isinstance(authority, dict):
            return
        github = authority.get("github-actions")
        if isinstance(github, dict):
            github["event"] = "workflow_dispatch"

    runner.check(
        "GitHub live event authority is exact push",
        base,
        2,
        after_seal=lambda documents, capture_root: rewrite_capture(
            capture_root,
            documents,
            1,
            event_drift,
        ),
    )

    def boolean_exit_codes(capture: dict[str, object]) -> None:
        authority = capture.get("authority")
        if isinstance(authority, dict) and isinstance(
            authority.get("oracle"), dict
        ):
            authority["oracle"]["step_exit_codes"] = [False, False]

    runner.check(
        "authority booleans cannot equal numeric exit codes",
        base,
        2,
        after_seal=lambda documents, capture_root: rewrite_capture(
            capture_root,
            documents,
            0,
            boolean_exit_codes,
        ),
    )

    numeric = copy.deepcopy(base)
    for document in numeric:
        document["outputs"][0]["value"] = 1

    def boolean_replay(capture: dict[str, object]) -> None:
        observation = capture.get("observation")
        authority = capture.get("authority")
        if not isinstance(observation, dict) or not isinstance(authority, dict):
            return
        observation["outputs"][0]["value"] = True
        authority["semantic_sha256"] = semantic_sha256(observation)

    runner.check(
        "capture replay keeps booleans distinct from numbers",
        numeric,
        2,
        after_seal=lambda documents, capture_root: rewrite_capture(
            capture_root,
            documents,
            2,
            boolean_replay,
        ),
    )

    def decimal_attempt(capture: dict[str, object]) -> None:
        observation = capture.get("observation")
        if not isinstance(observation, dict):
            return
        producer = observation.get("producer")
        if isinstance(producer, dict):
            producer["run_attempt"] = 1.0

    runner.check(
        "capture replay preserves integer token syntax",
        base,
        2,
        after_seal=lambda documents, capture_root: rewrite_capture(
            capture_root,
            documents,
            0,
            decimal_attempt,
        ),
    )

    dirty = runner.repository / "untracked-live-parity-canary"

    def dirty_source(_documents: Documents, _capture_root: Path) -> None:
        dirty.write_text("untracked\n", encoding="utf-8")

    try:
        runner.check(
            "live source must remain exactly clean",
            base,
            2,
            after_seal=dirty_source,
        )
    finally:
        dirty.unlink(missing_ok=True)


def version_process_matrix(runner: Runner, base: Documents) -> None:
    """Bound version output and descendants that retain inherited pipes."""

    cases = (
        (
            "release version output is bounded",
            b"  i=0\n"
            b"  while [ \"${i}\" -lt 5000 ]; do\n"
            b"    printf x\n"
            b"    i=$((i + 1))\n"
            b"  done\n"
            b"  exit 0\n",
            "output exceeds 4096 bytes",
        ),
        (
            "release version pipe holder is bounded",
            b"  sleep 30 &\n"
            b"  printf 'litci 0.0.0 (%s)\\n' '"
            + runner.source_commit.encode("ascii")
            + b"'\n"
            b"  exit 0\n",
            "exceeded 10 seconds",
        ),
        (
            "release version detached descendant is rejected",
            b"  setsid sh -c 'sleep 30' </dev/null >/dev/null 2>&1 &\n"
            b"  printf 'litci 0.0.0 (%s)\\n' '"
            + runner.source_commit.encode("ascii")
            + b"'\n"
            b"  exit 0\n",
            "left a descendant process",
        ),
    )
    for index, (label, body, fragment) in enumerate(cases):
        binary = runner.temporary / f"hostile-version-{index}" / "litci"
        binary.parent.mkdir()
        raw = (
            b"#!/bin/sh\n"
            b"if [ \"$#\" -eq 1 ] && [ \"$1\" = \"--version\" ]; then\n"
            + body
            + b"fi\n"
            b"exit 64\n"
        )
        binary.write_bytes(raw)
        binary.chmod(0o755)
        documents = copy.deepcopy(base)
        digest = hashlib.sha256(raw).hexdigest()
        documents[2]["producer"]["binary_sha256"] = digest
        runner.check(
            label,
            documents,
            2,
            greenlit_binary=binary,
            fragments=(fragment,),
        )


__all__ = ["live_boundary_matrix", "version_process_matrix"]
