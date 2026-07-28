"""Command-line interface for canonical three-party parity comparison."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from . import ContractError
from .diff import (
    ComparisonContractError,
    compare_triple,
    render_mismatch,
)
from .exceptions import ExceptionContractError, load_exception_ledger
from .live_capture import (
    LiveCaptureRootIdentity,
    assert_live_capture_root_unchanged,
    assert_live_capture_topology,
    bind_live_capture_root,
    read_live_observation,
)
from .provenance import validate_provenance
from .repository import (
    RepositoryIdentity,
    assert_repository_unchanged,
    bind_repository,
)
from .schema import validate_observation
from .selftest import run_self_test
from .values import load_json_bytes


REPOSITORY_ROOT = Path(__file__).resolve().parent.parent.parent
DEFAULT_EXCEPTIONS = REPOSITORY_ROOT / "docs" / "PARITY-EXCEPTIONS.md"
ROLES = ("oracle", "github-actions", "greenlit-release")


def parser() -> argparse.ArgumentParser:
    """Build the public parity-comparison argument parser."""

    result = argparse.ArgumentParser(
        description=(
            "validate and compare ORACLE, GITHUB, and GREENLIT "
            "ParityObservationV1 documents"
        )
    )
    result.add_argument(
        "--repository-root",
        type=Path,
        help="exact-clean Git checkout containing the immutable workflow source",
    )
    result.add_argument(
        "--repository-id",
        help="trusted GitHub API repository.full_name (OWNER/REPO)",
    )
    result.add_argument(
        "--source-commit",
        help="trusted full lowercase 40-character source commit",
    )
    result.add_argument(
        "--greenlit-binary",
        type=Path,
        help="exact release-built litci binary used by greenlit-release",
    )
    result.add_argument(
        "--capture-root",
        type=Path,
        help="private live evidence root containing fixed role capture paths",
    )
    result.add_argument(
        "--exceptions",
        type=Path,
        default=DEFAULT_EXCEPTIONS,
        help="approved parity-exception Markdown ledger",
    )
    result.add_argument(
        "--self-test",
        action="store_true",
        help="run isolated command-boundary contract canaries",
    )
    result.add_argument(
        "observations",
        metavar="OBSERVATION",
        nargs="*",
        type=Path,
        help="ORACLE.json GITHUB.json GREENLIT.json, in that exact order",
    )
    return result


def _required_inputs(
    argument_parser: argparse.ArgumentParser,
    arguments: argparse.Namespace,
) -> tuple[Path, str, str, Path, Path, tuple[Path, Path, Path]]:
    if arguments.repository_root is None:
        argument_parser.error("--repository-root is required")
    if arguments.repository_id is None:
        argument_parser.error("--repository-id is required")
    if arguments.source_commit is None:
        argument_parser.error("--source-commit is required")
    if arguments.greenlit_binary is None:
        argument_parser.error("--greenlit-binary is required")
    if arguments.capture_root is None:
        argument_parser.error("--capture-root is required")
    if len(arguments.observations) != len(ROLES):
        argument_parser.error("ORACLE, GITHUB, and GREENLIT observations are required")
    return (
        arguments.repository_root,
        arguments.repository_id,
        arguments.source_commit,
        arguments.greenlit_binary,
        arguments.capture_root,
        tuple(arguments.observations),
    )


def _load_validated(
    path: Path,
    role: str,
    repository: RepositoryIdentity,
    capture_root: LiveCaptureRootIdentity,
    repository_id: str,
    source_commit: str,
    greenlit_binary: Path,
) -> dict[str, object]:
    raw = read_live_observation(capture_root, path, role)
    document = load_json_bytes(raw, role, str(path))

    def provenance(root: dict[str, object]) -> None:
        validate_provenance(
            root,
            repository,
            capture_root,
            repository_id,
            source_commit,
            role,
            greenlit_binary,
        )

    return validate_observation(document, provenance)


def compare_command(
    arguments: argparse.Namespace,
    argument_parser: argparse.ArgumentParser,
    *,
    expected_case: str = "shell-only-seed",
    success_label: str = "parity match",
) -> int:
    """Run one validated three-party comparison."""

    (
        repository,
        repository_id,
        source_commit,
        greenlit_binary,
        capture_root,
        paths,
    ) = _required_inputs(argument_parser, arguments)
    try:
        identity = bind_repository(repository)
        try:
            capture_identity = bind_live_capture_root(capture_root, identity)
            try:
                ledger = load_exception_ledger(
                    arguments.exceptions,
                    repository_id=repository_id,
                )
                documents = tuple(
                    _load_validated(
                        path,
                        role,
                        identity,
                        capture_identity,
                        repository_id,
                        source_commit,
                        greenlit_binary,
                    )
                    for path, role in zip(paths, ROLES, strict=True)
                )
                if any(
                    document["case_id"] != expected_case
                    for document in documents
                ):
                    raise ContractError(
                        "comparison case must be the fixed "
                        f"{expected_case!r} contract"
                    )
                result = compare_triple(*documents, ledger.active)
                assert_live_capture_topology(
                    capture_identity,
                    documents[0]["case_id"],
                )
            finally:
                assert_live_capture_root_unchanged(capture_identity)
        finally:
            assert_repository_unchanged(identity)
    except (
        ContractError,
        ExceptionContractError,
        ComparisonContractError,
        RecursionError,
    ) as error:
        print(f"validation error: {error}", file=sys.stderr)
        return 2

    mismatches = (
        ("github-actions", result.github_mismatches),
        ("greenlit-release", result.greenlit_mismatches),
    )
    if any(items for _, items in mismatches):
        for label, items in mismatches:
            for mismatch in items:
                print(
                    f"parity mismatch: {render_mismatch(mismatch, label)}",
                    file=sys.stderr,
                )
        return 1

    suffix = ""
    if result.applied_exceptions:
        details = ", ".join(
            f"{row.exception_id} at {row.exact_field}"
            for row in sorted(
                result.applied_exceptions,
                key=lambda row: row.exception_id,
            )
        )
        suffix = (
            f" ({len(result.applied_exceptions)} approved exception(s): "
            f"{details})"
        )
    print(f"{success_label}: case {documents[0]['case_id']!r}{suffix}")
    return 0


def main(executable: Path) -> int:
    """Dispatch the public comparator command."""

    argument_parser = parser()
    arguments = argument_parser.parse_args()
    if arguments.self_test:
        if arguments.observations:
            argument_parser.error("--self-test does not accept observation paths")
        return run_self_test(executable)
    return compare_command(arguments, argument_parser)


__all__ = ["compare_command", "main", "parser"]
