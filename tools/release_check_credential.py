#!/usr/bin/python3 -I
"""Public isolation canaries for split release and CI parity workflows."""

from __future__ import annotations

import hashlib
import os
import subprocess
import sys
import tempfile

if not sys.flags.isolated:
    os.execve(
        "/usr/bin/python3",
        ["/usr/bin/python3", "-I", "-B", __file__, *sys.argv[1:]],
        os.environ,
    )
sys.dont_write_bytecode = True
sys.pycache_prefix = "/proc/self/fd/greenlit-impossible-pycache"

from pathlib import Path

_ENTRYPOINT = Path(__file__).absolute()
if _ENTRYPOINT.is_symlink():
    print("release credential canary failed: launcher must not be a symbolic link", file=sys.stderr)
    raise SystemExit(1)
sys.path[:0] = [str(_ENTRYPOINT.parent)]

from release_check_credential_workflow import (
    WorkflowError,
    validate_workflow_documents,
    validate_workflows,
)


BASH = "/usr/bin/bash"
PYTHON = "/usr/bin/python3"
CANARY_TOKEN = "greenlit-live-production-credential-canary"


class CanaryError(Exception):
    """A fail-closed isolation canary error."""


def _sanitized_environment() -> dict[str, str]:
    environment = os.environ.copy()
    for key in (
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
        "GH_HOST",
        "BASH_ENV",
        "ENV",
        "SHELLOPTS",
        "BASHOPTS",
        "BASH_XTRACEFD",
        "PS4",
        "CDPATH",
        "GLOBIGNORE",
        "LD_PRELOAD",
        "LD_AUDIT",
        "PYTHONPATH",
        "PYTHONHOME",
        "PYTHONINSPECT",
        "PYTHONSTARTUP",
    ):
        environment.pop(key, None)
    for key in tuple(environment):
        if key.startswith("BASH_FUNC_"):
            environment.pop(key, None)
    environment["PYTHONDONTWRITEBYTECODE"] = "1"
    return environment


def _repository() -> tuple[Path, str]:
    root = Path(__file__).resolve().parent.parent
    result = subprocess.run(
        [
            "/usr/bin/git",
            "-c",
            "credential.helper=",
            "-c",
            f"safe.directory={root}",
            "rev-parse",
            "--verify",
            "HEAD",
        ],
        cwd=root,
        env=_sanitized_environment(),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=10,
        check=False,
    )
    commit = result.stdout.decode("ascii", errors="strict").strip()
    if result.returncode != 0 or len(commit) != 40:
        raise CanaryError("cannot establish canary source commit")
    return root, commit


def _boundary_command(root: Path) -> list[str]:
    launcher = root / "tools" / "check-live-parity"
    digest = hashlib.sha256(launcher.read_bytes()).hexdigest()
    return [
        BASH,
        "--noprofile",
        "--norc",
        "-c",
        (
            f'exec {PYTHON} -I -B "$1" credential-canary '
            '--repository-root "$2" --expected-launcher-sha256 "$3"'
        ),
        "live-credential-canary",
        str(launcher),
        str(root),
        digest,
    ]


def _run_boundary(
    root: Path,
    commit: str,
    *,
    pass_fds: tuple[int, ...] = (),
) -> subprocess.CompletedProcess[bytes]:
    environment = _sanitized_environment()
    environment["GH_TOKEN"] = CANARY_TOKEN
    environment["GREENLIT_BUILD_COMMIT"] = commit
    return subprocess.run(
        _boundary_command(root),
        cwd=root,
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        pass_fds=pass_fds,
        timeout=15,
        check=False,
    )


def _preexisting_child_case(root: Path, commit: str) -> None:
    environment = _sanitized_environment()
    environment["GH_TOKEN"] = CANARY_TOKEN
    environment["GREENLIT_BUILD_COMMIT"] = commit
    child_code = """
import os
import sys
import time

child = os.fork()
if child == 0:
    time.sleep(0.2)
    os._exit(0)
os.execve(
    "/usr/bin/python3",
    [
        "/usr/bin/python3",
        "-I",
        "-B",
        sys.argv[1],
        "credential-canary",
        "--repository-root",
        sys.argv[2],
        "--expected-launcher-sha256",
        sys.argv[3],
    ],
    os.environ,
)
"""
    result = subprocess.run(
        [
            PYTHON,
            "-I",
            "-B",
            "-c",
            child_code,
            str(root / "tools/check-live-parity"),
            str(root),
            hashlib.sha256((root / "tools/check-live-parity").read_bytes()).hexdigest(),
        ],
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=15,
        check=False,
    )
    if (
        result.returncode == 0
        or b"preexisting child" not in result.stderr
        or CANARY_TOKEN.encode() in result.stdout + result.stderr
    ):
        raise CanaryError("production launcher accepted a preexisting child")


