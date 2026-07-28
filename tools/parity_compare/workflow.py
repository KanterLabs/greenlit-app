"""Declared-workflow binding for the fixed shell-only parity seed."""

from __future__ import annotations

from typing import Any

from . import ContractError


_DISPATCH_DECLARATION = "  workflow_dispatch:"
_EXPECTED_DECLARATIONS = (
    "name: Parity seed",
    "on:",
    "  push:",
    "    branches:",
    "      - main",
    '      - "stabilization/**"',
    _DISPATCH_DECLARATION,
    "permissions: {}",
    "jobs:",
    "  shell:",
    "    name: Shell-only parity seed",
    "    runs-on: homelab",
    "    steps:",
    "      - id: emit",
    "        name: Emit deterministic output",
    "        shell: bash",
    "        run: |",
    "          set -euo pipefail",
    "          printf '%s\\n' 'PARITY_IDENTITY job=shell step=emit'",
    "          printf '%s\\n' 'seed_value=greenlit' >> \"${GITHUB_OUTPUT}\"",
    "      - id: verify",
    "        name: Verify shell and filesystem behavior",
    "        shell: bash",
    "        run: |",
    "          set -euo pipefail",
    "          printf '%s\\n' 'PARITY_IDENTITY job=shell step=verify'",
    "          test '${{ steps.emit.outputs.seed_value }}' = 'greenlit'",
    "          printf 'PARITY_OUTPUT seed_value=%s\\n' \\",
    "            '${{ steps.emit.outputs.seed_value }}'",
    "          umask 0022",
    "          printf '%s\\n' 'greenlit' > parity-seed.txt",
    "          test \"$(cat parity-seed.txt)\" = 'greenlit'",
    "          mode=\"$(stat -c '%a' parity-seed.txt)\"",
    "          digest=\"$(sha256sum parity-seed.txt | cut -d ' ' -f 1)\"",
    "          printf 'PARITY_CONTEXT github.job=%s\\n' \"${GITHUB_JOB}\"",
    "          printf 'PARITY_CONTEXT github.workflow=%s\\n' \"${GITHUB_WORKFLOW}\"",
    "          printf 'PARITY_CONTEXT runner.arch=%s\\n' \"${RUNNER_ARCH}\"",
    "          printf 'PARITY_CONTEXT runner.os=%s\\n' \"${RUNNER_OS}\"",
    "          printf 'PARITY_TEMPORARY_DIRECTORY %s\\n' \"${RUNNER_TEMP}\"",
    "          printf 'PARITY_PROBE parity-seed-file mode=0%s sha256=%s\\n' \\",
    "            \"${mode}\" \"${digest}\"",
)


def _declarations(workflow: bytes) -> tuple[str, ...]:
    try:
        text = workflow.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ContractError(
            "$.source.workflow_path: seed workflow is not UTF-8"
        ) from error
    if "\r" in text or "\t" in text:
        raise ContractError(
            "$.source.workflow_path: seed workflow uses noncanonical whitespace"
        )
    return tuple(
        line
        for line in text.splitlines()
        if line and not line.startswith("#")
    )


def _without_optional_dispatch(lines: tuple[str, ...]) -> tuple[str, ...]:
    return tuple(line for line in lines if line != _DISPATCH_DECLARATION)


def validate_seed_workflow(
    _observation: dict[str, Any],
    workflow: bytes,
) -> None:
    """Require exact scoped triggers, job declarations, and authored run steps."""

    declarations = _declarations(workflow)
    without_dispatch = _without_optional_dispatch(_EXPECTED_DECLARATIONS)
    if declarations not in {_EXPECTED_DECLARATIONS, without_dispatch}:
        raise ContractError(
            "$.source.workflow_path: seed workflow declarations changed; "
            "require scoped push and the exact shell job and authored steps"
        )


__all__ = ["validate_seed_workflow"]
