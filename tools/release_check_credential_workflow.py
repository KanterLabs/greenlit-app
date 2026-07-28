#!/usr/bin/env python3
"""Fail-closed authority for credential-isolated CI and release workflows."""

from __future__ import annotations

import re
from pathlib import Path


CHECKOUT = "actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683"
DOWNLOAD = "actions/download-artifact@d3f86a106a0bac45b974a628896c90dbdf5c8093"
UPLOAD = "actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02"
PINNED_ACTION = re.compile(
    r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+@[0-9a-f]{40}"
)
CONTEXT_ROOT = re.compile(
    r"(?i)(?<![A-Za-z0-9_])(github|secrets|runner|needs|steps|inputs)"
    r"(?![A-Za-z0-9_])"
)
CONTEXT_PROPERTY = re.compile(r"\s*\.\s*(\*|[A-Za-z_][A-Za-z0-9_-]*)")
BLOCK_MARKERS = {"|", "|-", "|+", ">", ">-", ">+"}
CREDENTIAL_KEYS = {
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
}

CI_JOBS = {
    "ci",
    "runtime-integration",
    "credential-capability",
    "host-deep-path",
    "provider-and-policy",
    "live_parity_local",
    "live_parity_github",
    "live_parity_compare",
    "dogfood",
}
RELEASE_JOBS = {
    "prepare",
    "local_parity",
    "github_parity",
    "finalize",
    "publish",
}


class WorkflowError(Exception):
    """A workflow escaped the credential-isolation allowlist."""


def _yaml_authority(text: str) -> None:
    """Reject YAML features that could hide or replace inspected mappings."""

    if "\t" in text or "\r" in text:
        raise WorkflowError("workflow contains unsupported whitespace")
    active: list[tuple[int, int]] = []
    seen: dict[tuple[tuple[int, ...], int], set[str]] = {}
    sequence = 0
    block_indent: int | None = None

    def mapping(indent: int, content: str) -> None:
        nonlocal block_indent
        match = re.fullmatch(r"([A-Za-z0-9_-]+):(.*)", content)
        if match is None:
            raise WorkflowError("workflow contains an unsupported or quoted key")
        key, remainder = match.groups()
        while active and active[-1][0] >= indent:
            active.pop()
        scope = (tuple(identifier for _, identifier in active), indent)
        keys = seen.setdefault(scope, set())
        if key in keys:
            raise WorkflowError(f"workflow repeats YAML key {key}")
        keys.add(key)
        value = remainder.strip()
        if value.startswith(("{", "[")):
            raise WorkflowError("workflow uses an uninspected flow collection")
        expression_free = re.sub(r"\$\{\{.*?\}\}", "", value)
        if re.search(
            r"(?:^|[\s,\[\]{])(?:&|\*)[A-Za-z0-9_-]+"
            r"(?:$|[\s,\[\]}])",
            expression_free,
        ):
            raise WorkflowError("workflow uses a YAML anchor or alias")
        if value in BLOCK_MARKERS:
            block_indent = indent
        elif not value:
            active.append((indent, len(seen) + len(active) + sequence))

    for line in text.splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        indent = len(line) - len(line.lstrip(" "))
        if block_indent is not None and indent > block_indent:
            continue
        block_indent = None
        if indent % 2:
            raise WorkflowError("workflow has noncanonical indentation")
        content = line[indent:]
        if content in {"---", "..."} or content.startswith("%"):
            raise WorkflowError("workflow contains an unsupported YAML directive")
        if content.startswith("- "):
            while active and active[-1][0] >= indent:
                active.pop()
            sequence += 1
            active.append((indent, -sequence))
            item = content[2:]
            if ":" in item:
                mapping(indent + 2, item)
            elif re.search(r"(?:^|[\s])(?:&|\*)[A-Za-z0-9_-]+", item):
                raise WorkflowError("workflow uses a YAML anchor or alias")
            continue
        if content.startswith("-"):
            raise WorkflowError("workflow contains an unsupported sequence item")
        mapping(indent, content)


def _job_blocks(text: str) -> dict[str, str]:
    lines = text.splitlines()
    try:
        start = lines.index("jobs:") + 1
    except ValueError as error:
        raise WorkflowError("workflow has no jobs mapping") from error
    headers: list[tuple[int, str]] = []
    for index in range(start, len(lines)):
        line = lines[index]
        if not line.startswith("  ") or line.startswith("    "):
            continue
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        match = re.fullmatch(r"  ([A-Za-z0-9_-]+):", line)
        if match is None:
            raise WorkflowError("workflow contains an unsupported job key")
        headers.append((index, match.group(1)))
    if not headers:
        raise WorkflowError("workflow jobs mapping is empty")
    return {
        name: "\n".join(
            lines[index : headers[position + 1][0]]
            if position + 1 < len(headers)
            else lines[index:]
        )
        + "\n"
        for position, (index, name) in enumerate(headers)
    }


