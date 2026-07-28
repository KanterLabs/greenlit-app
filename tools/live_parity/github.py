"""Official GitHub API run discovery and same-attempt certification."""

from __future__ import annotations

import json
import os
import stat
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from .contract import (
    MAX_API_BYTES,
    POLL_SECONDS,
    POLL_TIMEOUT_SECONDS,
    REPOSITORY_ID,
    WORKFLOW_NAME,
    WORKFLOW_PATH,
)
from .errors import GateError
from .process import run_command


GH_CANDIDATES = (Path("/usr/local/bin/gh"), Path("/usr/bin/gh"))
MAX_TOKEN_BYTES = 64 * 1024
TOKEN_DESCRIPTOR = "GREENLIT_GITHUB_CREDENTIAL_FD"
PRODUCER_TOKEN_DESCRIPTOR = "GREENLIT_GITHUB_PRODUCER_CREDENTIAL_FD"
MAX_SIGNED_INTEGER = (1 << 63) - 1


@dataclass(frozen=True)
class RunIdentity:
    """One completed canonical Actions run and immutable attempt number."""

    run_id: int
    attempt: int


@dataclass(frozen=True)
class GitHubCredential:
    """One in-memory token removed from the wrapper process environment."""

    token: str | None = field(repr=False)

    @classmethod
    def capture(cls) -> "GitHubCredential":
        descriptor_value = os.environ.pop(TOKEN_DESCRIPTOR, None)
        if descriptor_value is not None:
            if not descriptor_value.isdecimal():
                raise GateError("internal GitHub credential descriptor is invalid")
            descriptor = int(descriptor_value, 10)
            chunks: list[bytes] = []
            remaining = MAX_TOKEN_BYTES + 1
            try:
                os.set_inheritable(descriptor, False)
                while remaining > 0:
                    chunk = os.read(descriptor, min(4096, remaining))
                    if not chunk:
                        break
                    chunks.append(chunk)
                    remaining -= len(chunk)
            except OSError as error:
                raise GateError(
                    f"cannot read internal GitHub credential: {error}"
                ) from error
            finally:
                try:
                    os.close(descriptor)
                except OSError:
                    pass
            raw = b"".join(chunks)
            if not raw or len(raw) > MAX_TOKEN_BYTES:
                raise GateError("internal GitHub credential is empty or oversized")
            try:
                return cls(raw.decode("utf-8", errors="strict"))
            except UnicodeDecodeError as error:
                raise GateError("internal GitHub credential is not UTF-8") from error
        gh_token = os.environ.get("GH_TOKEN")
        github_token = os.environ.get("GITHUB_TOKEN")
        if gh_token is not None and github_token is not None and gh_token != github_token:
            raise GateError("GH_TOKEN and GITHUB_TOKEN disagree")
        token = gh_token if gh_token is not None else github_token
        return cls(token if token else None)

    def environment(self) -> dict[str, str]:
        if self.token is None:
            raise GateError("live parity requires GH_TOKEN or GITHUB_TOKEN")
        return {
            "HOME": "/",
            "LANG": "C.UTF-8",
            "LC_ALL": "C.UTF-8",
            "GH_TOKEN": self.token,
            "GH_PROMPT_DISABLED": "1",
            "NO_COLOR": "1",
        }

    def open_descriptor(self) -> int:
        if self.token is None:
            raise GateError("live parity requires GH_TOKEN or GITHUB_TOKEN")
        descriptor = os.memfd_create("greenlit-github-producer-credential", flags=0)
        raw = self.token.encode("utf-8")
        try:
            offset = 0
            while offset < len(raw):
                offset += os.write(descriptor, raw[offset:])
            os.lseek(descriptor, 0, os.SEEK_SET)
            os.set_inheritable(descriptor, False)
        except OSError:
            os.close(descriptor)
            raise
        return descriptor


def credential_free_environment() -> dict[str, str]:
    """Return inherited runtime settings with every GitHub credential removed."""

    environment = os.environ.copy()
    for key in (
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
        "GH_HOST",
        TOKEN_DESCRIPTOR,
        PRODUCER_TOKEN_DESCRIPTOR,
    ):
        environment.pop(key, None)
    environment["PYTHONDONTWRITEBYTECODE"] = "1"
    environment["GH_PROMPT_DISABLED"] = "1"
    return environment


def _strict_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise GateError(f"GitHub API response repeats key {key!r}")
        result[key] = value
    return result


def _bounded_integer(raw: str) -> int:
    digits = raw[1:] if raw.startswith("-") else raw
    if not digits or len(digits) > 19 or not digits.isdecimal():
        raise ValueError("GitHub API integer is malformed or oversized")
    value = int(raw, 10)
    if value < -MAX_SIGNED_INTEGER - 1 or value > MAX_SIGNED_INTEGER:
        raise ValueError("GitHub API integer exceeds signed 64-bit range")
    return value


def _gh_executable() -> str:
    for candidate in GH_CANDIDATES:
        try:
            metadata = candidate.lstat()
        except OSError:
            continue
        if (
            stat.S_ISREG(metadata.st_mode)
            and metadata.st_uid == 0
            and metadata.st_mode & 0o111
            and stat.S_IMODE(metadata.st_mode) & 0o022 == 0
        ):
            return str(candidate)
    raise GateError(
        "live parity requires a trusted GitHub CLI at "
        "/usr/local/bin/gh or /usr/bin/gh"
    )


