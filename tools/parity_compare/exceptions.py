"""Strict parsing and authority rules for Greenlit parity exceptions."""
from __future__ import annotations
import datetime as dt
import re
from dataclasses import dataclass
from pathlib import Path

from .exception_paths import (
    ExceptionContractError,
    record_scope as _record_scope,
    validate_exact_path,
)
from .text import is_safe_markdown_text, read_commonmark_lines

HEADER = (
    "Exception ID", "Case ID", "Source commit", "Exact field",
    "Authoritative source", "Reason and scope", "Owner approval",
    "Removal criterion", "Status")
PLACEHOLDER = "—"
TITLE = "# Greenlit parity-exception ledger"
_EXCEPTION_ID = re.compile(r"^GL-PARITY-(?P<number>[0-9]{3})$")
_CASE_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
_COMMIT = re.compile(r"^[0-9a-f]{40}$")
_APPROVAL = re.compile(r"^Shane (?P<date>[0-9]{4}-[0-9]{2}-[0-9]{2})$")
_REPOSITORY = r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+"
_REPOSITORY_ID = re.compile(rf"^{_REPOSITORY}$")
_RUN_SOURCE = re.compile(
    rf"^https://github\.com/(?P<repository>{_REPOSITORY})/"
    r"actions/runs/[1-9][0-9]*$"
)
_REASON = re.compile(
    r"^(?P<kind>explicit non-goal|specification-permitted degradation) "
    r"\(greenlit-v0-spec\.md#(?P<anchor>[A-Za-z0-9._:-]+)\): "
    r"(?P<detail>.+)$"
)
_DEGRADATION_ANCHORS = {
    "compatibility-and-result-truth", "content-and-environment-preparation",
}
_REMOVAL = re.compile(r"^remove when (?P<detail>.+)$")
_FORBIDDEN_REASON = re.compile(
    r"\b(?:in[- ]scope|bugs?|defect(?:s|ive)?|regressions?|broken|"
    r"unimplemented|pending fix|incorrect|wrong|repairs?|"
    r"needs? implementation|failure to match)\b",
    re.IGNORECASE,
)
_FORBIDDEN_REMOVAL = re.compile(
    r"\b(?:never|permanent|n/?a|tbd|unknown|later|eventually|whenever|"
    r"owner discretion|if desired|if practical|if possible|as appropriate|"
    r"when convenient|when the owner decides|when the owner agrees|maybe)\b",
    re.IGNORECASE,
)
_FINITE_SUBJECT = re.compile(
    r"\b(?:GitHub|Greenlit|upstream|specification|spec|scope|contract|"
    r"documented behavior|release|version|field|non-goal|degradation)\b",
    re.IGNORECASE,
)
_FINITE_EVENT = re.compile(
    r"\b(?:becomes?|changes?|enters?|leaves?|removes?|adds?|supports?|"
    r"exposes?|documents?|publishes?|ships?|matches?|equals?|ceases?|"
    r"adopts?|requires?|includes?|reclassifies?|is removed|is adopted|"
    r"are equal|are equivalent)\b",
    re.IGNORECASE,
)
ContractError = ExceptionContractError
@dataclass(frozen=True)
class ExceptionRow:
    """One immutable, fully retained parity-exception row."""
    exception_id: str
    case_id: str
    source_commit: str
    exact_field: str
    authoritative_source: str
    reason_and_scope: str
    owner_approval: str
    approval_date: dt.date
    removal_criterion: str
    status: str
@dataclass(frozen=True)
class ExceptionLedger:
    """Validated permanent history and its active exact-key lookup."""
    rows: tuple[ExceptionRow, ...]
    active: dict[tuple[str, str, str], ExceptionRow]
def _split_row(line: str, _label: str) -> tuple[str, ...] | None:
    if line != line.lstrip():
        return None
    stripped = line.strip()
    if not stripped.startswith("|") or not stripped.endswith("|"):
        return None
    cells: list[str] = []
    current: list[str] = []
    content = stripped[1:-1]
    index = 0
    while index < len(content):
        character = content[index]
        if (
            character == "\\"
            and index + 1 < len(content)
            and content[index + 1] in {"\\", "|"}
        ):
            current.append(content[index + 1])
            index += 2
            continue
        if character == "|":
            cells.append("".join(current).strip())
            current.clear()
        else:
            current.append(character)
        index += 1
    cells.append("".join(current).strip())
    return tuple(cells)
