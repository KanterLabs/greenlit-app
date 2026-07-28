"""Command-line routing for canonical live parity production."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from parity_producer.capture import publish
from parity_producer.common import AUTHORITATIVE_REPOSITORY, COMMIT, ProducerError
from parity_producer.github import produce_github
from parity_producer.live_root import validate_live_roots
from parity_producer.local import produce_local
from parity_producer.oracle import produce_oracle


def main(arguments: list[str] | None = None) -> int:
    """Run the producer CLI and return a stable process status."""
    args = _parser().parse_args(arguments)
    try:
        args.checkout, args.output_root = validate_live_roots(
            args.checkout, args.output_root, args.source_commit
        )
        if args.command == "oracle":
            production = produce_oracle(
                checkout=args.checkout,
                repository_id=args.repository_id,
                trusted_source_commit=args.source_commit,
            )
        elif args.command == "greenlit-release":
            production = produce_local(
                binary=args.binary,
                repository=args.checkout,
                home=args.home,
                repository_id=args.repository_id,
                trusted_source_commit=args.source_commit,
            )
        else:
            production = produce_github(
                repository=args.repository_id,
                run_id=args.run_id,
                trusted_source_commit=args.source_commit,
                run_json=args.run_json,
                jobs_json=args.jobs_json,
                content_json=args.content_json,
                job_log_path=args.job_log,
                self_test_raw_evidence=args.self_test_raw_evidence,
                self_test_gh_executable=args.self_test_gh_executable,
            )
        capture, observation = publish(
            production,
            checkout=args.checkout,
            output_root=args.output_root,
            trusted_repository=args.repository_id,
            trusted_source_commit=args.source_commit,
        )
    except ProducerError as error:
        print(f"parity observation not produced: {error}", file=sys.stderr)
        return 2
    print(f"wrote parity capture: {capture}")
    print(f"wrote parity observation: {observation}")
    return 0


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="tools/collect-parity-observation",
        description=(
            "Produce canonical Phase 12 oracle, GitHub Actions, and "
            "release-built Greenlit live parity evidence."
        ),
    )
    commands = parser.add_subparsers(dest="command", required=True)

    oracle = commands.add_parser(
        "oracle",
        help="execute the exact committed shell blocks directly under bash/coreutils",
    )
    _trusted_arguments(oracle)
    _output_argument(oracle)

    greenlit = commands.add_parser(
        "greenlit-release",
        help="run the exact prebuilt Greenlit release and project retained evidence",
    )
    _trusted_arguments(greenlit)
    _output_argument(greenlit)
    greenlit.add_argument(
        "--binary",
        type=Path,
        required=True,
        help="prebuilt <target-dir>/release/litci retained for the atomic comparator",
    )
    greenlit.add_argument(
        "--home",
        type=Path,
        required=True,
        help="private empty HOME in which the retained run remains available",
    )

    github = commands.add_parser(
        "github-actions",
        help="fetch and project one exact same-commit live Actions run",
    )
    _trusted_arguments(github)
    _output_argument(github)
    github.add_argument("--run-id", type=_positive_integer, required=True)
    raw = github.add_argument_group(
        "behavior-gate raw evidence",
        (
            "canonical production omits these options and queries with `gh api`; "
            "the repository behavior gate alone supplies all four mocked boundaries"
        ),
    )
    raw.add_argument("--run-json", type=Path)
    raw.add_argument("--jobs-json", type=Path)
    raw.add_argument("--content-json", type=Path)
    raw.add_argument("--job-log", type=Path)
    raw.add_argument(
        "--self-test-raw-evidence",
        action="store_true",
        help=argparse.SUPPRESS,
    )
    raw.add_argument(
        "--self-test-gh-executable",
        type=Path,
        help=argparse.SUPPRESS,
    )

    return parser


def _trusted_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--checkout",
        type=Path,
        required=True,
        help="capture worktree containing the trusted source commit",
    )
    parser.add_argument(
        "--repository-id",
        required=True,
        help=f"trusted repository identity (exactly {AUTHORITATIVE_REPOSITORY})",
    )
    parser.add_argument(
        "--source-commit",
        type=_commit,
        required=True,
        help="trusted full commit shared by all three producers",
    )


def _output_argument(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--output-root",
        type=Path,
        required=True,
        help=(
            "absolute mode-0700 directory outside the checkout for private "
            "live captures and observations"
        ),
    )


def _positive_integer(value: str) -> int:
    try:
        parsed = int(value, 10)
    except ValueError as error:
        raise argparse.ArgumentTypeError("expected a positive integer") from error
    if parsed <= 0:
        raise argparse.ArgumentTypeError("expected a positive integer")
    return parsed


def _commit(value: str) -> str:
    if COMMIT.fullmatch(value) is None:
        raise argparse.ArgumentTypeError(
            "expected a full lowercase 40-character Git commit"
        )
    return value