def _api(
    credential: GitHubCredential,
    endpoint: str,
    fields: tuple[str, ...] = (),
    *,
    timeout: float = 120,
) -> Any:
    command = [
        _gh_executable(),
        "api",
        "--hostname",
        "github.com",
        "--method",
        "GET",
        endpoint,
    ]
    for field in fields:
        command.extend(("--raw-field", field))
    raw = run_command(
        command,
        cwd=Path("/"),
        timeout=timeout,
        environment=credential.environment(),
        capture_stdout=True,
        stdout_limit=MAX_API_BYTES,
    ).stdout
    try:
        return json.loads(
            raw.decode("utf-8", errors="strict"),
            object_pairs_hook=_strict_object,
            parse_int=_bounded_integer,
            parse_constant=lambda value: (_ for _ in ()).throw(
                GateError(f"GitHub API returned non-JSON constant {value!r}")
            ),
        )
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
        raise GateError(f"GitHub Actions response is invalid JSON: {error}") from error


def _positive_integer(value: Any, field: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < 1:
        raise GateError(f"canonical parity run has an invalid {field}")
    return value


def _canonical_run(value: Any, source_commit: str) -> tuple[RunIdentity, str, Any]:
    if not isinstance(value, dict):
        raise GateError("GitHub Actions run listing contains a non-object")
    repository = value.get("repository")
    if not isinstance(repository, dict) or repository.get("full_name") != REPOSITORY_ID:
        raise GateError("canonical parity run has the wrong repository identity")
    if (
        value.get("head_sha") != source_commit
        or value.get("event") != "push"
        or value.get("path") != WORKFLOW_PATH
        or value.get("name") != WORKFLOW_NAME
    ):
        raise GateError("canonical parity run identity fields do not match")
    identity = RunIdentity(
        _positive_integer(value.get("id"), "id"),
        _positive_integer(value.get("run_attempt"), "attempt"),
    )
    return identity, value.get("status"), value.get("conclusion")


def _listing(
    credential: GitHubCredential,
    source_commit: str,
    *,
    timeout: float = 120,
) -> list[dict[str, Any]]:
    workflow_file = WORKFLOW_PATH.rsplit("/", 1)[-1]
    document = _api(
        credential,
        f"repos/{REPOSITORY_ID}/actions/workflows/{workflow_file}/runs",
        (
            f"head_sha={source_commit}",
            "event=push",
            "per_page=100",
            "page=1",
        ),
        timeout=timeout,
    )
    required = {"total_count", "workflow_runs"}
    if not isinstance(document, dict) or not required <= set(document):
        raise GateError("GitHub Actions run listing omitted its result fields")
    runs = document["workflow_runs"]
    count = document["total_count"]
    if (
        not isinstance(runs, list)
        or not isinstance(count, int)
        or isinstance(count, bool)
        or count < 0
        or count != len(runs)
        or count > 100
    ):
        raise GateError("GitHub Actions run listing was truncated or malformed")
    matches: list[dict[str, Any]] = []
    for value in runs:
        if not isinstance(value, dict):
            raise GateError("GitHub Actions run listing contains a non-object")
        if (
            value.get("head_sha") == source_commit
            and value.get("event") == "push"
            and value.get("path") == WORKFLOW_PATH
            and value.get("name") == WORKFLOW_NAME
        ):
            matches.append(value)
    if len(matches) > 1:
        raise GateError(
            "more than one canonical parity-seed push run exists for this commit"
        )
    return matches


def _detail(
    credential: GitHubCredential,
    source_commit: str,
    identity: RunIdentity,
    *,
    timeout: float = 120,
) -> RunIdentity:
    value = _api(
        credential,
        f"repos/{REPOSITORY_ID}/actions/runs/{identity.run_id}",
        timeout=timeout,
    )
    observed, status, conclusion = _canonical_run(value, source_commit)
    if observed != identity:
        raise GateError("canonical parity run id or attempt changed")
    if status != "completed" or conclusion != "success":
        raise GateError("canonical parity-seed push run is not completed successfully")
    return observed


def certify_run(
    credential: GitHubCredential,
    source_commit: str,
    run_id: int,
) -> RunIdentity:
    """Require exactly one matching successful run and bind its attempt."""

    matches = _listing(credential, source_commit)
    if len(matches) != 1:
        raise GateError("canonical same-commit parity push run does not exist exactly once")
    identity, status, conclusion = _canonical_run(matches[0], source_commit)
    if identity.run_id != run_id:
        raise GateError("selected run id is not the unique canonical same-commit run")
    if status != "completed" or conclusion != "success":
        raise GateError("canonical parity-seed push run is not completed successfully")
    return _detail(credential, source_commit, identity)


def discover_run(
    credential: GitHubCredential,
    source_commit: str,
) -> RunIdentity:
    """Poll for one unique completed successful canonical push run."""

    deadline = time.monotonic() + POLL_TIMEOUT_SECONDS
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise GateError(
                "timed out waiting for the canonical same-commit parity push run"
            )
        matches = _listing(
            credential,
            source_commit,
            timeout=min(120, remaining),
        )
        if matches:
            identity, status, conclusion = _canonical_run(matches[0], source_commit)
            if status == "completed":
                if conclusion != "success":
                    raise GateError(
                        "canonical parity-seed push run completed without success"
                    )
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise GateError(
                        "timed out validating the canonical same-commit parity push run"
                    )
                return _detail(
                    credential,
                    source_commit,
                    identity,
                    timeout=min(120, remaining),
                )
            if status not in {
                "queued",
                "in_progress",
                "pending",
                "requested",
                "waiting",
            }:
                raise GateError(
                    f"canonical parity-seed push run has unknown status {status!r}"
                )
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise GateError(
                "timed out waiting for the canonical same-commit parity push run"
            )
        time.sleep(min(POLL_SECONDS, remaining))
