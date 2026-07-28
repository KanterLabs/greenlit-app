#!/usr/bin/env python3
"""Public command-boundary canary for split-job release transfer bundles."""

from __future__ import annotations

import subprocess
import sys
import tarfile

sys.dont_write_bytecode = True

from release_check_credential_bundle_canary_cases import CanaryError, run_cases
from release_check_credential_bundle_finalizer_canary import run_finalizer_mismatch
from release_check_credential_bundle_path_canary import run_path_attacks


def main() -> int:
    """Run every transfer round trip and negative through public commands."""

    try:
        run_cases()
        run_path_attacks()
        run_finalizer_mismatch()
    except (
        CanaryError,
        OSError,
        subprocess.TimeoutExpired,
        tarfile.TarError,
        UnicodeError,
    ) as error:
        print(f"release transfer self-test failed: {error}", file=sys.stderr)
        return 1
    print(
        "release transfer self-test passed: four command-boundary round trips; "
        "canonical/archive/path/symlink/hardlink/source/role/mode/finalizer "
        "mismatch negatives"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