def _symlink_case(root: Path, commit: str) -> None:
    launcher = root / "tools" / "check-live-parity"
    with tempfile.TemporaryDirectory(prefix="greenlit-live-symlink.") as directory:
        symlink = Path(directory) / "check-live-parity"
        symlink.symlink_to(launcher)
        environment = _sanitized_environment()
        environment["GH_TOKEN"] = CANARY_TOKEN
        environment["GREENLIT_BUILD_COMMIT"] = commit
        result = subprocess.run(
            [
                PYTHON,
                "-I",
                "-B",
                str(symlink),
                "credential-canary",
                "--repository-root",
                str(root),
                "--expected-launcher-sha256",
                hashlib.sha256(launcher.read_bytes()).hexdigest(),
            ],
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=15,
            check=False,
        )
    if result.returncode == 0 or b"symbolic link" not in result.stderr:
        raise CanaryError("production launcher accepted a symbolic-link recipient")


def _replace_once(text: str, old: str, new: str, label: str) -> str:
    if text.count(old) != 1:
        raise CanaryError(f"cannot construct workflow mutation {label}")
    return text.replace(old, new)


def _replace_first_after(
    text: str,
    marker: str,
    old: str,
    new: str,
    label: str,
) -> str:
    if text.count(marker) != 1:
        raise CanaryError(f"cannot locate workflow mutation section {label}")
    prefix, suffix = text.split(marker)
    if old not in suffix:
        raise CanaryError(f"cannot construct workflow mutation {label}")
    return prefix + marker + suffix.replace(old, new, 1)


def _workflow_mutation_cases(root: Path) -> int:
    ci = (root / ".github/workflows/ci.yml").read_text(encoding="utf-8")
    release = (root / ".github/workflows/release.yml").read_text(encoding="utf-8")
    credential_header = (
        "  live_parity_github:\n"
        "    name: credential-only same-SHA GitHub observation\n"
    )
    credential_checkout = (
        "      - name: Check out exact source without persisted credentials\n"
        "        uses: actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683\n"
        "        with:\n"
        "          fetch-depth: 0\n"
        "          persist-credentials: false\n"
    )
    token_environment = (
        "        env:\n"
        "          GH_TOKEN: ${{ github.token }}\n"
        "          GREENLIT_BUILD_COMMIT: ${{ github.sha }}\n"
    )
    local_environment = (
        "      - name: Build exact release binary outside the checkout\n"
        "        env:\n"
        "          GREENLIT_BUILD_COMMIT: ${{ github.sha }}\n"
    )
    collect_step = (
        "      - name: Collect GitHub evidence with no candidate present\n"
        "        env:\n"
    )
    upload_action = (
        "      - name: Upload exact GitHub evidence bundle\n"
        "        uses: actions/upload-artifact@"
        "ea165f8d65b6e75b540449e92b4886f43607fa02\n"
    )
    mutations = [
        (
            "quoted YAML key",
            _replace_once(ci, "permissions:\n", '"permissions":\n', "quoted key"),
            release,
        ),
        (
            "YAML merge key",
            _replace_once(
                ci,
                credential_header,
                credential_header + "    <<: *credential_defaults\n",
                "merge key",
            ),
            release,
        ),
        (
            "repeated YAML key",
            _replace_once(
                ci,
                "  actions: read\n  contents: read\n",
                "  actions: read\n  contents: read\n  contents: read\n",
                "repeated key",
            ),
            release,
        ),
        (
            "YAML anchor",
            _replace_once(
                ci,
                "env:\n  CARGO_TERM_COLOR: always\n",
                "env: &shared_environment\n  CARGO_TERM_COLOR: always\n",
                "anchor",
            ),
            release,
        ),
        (
            "workflow permission escalation",
            _replace_once(ci, "  contents: read\n", "  contents: write\n", "write"),
            release,
        ),
        (
            "job permission escalation",
            _replace_once(
                ci,
                credential_header,
                credential_header + "    permissions:\n      contents: write\n",
                "job permissions",
            ),
            release,
        ),
        (
            "spaced GitHub token reference",
            _replace_once(
                ci,
                "${{ github.token }}",
                "${{ github . token }}",
                "spaced token",
            ),
            release,
        ),
        (
            "quoted GitHub token mapping",
            _replace_once(
                ci,
                "          GH_TOKEN: ${{ github.token }}\n",
                '          GH_TOKEN: "${{ github.token }}"\n',
                "quoted token",
            ),
            release,
        ),
        (
            "secrets-root serialization",
            _replace_once(
                ci,
                local_environment,
                local_environment.replace(
                    "${{ github.sha }}", "${{ toJSON(secrets) }}"
                ),
                "secrets root",
            ),
            release,
        ),
        (
            "GitHub wildcard serialization",
            _replace_once(
                ci,
                local_environment,
                local_environment.replace(
                    "${{ github.sha }}", "${{ join(github.*, ',') }}"
                ),
                "GitHub wildcard",
            ),
            release,
        ),
        (
            "GitHub-root serialization",
            _replace_once(
                ci,
                local_environment,
                local_environment.replace(
                    "${{ github.sha }}", "${{ toJSON(github) }}"
                ),
                "GitHub root",
            ),
            release,
        ),
        (
            "candidate GitHub token access",
            _replace_once(
                ci,
                local_environment,
                local_environment.replace(
                    "${{ github.sha }}", "${{ github.token }}"
                ),
                "candidate token",
            ),
            release,
        ),
        (
            "candidate credential export",
            _replace_once(
                ci,
                local_environment,
                local_environment + "          GITHUB_TOKEN: attacker-controlled\n",
                "candidate credential export",
            ),
            release,
        ),
        (
            "indexed GitHub token access",
            _replace_once(
                ci,
                local_environment,
                local_environment.replace(
                    "${{ github.sha }}", "${{ github['token'] }}"
                ),
                "indexed token",
            ),
            release,
        ),
        (
            "extra credential step",
            _replace_once(
                ci,
                collect_step,
                "      - name: Inspect candidate\n"
                "        run: target/release/litci --version\n\n"
                + collect_step,
                "extra step",
            ),
            release,
        ),
        (
            "unnamed credential step",
            _replace_once(
                ci,
                "      - name: Collect GitHub evidence with no candidate present\n",
                "      - run: echo unnamed\n",
                "unnamed step",
            ),
            release,
        ),
        (
            "repeated step key",
            _replace_once(
                ci,
                token_environment,
                token_environment + "        run: echo repeated\n",
                "repeated step key",
            ),
            release,
        ),
        (
            "unpinned action",
            _replace_once(
                ci,
                upload_action,
                upload_action.replace(
                    "ea165f8d65b6e75b540449e92b4886f43607fa02", "v4"
                ),
                "unpinned action",
            ),
            release,
        ),
        (
            "persisted checkout credential",
            _replace_first_after(
                ci,
                credential_header,
                credential_checkout,
                credential_checkout.replace(
                    "persist-credentials: false", "persist-credentials: true"
                ),
                "checkout persistence",
            ),
            release,
        ),
        (
            "publication command",
            ci,
            _replace_once(
                release,
                "          exit 1\n",
                "          cargo publish --workspace\n          exit 1\n",
                "publication",
            ),
        ),
        (
            "successful publication boundary",
            ci,
            _replace_once(
                release,
                "          exit 1\n",
                "          exit 0\n",
                "successful publication boundary",
            ),
        ),
        (
            "extra publication step",
            ci,
            _replace_once(
                release,
                "      - name: Refuse unverified repackaging\n",
                "      - name: Publish candidate\n"
                "        run: cargo publish --workspace\n\n"
                "      - name: Refuse unverified repackaging\n",
                "extra publication step",
            ),
        ),
    ]
    validate_workflow_documents(ci, release)
    for label, mutated_ci, mutated_release in mutations:
        try:
            validate_workflow_documents(mutated_ci, mutated_release)
        except WorkflowError:
            continue
        raise CanaryError(f"workflow authority accepted mutation: {label}")
    return len(mutations)