def _field(block: str, name: str) -> str:
    matches = re.findall(rf"^    {re.escape(name)}: *(.*)$", block, re.MULTILINE)
    if len(matches) != 1 or not matches[0]:
        raise WorkflowError(f"job lacks one scalar {name}")
    return matches[0]


def _keys(block: str, spaces: int) -> set[str]:
    prefix = " " * spaces
    values: list[str] = []
    for line in block.splitlines():
        if not line.startswith(prefix) or line.startswith(prefix + " "):
            continue
        match = re.fullmatch(r"([A-Za-z0-9_-]+):(?: .*)?", line[spaces:])
        if match:
            values.append(match.group(1))
    return set(values)


def _mapping_values(
    block: str,
    name: str,
    *,
    header_spaces: int,
) -> dict[str, str]:
    lines = block.splitlines()
    header = f"{' ' * header_spaces}{name}:"
    if lines.count(header) != 1:
        return {}
    start = lines.index(header) + 1
    result: dict[str, str] = {}
    for line in lines[start:]:
        indent = len(line) - len(line.lstrip(" "))
        if line.strip() and indent <= header_spaces:
            break
        match = re.fullmatch(
            rf"{' ' * (header_spaces + 2)}([A-Za-z0-9_-]+): (.+)", line
        )
        if match:
            result[match.group(1)] = match.group(2)
    return result


def _needs(block: str) -> set[str]:
    scalar = re.findall(r"^    needs: *(.*)$", block, re.MULTILINE)
    if len(scalar) != 1:
        raise WorkflowError("job must declare needs exactly once")
    if scalar[0]:
        if re.fullmatch(r"[A-Za-z0-9_-]+", scalar[0]) is None:
            raise WorkflowError("job has a noncanonical needs value")
        return {scalar[0]}
    lines = block.splitlines()
    start = lines.index("    needs:") + 1
    result: set[str] = set()
    for line in lines[start:]:
        match = re.fullmatch(r"      - ([A-Za-z0-9_-]+)", line)
        if match:
            result.add(match.group(1))
        elif line.strip() and len(line) - len(line.lstrip(" ")) <= 4:
            break
    return result


def _steps(block: str) -> list[str]:
    lines = block.splitlines()
    if lines.count("    steps:") != 1:
        raise WorkflowError("job must contain one steps sequence")
    begin = lines.index("    steps:") + 1
    end = len(lines)
    for index in range(begin, len(lines)):
        line = lines[index]
        if line.strip() and len(line) - len(line.lstrip(" ")) <= 4:
            end = index
            break
    starts: list[int] = []
    for index in range(begin, end):
        line = lines[index]
        if line.startswith("      -"):
            if re.fullmatch(r"      - name: .+", line) is None:
                raise WorkflowError("workflow contains an unnamed step")
            starts.append(index)
    if not starts:
        raise WorkflowError("job steps sequence is empty")
    return [
        "\n".join(lines[start : starts[position + 1] if position + 1 < len(starts) else end])
        for position, start in enumerate(starts)
    ]


def _step_name(step: str) -> str:
    return step.splitlines()[0].split(": ", 1)[1]


def _step_keys(step: str) -> set[str]:
    return {"name", *_keys(step, 8)}


def _run_command(step: str) -> str:
    lines = step.splitlines()
    for index, line in enumerate(lines):
        if line.startswith("        run:"):
            inline = line.split(":", 1)[1].strip()
            parts = [] if inline in BLOCK_MARKERS else [inline]
            parts.extend(value.strip() for value in lines[index + 1 :])
            return " ".join(part for part in parts if part)
    return ""


def _uses(step: str) -> str | None:
    values = re.findall(r"^        uses: (.+)$", step, re.MULTILINE)
    if len(values) > 1:
        raise WorkflowError("step repeats uses")
    return values[0] if values else None


def _if_expression(block: str) -> str:
    lines = block.splitlines()
    matches = [
        (index, line.split(":", 1)[1].strip())
        for index, line in enumerate(lines)
        if line.startswith("    if:")
    ]
    if not matches:
        return ""
    if len(matches) != 1:
        raise WorkflowError("job repeats if")
    index, value = matches[0]
    if value not in BLOCK_MARKERS:
        return value
    body = []
    for line in lines[index + 1 :]:
        if line.strip() and len(line) - len(line.lstrip(" ")) <= 4:
            break
        body.append(line.strip())
    return " ".join(body)


