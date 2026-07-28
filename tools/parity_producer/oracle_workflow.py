"""Fail-closed derivation of the fixed shell blocks executed by the oracle."""

from __future__ import annotations

from parity_producer.common import ProducerError
from parity_producer.contract import EXPECTED_STEPS


EXPRESSION = "${{ steps.emit.outputs.seed_value }}"
EXPECTED_RUN_BLOCKS = (
    """set -euo pipefail
printf '%s\\n' 'PARITY_IDENTITY job=shell step=emit'
printf '%s\\n' 'seed_value=greenlit' >> "${GITHUB_OUTPUT}"
""",
    """set -euo pipefail
printf '%s\\n' 'PARITY_IDENTITY job=shell step=verify'
test '${{ steps.emit.outputs.seed_value }}' = 'greenlit'
printf 'PARITY_OUTPUT seed_value=%s\\n' \\
  '${{ steps.emit.outputs.seed_value }}'
printf '%s\\n' 'greenlit' > parity-seed.txt
test "$(cat parity-seed.txt)" = 'greenlit'
mode="$(stat -c '%a' parity-seed.txt)"
digest="$(sha256sum parity-seed.txt | cut -d ' ' -f 1)"
printf 'PARITY_CONTEXT github.job=%s\\n' "${GITHUB_JOB}"
printf 'PARITY_CONTEXT github.workflow=%s\\n' "${GITHUB_WORKFLOW}"
printf 'PARITY_CONTEXT runner.arch=%s\\n' "${RUNNER_ARCH}"
printf 'PARITY_CONTEXT runner.os=%s\\n' "${RUNNER_OS}"
printf 'PARITY_TEMPORARY_DIRECTORY %s\\n' "${RUNNER_TEMP}"
printf 'PARITY_PROBE parity-seed-file mode=0%s sha256=%s\\n' \\
  "${mode}" "${digest}"
""",
)


def extract_run_blocks(workflow: bytes) -> tuple[str, str]:
    """Extract the two exact authored run blocks from committed workflow bytes."""
    try:
        lines = workflow.decode("utf-8").splitlines(keepends=True)
    except UnicodeDecodeError as error:
        raise ProducerError("committed parity workflow is not UTF-8") from error
    blocks: list[str] = []
    for step_id, _ in EXPECTED_STEPS:
        header = f"      - id: {step_id}\n"
        indices = [index for index, line in enumerate(lines) if line == header]
        if len(indices) != 1:
            raise ProducerError(
                f"committed parity workflow does not contain one exact {step_id!r} step"
            )
        start = indices[0]
        end = next(
            (
                index
                for index in range(start + 1, len(lines))
                if lines[index].startswith("      - id: ")
                or (
                    lines[index].strip()
                    and not lines[index].startswith("        ")
                )
            ),
            len(lines),
        )
        run_lines = [
            index
            for index in range(start + 1, end)
            if lines[index] == "        run: |\n"
        ]
        if len(run_lines) != 1:
            raise ProducerError(f"step {step_id!r} lacks one literal run block")
        block_lines: list[str] = []
        for line in lines[run_lines[0] + 1 : end]:
            if line.strip() and not line.startswith("          "):
                break
            block_lines.append(line[10:] if line.startswith("          ") else "\n")
        block = "".join(block_lines)
        if not block or not block.endswith("\n"):
            raise ProducerError(
                f"step {step_id!r} run block is empty or unterminated"
            )
        blocks.append(block)
    return blocks[0], blocks[1]