def _self_test() -> None:
    root, commit = _repository()
    expected = b"live parity production credential boundary canary passed\n"
    ordinary = _run_boundary(root, commit)
    if ordinary.returncode != 0 or ordinary.stdout != expected or ordinary.stderr:
        raise CanaryError("production credential boundary canary did not pass exactly")
    hostile = os.memfd_create("hostile-inherited-token", flags=0)
    try:
        os.write(hostile, CANARY_TOKEN.encode())
        os.set_inheritable(hostile, True)
        inherited = _run_boundary(root, commit, pass_fds=(hostile,))
    finally:
        os.close(hostile)
    if inherited.returncode != 0 or inherited.stdout != expected or inherited.stderr:
        raise CanaryError("production launcher retained a hostile inherited descriptor")
    _preexisting_child_case(root, commit)
    _symlink_case(root, commit)
    validate_workflows(root)
    mutation_count = _workflow_mutation_cases(root)
    print(
        "release/CI isolation self-test passed: production credential boundary, "
        "hostile FD, recipient, checkout, split-job, and "
        f"{mutation_count} workflow mutation canaries"
    )


def main(arguments: list[str]) -> int:
    """Run the shared public workflow and production-launcher canaries."""

    try:
        if arguments != ["--self-test"]:
            raise CanaryError("usage: tools/release_check_credential.py --self-test")
        _self_test()
    except (
        CanaryError,
        WorkflowError,
        OSError,
        subprocess.TimeoutExpired,
        UnicodeError,
    ) as error:
        print(f"release/CI isolation self-test failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