def _substantive(value: str) -> bool:
    return (
        24 <= len(value) <= 1_024
        and len(re.findall(r"[A-Za-z0-9]+", value)) >= 5
        and is_safe_markdown_text(value)
    )
def _validate_authority(
    value: str,
    _case_id: str,
    source_commit: str,
    label: str,
    repository_id: str | None,
) -> None:
    evidence, separator, binding = value.rpartition("; source-commit=")
    if not separator or _COMMIT.fullmatch(binding) is None:
        raise ExceptionContractError(
            f"{label}: Authoritative source must end with "
            "'; source-commit=<full lowercase commit>'"
        )
    if binding != source_commit:
        raise ExceptionContractError(
            f"{label}: Authoritative source binding must equal Source commit"
        )
    run = _RUN_SOURCE.fullmatch(evidence)
    if run is None:
        raise ExceptionContractError(
            f"{label}: Authoritative source must be the exact validated "
            "GitHub Actions run URL"
        )
    if repository_id is not None:
        if run.group("repository") != repository_id:
            raise ExceptionContractError(
                f"{label}: Actions run authority names another repository"
            )
def _parse_approval(value: str, label: str) -> dt.date:
    match = _APPROVAL.fullmatch(value)
    if match is None:
        raise ExceptionContractError(
            f"{label}: Owner approval must be 'Shane YYYY-MM-DD'"
        )
    try:
        approval_date = dt.date.fromisoformat(match.group("date"))
    except ValueError as error:
        raise ExceptionContractError(f"{label}: invalid owner approval date") from error
    if approval_date > dt.datetime.now(dt.timezone.utc).date():
        raise ExceptionContractError(
            f"{label}: Owner approval date cannot be in the future"
        )
    return approval_date
def _validate_reason(value: str, label: str) -> None:
    match = _REASON.fullmatch(value)
    if (
        match is None
        or not _substantive(match.group("detail"))
        or _FORBIDDEN_REASON.search(value) is not None
        or (
            match.group("kind") == "explicit non-goal"
            and match.group("anchor") != "explicit-non-goals"
        )
        or (
            match.group("kind") == "specification-permitted degradation"
            and match.group("anchor") not in _DEGRADATION_ANCHORS
        )
    ):
        raise ExceptionContractError(
            f"{label}: Reason and scope must be a substantive explicit "
            "non-goal or specification-permitted degradation with its exact "
            "permitted v0 spec anchor, never an in-scope defect or regression"
        )
def _validate_removal(value: str, label: str) -> None:
    match = _REMOVAL.fullmatch(value)
    detail = match.group("detail") if match is not None else ""
    if (
        match is None
        or not _substantive(detail)
        or _FORBIDDEN_REMOVAL.search(value) is not None
        or _FINITE_SUBJECT.search(detail) is None
        or _FINITE_EVENT.search(detail) is None
    ):
        raise ExceptionContractError(
            f"{label}: Removal criterion must be 'remove when' plus a "
            "substantive finite condition"
        )
