"""Command canaries for immutable source and producer provenance."""

from __future__ import annotations

import copy
import hashlib
import subprocess
from pathlib import Path

from .selftest_data import WORKFLOW_BYTES, WORKFLOW_PATH, release_binary_bytes
from .selftest_repo import (
    install_empty_commit_replacement,
    remove_replacement,
    rewrite_capture,
)
from .selftest_runner import Documents, Runner


def _git(repository: Path, *arguments: str) -> None:
    subprocess.run(
        ["git", "-C", str(repository), *arguments],
        check=True,
        capture_output=True,
    )


def provenance_audit_matrix(runner: Runner, base: Documents) -> None:
    """Reject redirects, concealment, replacement refs, and false claims."""

    unrelated = runner.temporary / "not-a-repository"
    unrelated.mkdir()
    runner.check(
        "inherited Git environment cannot redirect repository",
        base,
        2,
        repository_root=unrelated,
        environment={
            "GIT_DIR": str(runner.repository / ".git"),
            "GIT_WORK_TREE": str(runner.repository),
        },
    )
    replacement_installed = [False]

    def install_replacement(
        _documents: Documents,
        _capture_root: Path,
    ) -> None:
        install_empty_commit_replacement(runner.repository, runner.source_commit)
        replacement_installed[0] = True

    try:
        runner.check(
            "Git replacement refs cannot substitute trusted source",
            base,
            0,
            after_seal=install_replacement,
        )
    finally:
        if replacement_installed[0]:
            remove_replacement(runner.repository, runner.source_commit)

    concealed = [False]

    def conceal_modified_workflow(
        _documents: Documents,
        _capture_root: Path,
    ) -> None:
        _git(runner.repository, "update-index", "--skip-worktree", WORKFLOW_PATH)
        concealed[0] = True
        workflow = runner.repository / WORKFLOW_PATH
        workflow.write_bytes(workflow.read_bytes() + b"# concealed drift\n")

    try:
        runner.check(
            "skip-worktree cannot conceal modified source",
            base,
            2,
            after_seal=conceal_modified_workflow,
            fragments=("index flags",),
        )
    finally:
        if concealed[0]:
            _git(
                runner.repository,
                "update-index",
                "--no-skip-worktree",
                WORKFLOW_PATH,
            )
            (runner.repository / WORKFLOW_PATH).write_bytes(WORKFLOW_BYTES)

    workflow = runner.repository / WORKFLOW_PATH

    def conceal_executable_mode(
        _documents: Documents,
        _capture_root: Path,
    ) -> None:
        _git(runner.repository, "config", "core.filemode", "false")
        workflow.chmod(0o755)

    try:
        runner.check(
            "local Git config cannot conceal tracked mode drift",
            base,
            2,
            after_seal=conceal_executable_mode,
            fragments=("executable mode differs",),
        )
    finally:
        workflow.chmod(0o644)
        _git(runner.repository, "config", "core.filemode", "true")

    attributes = runner.repository / ".git" / "info" / "attributes"
    canonical = runner.temporary / "canonical-filter-source"
    canonical.write_bytes(WORKFLOW_BYTES)

    def conceal_filtered_bytes(
        _documents: Documents,
        _capture_root: Path,
    ) -> None:
        attributes.write_text(
            f"{WORKFLOW_PATH} filter=mask\n",
            encoding="utf-8",
        )
        _git(
            runner.repository,
            "config",
            "filter.mask.clean",
            f"cat {canonical}",
        )
        workflow.write_bytes(
            WORKFLOW_BYTES.replace(b"Parity seed", b"Parity evil", 1)
        )

    try:
        runner.check(
            "clean filters cannot conceal tracked byte drift",
            base,
            2,
            after_seal=conceal_filtered_bytes,
            fragments=("raw worktree bytes differ",),
        )
    finally:
        workflow.write_bytes(WORKFLOW_BYTES)
        attributes.unlink(missing_ok=True)
        _git(
            runner.repository,
            "config",
            "--unset-all",
            "filter.mask.clean",
        )

    def corrupt_oracle(capture: dict[str, object]) -> None:
        authority = capture["authority"]
        if not isinstance(authority, dict):
            return
        oracle = authority["oracle"]
        if isinstance(oracle, dict):
            oracle["run_block_sha256"] = ["f" * 64, "f" * 64]
            oracle["bash_path"] = "/definitely/not/bash"

    runner.check(
        "oracle claims are independently derived",
        base,
        2,
        after_seal=lambda documents, capture_root: rewrite_capture(
            capture_root,
            documents,
            0,
            corrupt_oracle,
        ),
    )

    wrong_commit = "f" * 40
    wrong_binary = (
        runner.temporary / "wrong-source-target" / "release" / "litci"
    )
    wrong_binary.parent.mkdir(parents=True)
    raw = release_binary_bytes(wrong_commit)
    wrong_binary.write_bytes(raw)
    wrong_binary.chmod(0o755)
    wrong_documents = copy.deepcopy(base)
    digest = hashlib.sha256(raw).hexdigest()
    wrong_documents[2]["producer"]["binary_sha256"] = digest
    runner.check(
        "release binary embeds another source commit",
        wrong_documents,
        2,
        greenlit_binary=wrong_binary,
    )


__all__ = ["provenance_audit_matrix"]
