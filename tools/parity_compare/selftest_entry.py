#!/usr/bin/python3 -I
"""Non-certifying command entrypoint for the synthetic schema self-test case."""

from __future__ import annotations

import os
import sys

if not sys.flags.isolated:
    os.execve(
        "/usr/bin/python3",
        ["/usr/bin/python3", "-I", "-B", __file__, *sys.argv[1:]],
        os.environ,
    )

sys.dont_write_bytecode = True
sys.pycache_prefix = "/proc/self/fd/greenlit-impossible-pycache"

from pathlib import Path

_ENTRYPOINT = Path(__file__).absolute()
if _ENTRYPOINT.is_symlink():
    print(
        "synthetic parity comparison failed: launcher must not be a symbolic link",
        file=sys.stderr,
    )
    raise SystemExit(1)
TOOLS = _ENTRYPOINT.parent.parent
sys.path[:0] = [str(TOOLS)]

from parity_compare.cli import compare_command, parser


def main() -> int:
    """Run the shared comparison pipeline with the fixed synthetic test case."""

    argument_parser = parser()
    arguments = argument_parser.parse_args()
    if arguments.self_test:
        argument_parser.error("synthetic self-test entry does not recurse")
    return compare_command(
        arguments,
        argument_parser,
        expected_case="contract-case",
        success_label="self-test parity match",
    )


if __name__ == "__main__":
    sys.exit(main())
