#!/usr/bin/env python3
"""Public path-binding canaries for release transfer commands."""

from __future__ import annotations

import os
import tempfile
from pathlib import Path

from release_check_credential_bundle_canary_cases import (
    SOURCE,
    _digest,
    _directory,
    _evidence,
    _invoke,
    _pack,
    _prepared,
    _unpack,
)


def run_path_attacks() -> None:
    """Reject ancestor symlinks and multiply-linked transfer inputs."""

    previous_umask = os.umask(0o077)
    try:
        with tempfile.TemporaryDirectory(
            prefix="greenlit-transfer-path-canary."
        ) as raw:
            root = Path(raw)
            root.chmod(0o700)
            parent = root / "real-parent"
            _directory(parent, 0o700)
            prepared = parent / "prepared"
            _directory(prepared, 0o700)
            binary = _prepared(prepared)
            evidence = parent / "evidence"
            _directory(evidence, 0o700)
            _evidence(evidence, ("oracle", "greenlit-release"))
            bundle = parent / "local.tar"
            digest = _pack(root, "local", evidence, bundle, binary)

            alias = root / "parent-alias"
            alias.symlink_to(parent, target_is_directory=True)
            _unpack(
                root,
                "local",
                alias / "local.tar",
                root / "ancestor-symlink-out",
                digest,
                accepted=False,
            )
            _invoke(
                root,
                [
                    "pack-local",
                    "--input-root",
                    str(alias / "evidence"),
                    "--greenlit-binary",
                    str(binary),
                    "--output",
                    str(root / "ancestor-symlink.tar"),
                    "--expected-source",
                    SOURCE,
                ],
                accepted=False,
            )

            linked_bundle = root / "linked-bundle.tar"
            os.link(bundle, linked_bundle)
            _unpack(
                root,
                "local",
                linked_bundle,
                root / "hardlink-out",
                _digest(linked_bundle),
                accepted=False,
            )
    finally:
        os.umask(previous_umask)