def _context_references(
    expression: str,
    *,
    job: str,
    jobs: dict[str, str],
    credential_job: str,
) -> int:
    token_references = 0
    for match in CONTEXT_ROOT.finditer(expression):
        root = match.group(1)
        if root != root.lower():
            raise WorkflowError("expression uses a noncanonical context root")
        properties: list[str] = []
        position = match.end()
        while property_match := CONTEXT_PROPERTY.match(expression, position):
            properties.append(property_match.group(1))
            position = property_match.end()
        reference = expression[match.start() : position]
        canonical = root + "".join(f".{value}" for value in properties)
        if reference != canonical or not properties or "*" in properties:
            raise WorkflowError("expression uses a root, wildcard, or spaced reference")
        if expression[position:].lstrip().startswith("["):
            raise WorkflowError("expression uses an indexed context reference")
        path = tuple(properties)
        if root == "secrets":
            raise WorkflowError("workflow accesses the secrets context")
        if root == "github":
            if path == ("token",):
                if job != credential_job or expression.strip() != "github.token":
                    raise WorkflowError("workflow leaks the GitHub token context")
                token_references += 1
            elif path not in {("sha",), ("event_name",), ("ref",)}:
                raise WorkflowError("workflow accesses an unsafe GitHub context")
        elif root == "runner" and path != ("temp",):
            raise WorkflowError("workflow accesses an unsafe runner context")
        elif root == "inputs" and (job != "publish" or path != ("publish",)):
            raise WorkflowError("workflow accesses an unsafe inputs context")
        elif root == "needs":
            declared = _needs(jobs[job]) if "    needs:" in jobs[job] else set()
            if (
                len(path) != 3
                or path[0] not in declared
                or path[1] != "outputs"
                or path[2] not in _mapping_values(
                    jobs[path[0]], "outputs", header_spaces=4
                )
            ):
                raise WorkflowError("workflow has an unbound needs output")
        elif root == "steps":
            ids = set(re.findall(r"^        id: ([A-Za-z0-9_-]+)$", jobs[job], re.MULTILINE))
            if len(path) != 3 or path[0] not in ids or path[1] != "outputs":
                raise WorkflowError("workflow has an unbound step output")
    return token_references


def _validate_expressions(
    text: str,
    jobs: dict[str, str],
    credential_job: str,
) -> None:
    token_references = 0
    header = text.split("jobs:", 1)[0]
    if "${{" in header or "}}" in header:
        raise WorkflowError("workflow header contains an expression")
    for name, block in jobs.items():
        starts = block.count("${{")
        ends = block.count("}}")
        matches = list(re.finditer(r"\$\{\{(.*?)\}\}", block, re.DOTALL))
        if starts != ends or len(matches) != starts:
            raise WorkflowError("workflow contains a malformed expression")
        for expression in matches:
            token_references += _context_references(
                expression.group(1).strip(),
                job=name,
                jobs=jobs,
                credential_job=credential_job,
            )
        implicit = _if_expression(block)
        if implicit:
            if "${{" in implicit or "}}" in implicit:
                raise WorkflowError("job if expression has redundant delimiters")
            token_references += _context_references(
                implicit,
                job=name,
                jobs=jobs,
                credential_job=credential_job,
            )
    if token_references != 1:
        raise WorkflowError("workflow must contain exactly one GitHub token reference")


def _validate_permissions(text: str) -> None:
    if len(re.findall(r"^permissions:", text, re.MULTILINE)) != 1:
        raise WorkflowError("workflow lacks one top-level permissions mapping")
    if re.search(r"^ +permissions:", text, re.MULTILINE):
        raise WorkflowError("workflow grants job- or step-level permissions")
    values = _mapping_values(text, "permissions", header_spaces=0)
    if values != {"actions": "read", "contents": "read"}:
        raise WorkflowError("workflow permissions are not read-only")