def load_exception_ledger(
    path: Path, *, repository_id: str | None = None
) -> ExceptionLedger:
    """Parse and validate the canonical permanent exception ledger."""
    if repository_id is not None and _REPOSITORY_ID.fullmatch(repository_id) is None:
        raise ExceptionContractError(
            "trusted repository must be a canonical OWNER/REPO identifier"
        )
    try:
        lines = read_commonmark_lines(path)
    except FileNotFoundError as error:
        raise ExceptionContractError(
            f"parity exception ledger is missing: {path}"
        ) from error
    except (OSError, UnicodeError, ValueError) as error:
        raise ExceptionContractError(
            f"cannot read parity exception ledger {path}: {error}"
        ) from error
    if not lines or lines[0] != TITLE:
        raise ExceptionContractError(
            f"parity exception ledger must begin with exact title {TITLE!r}"
        )
    headers = [
        index for index, line in enumerate(lines) if _split_row(line, str(path)) == HEADER
    ]
    if len(headers) != 1:
        raise ExceptionContractError(
            f"parity exception ledger must contain exactly one canonical header: {path}"
        )
    header_index = headers[0]
    prefix = lines[1:header_index]
    if any(
        "<!--" in line
        or "-->" in line
        or "<" in line
        or ">" in line
        or line.lstrip().startswith(("```", "~~~", "#", "|"))
        for line in prefix
    ):
        raise ExceptionContractError(
            "parity exception table must be a visible top-level Markdown table"
        )
    if header_index == 0 or lines[header_index - 1].strip():
        raise ExceptionContractError(
            "parity exception table header must be preceded by a blank "
            "CommonMark line"
        )
    if header_index + 1 >= len(lines):
        raise ExceptionContractError("parity exception ledger is missing its delimiter row")
    delimiter = _split_row(lines[header_index + 1], str(path))
    if delimiter is None or len(delimiter) != len(HEADER) or any(
        re.fullmatch(r":?-{3,}:?", cell) is None for cell in delimiter
    ):
        raise ExceptionContractError("parity exception ledger has an invalid delimiter row")
    rows: list[ExceptionRow] = []
    seen_ids: set[str] = set()
    active: dict[tuple[str, str, str], ExceptionRow] = {}
    active_scopes: dict[tuple[str, str, str], str] = {}
    placeholder_count = 0
    table_ended = False
    for line_index in range(header_index + 2, len(lines)):
        cells = _split_row(lines[line_index], f"{path}:{line_index + 1}")
        if cells is None:
            stripped = lines[line_index].strip()
            if not stripped:
                table_ended = True
                continue
            if stripped.startswith("#"):
                break
            raise ExceptionContractError(
                f"{path}:{line_index + 1}: malformed parity exception table row"
            )
        if table_ended:
            raise ExceptionContractError(
                f"{path}:{line_index + 1}: parity exception rows must form "
                "one contiguous visible Markdown table"
            )
        label = f"parity exception ledger row {line_index + 1}"
        if len(cells) != len(HEADER):
            raise ExceptionContractError(f"{label}: expected {len(HEADER)} columns")
        if all(cell == PLACEHOLDER for cell in cells):
            placeholder_count += 1
            continue
        (
            exception_id,
            case_id,
            source_commit,
            exact_field,
            authority,
            reason,
            approval,
            removal,
            status,
        ) = cells
        match = _EXCEPTION_ID.fullmatch(exception_id)
        if match is None or int(match.group("number")) == 0:
            raise ExceptionContractError(f"{label}: invalid Exception ID")
        if exception_id in seen_ids:
            raise ExceptionContractError(f"{label}: duplicate Exception ID {exception_id}")
        seen_ids.add(exception_id)
        if _CASE_ID.fullmatch(case_id) is None:
            raise ExceptionContractError(f"{label}: invalid Case ID")
        if _COMMIT.fullmatch(source_commit) is None:
            raise ExceptionContractError(
                f"{label}: Source commit must be a full lowercase 40-character commit"
            )
        validate_exact_path(exact_field, label)
        _validate_authority(
            authority, case_id, source_commit, label, repository_id
        )
        _validate_reason(reason, label)
        approval_date = _parse_approval(approval, label)
        _validate_removal(removal, label)
        if status not in {"active", "closed"}:
            raise ExceptionContractError(f"{label}: Status must be active or closed")
        row = ExceptionRow(
            exception_id,
            case_id,
            source_commit,
            exact_field,
            authority,
            reason,
            approval,
            approval_date,
            removal,
            status,
        )
        rows.append(row)
        if status == "active":
            key = (case_id, source_commit, exact_field)
            if key in active:
                raise ExceptionContractError(
                    f"{label}: duplicate active case/source/field exception"
                )
            scope_key = (case_id, source_commit, _record_scope(exact_field))
            previous = active_scopes.get(scope_key)
            if previous is not None:
                raise ExceptionContractError(
                    f"{label}: active exceptions {previous} and {exception_id} "
                    "target multiple leaves in one semantic record"
                )
            active_scopes[scope_key] = exception_id
            active[key] = row
    if placeholder_count > 1:
        raise ExceptionContractError(
            f"{path}: parity ledger has more than one placeholder row"
        )
    if placeholder_count and rows:
        raise ExceptionContractError(
            f"{path}: remove the all-{PLACEHOLDER} placeholder once real rows exist"
        )
    return ExceptionLedger(tuple(rows), active)
def load_exceptions(
    path: Path, *, repository_id: str | None = None
) -> dict[tuple[str, str, str], ExceptionRow]:
    """Return a copy of the validated ledger's active exact-key lookup."""
    return dict(
        load_exception_ledger(path, repository_id=repository_id).active
    )
