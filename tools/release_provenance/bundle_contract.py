"""The exact sealed-tar closure and fixed resource limits."""

from __future__ import annotations

from .common import BINARY_BASENAME, CRATES, MANIFEST_NAME, PARITY_FILES


MAX_BUNDLE_BYTES = 512 * 1024 * 1024
MAX_BUNDLE_FILE_BYTES = 256 * 1024 * 1024
MAX_BUNDLE_EXPANDED_BYTES = 512 * 1024 * 1024


def _closure() -> dict[str, tuple[str, int, str | None]]:
    entries: dict[str, tuple[str, int, str | None]] = {
        "release-candidate": ("directory", 0o700, None),
        "release-candidate/target": ("directory", 0o755, None),
        "release-candidate/target/package": ("directory", 0o755, None),
        "release-candidate/target/release": ("directory", 0o755, None),
        "parity-evidence": ("directory", 0o700, None),
        "parity-evidence/captures": ("directory", 0o700, None),
        f"release-candidate/{MANIFEST_NAME}": (
            "file",
            0o644,
            MANIFEST_NAME,
        ),
        f"release-candidate/target/release/{BINARY_BASENAME}": (
            "file",
            0o755,
            f"target/release/{BINARY_BASENAME}",
        ),
    }
    entries.update(
        {
            f"release-candidate/target/package/{basename}": (
                "file",
                0o644,
                f"target/package/{basename}",
            )
            for basename in CRATES
        }
    )
    entries.update(
        {
            f"parity-evidence/{relative}": ("file", 0o600, relative)
            for relative in PARITY_FILES
        }
    )
    return entries


CLOSURE = _closure()
