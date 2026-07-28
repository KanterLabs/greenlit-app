"""Fixed identities shared by the live-parity gate."""

from __future__ import annotations

import re


REPOSITORY_ID = "KanterLabs/greenlit-app"
WORKFLOW_PATH = ".github/workflows/parity-seed.yml"
WORKFLOW_NAME = "Parity seed"
CASE_ID = "shell-only-seed"
ROLES = ("oracle", "github-actions", "greenlit-release")
PRODUCTION_ROLES = ("oracle", "greenlit-release", "github-actions")
COMMIT = re.compile(r"^[0-9a-f]{40}$")
POSITIVE_INTEGER = re.compile(r"^[1-9][0-9]*$")
POLL_SECONDS = 10
POLL_TIMEOUT_SECONDS = 30 * 60
MAX_API_BYTES = 8 * 1024 * 1024