def _validate_actions(path: str, jobs: dict[str, str]) -> None:
    for name, block in jobs.items():
        steps = _steps(block)
        step_ids = re.findall(r"^        id: ([A-Za-z0-9_-]+)$", block, re.MULTILINE)
        if len(step_ids) != len(set(step_ids)):
            raise WorkflowError(f"{path} repeats a step id")
        for step in steps:
            action = _uses(step)
            if action is None:
                continue
            if PINNED_ACTION.fullmatch(action) is None:
                raise WorkflowError(f"{path} job {name} uses an unpinned action")
            if action.startswith("actions/checkout@"):
                values = _mapping_values(step, "with", header_spaces=8)
                if (
                    action != CHECKOUT
                    or values.get("persist-credentials") != "false"
                    or values.get("fetch-depth") != "0"
                ):
                    raise WorkflowError(f"{path} persists checkout credentials")


def _validate_runner_policy(
    path: str,
    jobs: dict[str, str],
    *,
    light_jobs: set[str],
) -> None:
    for name, block in jobs.items():
        expected = "homelab" if name in light_jobs else "homelab-heavy"
        if _field(block, "runs-on") != expected:
            raise WorkflowError(f"{path} job {name} violates the runner policy")


def _validate_credential_job(
    path: str,
    block: str,
    *,
    names: tuple[str, ...],
    token_command: str,
    expected_job_keys: set[str],
) -> None:
    steps = _steps(block)
    expected_step_keys = (
        {"name", "uses", "with"},
        {"name", "run"},
        {"env", "name", "run"},
        {"id", "name", "run"},
        {"name", "uses", "with"},
    )
    if (
        _keys(block, 4) != expected_job_keys
        or tuple(map(_step_name, steps)) != names
        or tuple(map(_step_keys, steps)) != expected_step_keys
        or _mapping_values(block, "outputs", header_spaces=4)
        != {"github_sha256": "${{ steps.github_digest.outputs.sha256 }}"}
        or [_uses(step) for step in steps] != [CHECKOUT, None, None, None, UPLOAD]
    ):
        raise WorkflowError(f"{path} credential job shape is not allowlisted")
    token_environment = _mapping_values(steps[2], "env", header_spaces=8)
    if token_environment != {
        "GH_TOKEN": "${{ github.token }}",
        "GREENLIT_BUILD_COMMIT": "${{ github.sha }}",
    } or _run_command(steps[2]) != token_command:
        raise WorkflowError(f"{path} lacks one exec-only credential step")
    credential_exports = re.findall(
        r"^\s+(" + "|".join(sorted(CREDENTIAL_KEYS)) + r"):", block, re.MULTILINE
    )
    if credential_exports != ["GH_TOKEN"]:
        raise WorkflowError(f"{path} exports an unapproved credential key")
    forbidden = (
        "actions/download-artifact@",
        "--greenlit-binary",
        "pack-local",
        "pack-prepared",
        "unpack-",
        "cargo ",
        "docker ",
        "/litci",
    )
    if any(value in block for value in forbidden):
        raise WorkflowError(f"{path} credential job can access a candidate")


def _validate_publish_job(block: str) -> None:
    steps = _steps(block)
    expected_names = (
        "Check out exact source without persisted credentials",
        "Resolve locked offline verification inputs",
        "Download verified release candidate",
        "Authenticate and statically verify candidate",
        "Refuse unverified repackaging",
    )
    expected_keys = (
        {"name", "uses", "with"},
        {"name", "run"},
        {"name", "uses", "with"},
        {"name", "run"},
        {"name", "run"},
    )
    commands = tuple(_run_command(step) for step in steps)
    if (
        _keys(block, 4)
        != {"environment", "if", "name", "needs", "runs-on", "steps"}
        or _field(block, "if") != "inputs.publish"
        or _field(block, "environment") != "release"
        or _needs(block) != {"finalize"}
        or tuple(map(_step_name, steps)) != expected_names
        or tuple(map(_step_keys, steps)) != expected_keys
        or [_uses(step) for step in steps] != [CHECKOUT, None, DOWNLOAD, None, None]
        or commands[1] != "cargo fetch --locked"
        or commands[3]
        != (
            'set -euo pipefail download="$RUNNER_TEMP/greenlit-candidate-download/'
            'greenlit-release-candidate.tar" output="$RUNNER_TEMP/'
            'greenlit-candidate-unpacked" test ! -e "$output" install -d -m 0700 '
            '"$output" tools/release-provenance unpack \\ --repository-root '
            '"$GITHUB_WORKSPACE" \\ --bundle "$download" \\ --output-root "$output" '
            '\\ --expected-source "$GITHUB_SHA" \\ --expected-sha256 '
            '"${{ needs.finalize.outputs.candidate_sha256 }}"'
        )
        or commands[4]
        != (
            'echo "Cargo cannot publish the verified prebuilt .crate files; '
            'refusing any registry write" >&2 exit 1'
        )
    ):
        raise WorkflowError("release publication boundary is not an exact refusal")


