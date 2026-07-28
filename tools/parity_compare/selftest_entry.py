#!/usr/bin/env python3
"""Non-certifying command entrypoint for the synthetic schema self-test case."""

from __future__ import annotations

import importlib.machinery
import importlib.util
import sys
from pathlib import Path


sys.dont_write_bytecode = True
TOOLS = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(TOOLS))


def main() -> int:
    """Run the shared comparison pipeline with the fixed synthetic test case."""

    executable = TOOLS / "compare-parity"
    loader = importlib.machinery.SourceFileLoader(
        "parity_compare_production_entry",
        str(executable),
    )
    spec = importlib.util.spec_from_loader(
        "parity_compare_production_entry",
        loader,
    )
    if spec is None or spec.loader is None:
        print("self-test validation error: cannot load comparator", file=sys.stderr)
        return 2
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    parser = module._parser()
    arguments = parser.parse_args()
    if arguments.self_test:
        parser.error("synthetic self-test entry does not recurse")
    return module._compare(
        arguments,
        parser,
        expected_case="contract-case",
        success_label="self-test parity match",
    )


if __name__ == "__main__":
    sys.exit(main())