def _validate_case(
    path: str,
    text: str,
    *,
    expected_jobs: set[str],
    credential_name: str,
    local_name: str,
    final_name: str,
    first_needs: set[str],
    final_needs: set[str],
    names: tuple[str, ...],
    token_command: str,
    expected_job_keys: set[str],
    light_jobs: set[str],
) -> dict[str, str]:
    _yaml_authority(text)
    _validate_permissions(text)
    jobs = _job_blocks(text)
    if set(jobs) != expected_jobs:
        raise WorkflowError(f"{path} job set is not allowlisted")
    _validate_runner_policy(path, jobs, light_jobs=light_jobs)
    _validate_actions(path, jobs)
    _validate_expressions(text, jobs, credential_name)
    credential_exports = re.findall(
        r"^\s+(" + "|".join(sorted(CREDENTIAL_KEYS)) + r"):", text, re.MULTILINE
    )
    if credential_exports != ["GH_TOKEN"]:
        raise WorkflowError(
            f"{path} must export GH_TOKEN only in its credential job"
        )
    credential = jobs[credential_name]
    if (
        _needs(credential) != first_needs
        or _needs(jobs[local_name]) != first_needs
        or _needs(jobs[final_name]) != final_needs
    ):
        raise WorkflowError(f"{path} parity topology is not allowlisted")
    if "if" in expected_job_keys:
        expected_if = (
            "github.event_name == 'push' && "
            "(github.ref == 'refs/heads/main' || "
            "startsWith(github.ref, 'refs/heads/stabilization/'))"
        )
        if any(
            _if_expression(jobs[name]) != expected_if
            for name in (credential_name, local_name, final_name)
        ):
            raise WorkflowError(f"{path} parity trigger is not allowlisted")
    for name, block in jobs.items():
        declared = _needs(block) if "    needs:" in block else set()
        if not declared <= set(jobs):
            raise WorkflowError(f"{path} job {name} needs an unknown job")
    _validate_credential_job(
        path,
        credential,
        names=names,
        token_command=token_command,
        expected_job_keys=expected_job_keys,
    )
    return jobs


def validate_workflow_documents(ci_text: str, release_text: str) -> None:
    """Validate exact workflow topology, expressions, credentials, and refusal."""

    _validate_case(
        "ci.yml",
        ci_text,
        expected_jobs=CI_JOBS,
        credential_name="live_parity_github",
        local_name="live_parity_local",
        final_name="live_parity_compare",
        first_needs={"ci"},
        final_needs={"live_parity_local", "live_parity_github"},
        names=(
            "Check out exact source without persisted credentials",
            "Prepare credential-only boundary",
            "Collect GitHub evidence with no candidate present",
            "Seal GitHub evidence",
            "Upload exact GitHub evidence bundle",
        ),
        token_command=(
            'exec /usr/bin/python3 -E -s -B tools/check-live-parity github '
            '--repository-root "$GITHUB_WORKSPACE" '
            '--output-root "$RUNNER_TEMP/greenlit-live-github"'
        ),
        expected_job_keys={"if", "name", "needs", "outputs", "runs-on", "steps"},
        light_jobs={"live_parity_github"},
    )
    release_jobs = _validate_case(
        "release.yml",
        release_text,
        expected_jobs=RELEASE_JOBS,
        credential_name="github_parity",
        local_name="local_parity",
        final_name="finalize",
        first_needs={"prepare"},
        final_needs={"prepare", "local_parity", "github_parity"},
        names=(
            "Check out exact source without persisted credentials",
            "Prepare credential-only source boundary",
            "Collect GitHub evidence with no candidate present",
            "Seal exact GitHub evidence",
            "Upload exact GitHub evidence",
        ),
        token_command=(
            'exec /usr/bin/python3 -E -s -B tools/check-live-parity github '
            '--repository-root "$GITHUB_WORKSPACE" '
            '--output-root "$RUNNER_TEMP/greenlit-release-github-evidence"'
        ),
        expected_job_keys={"name", "needs", "outputs", "runs-on", "steps"},
        light_jobs={"github_parity", "publish"},
    )
    _validate_publish_job(release_jobs["publish"])


def validate_workflows(root: Path) -> None:
    """Validate the repository's CI and release workflow documents."""

    validate_workflow_documents(
        (root / ".github/workflows/ci.yml").read_text(encoding="utf-8"),
        (root / ".github/workflows/release.yml").read_text(encoding="utf-8"),
    )
