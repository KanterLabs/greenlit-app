"""Checker-owned exact identities for capability-governing workflows."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
from typing import Any

from capability_yaml import WorkflowStep
from cargo_test_manifest import GateError
from test_authority.noncargo_fs import SourceTree


REQUIRED_ROUTE_POLICY_SHA256 = (
    "c48e7717e38eceb4a9f399bad358a37dd4f49a98addf3267113a7d4cdd451dbf"
)
# These are explicit reviewed authority baselines. There is intentionally no
# auto-update path: any workflow byte change requires a code-reviewed rebind.
REQUIRED_WORKFLOW_POLICY_SHA256 = {
    ".github/workflows/ci.yml": (
        "e3626be1608be2242b23c7167bfc9f7be2d323c42ec8d094f46f2a03d9047e97"
    ),
    ".github/workflows/release.yml": (
        "d63af41d693fcb2d3d4c6a500e704c0717749aa52b1402ecd1df82309384e185"
    ),
}
REQUIRED_STEP_POLICY = {
    (".github/workflows/ci.yml", "credential-capability"): (
        ("Check out repository", "uses", "a46f6f0525346f070ee0a4545858a5d64b7a22551556dde99a5b12759c31ba1d"),
        ("Provision pinned Rust toolchain", "run", "3cd2e27207eedcadb97bf9e08c3ae967b8bdae43dcf621d75c1e0b54fba57225"),
        ("Cache cargo registry and build artifacts", "uses", "01cd0291858fd7218948c5dc72b333364b7223ca577a805debfcce5f8bb4245b"),
        ("Provision isolated keyring prerequisites", "run", "aa885c0d983114fb49cc36d8f5f991763c9a3177ebeb5feded0162d101760ca0"),
        ("Run production persistent-keyring acceptance", "run", "106a16c28defc247eaeb29851cdea75186b2df18745f695a0e707b5a9b0e1411"),
    ),
    (".github/workflows/ci.yml", "dogfood"): (
        ("Check out repository", "uses", "dae4d90a8021944337a0c285f3cdd739641d2c54ce4994c2b93fc5c2bba0bae7"),
        ("Prepare branch-backed source identity", "run", "e1f9a3be96d198d2154398743873eb4d6ce0a02a6402251d9f1a92cf2115173c"),
        ("Provision pinned Rust toolchain", "run", "3cd2e27207eedcadb97bf9e08c3ae967b8bdae43dcf621d75c1e0b54fba57225"),
        ("Cache cargo registry and build artifacts", "uses", "01cd0291858fd7218948c5dc72b333364b7223ca577a805debfcce5f8bb4245b"),
        ("Build litci", "run", "4ad3edc72b3d9b6e18d31b13fb805bf4c03ecce2bd5ebc51e3b2a2ee5a5e6bba"),
        ("Run and verify repository gates under Greenlit twice", "run", "9fd7ef79ce457b85a60c8f05a4ef1e61e529d1e0bd96246fa2a9e54a2eb58d8e"),
    ),
    (".github/workflows/ci.yml", "host-deep-path"): (
        ("Check out repository", "uses", "a46f6f0525346f070ee0a4545858a5d64b7a22551556dde99a5b12759c31ba1d"),
        ("Provision pinned Rust toolchain", "run", "3cd2e27207eedcadb97bf9e08c3ae967b8bdae43dcf621d75c1e0b54fba57225"),
        ("Cache cargo registry and build artifacts", "uses", "01cd0291858fd7218948c5dc72b333364b7223ca577a805debfcce5f8bb4245b"),
        ("Require native writable tmpfs", "run", "6266f9c045b947672dc3155b654099fa9acbaaeae54da4d7b6d98495ddbff742"),
        ("Run manifest-owned beyond-PATH_MAX invariant", "run", "4e219ce1707e315c9d8e49fbc2766e45550dc9d18d32713f9ee26f0179e266f4"),
    ),
    (".github/workflows/ci.yml", "live_parity_compare"): (
        ("Check out exact source without persisted credentials", "uses", "9a191443447054d3de9528d06cd6054992d0e39ff3e5c93df440230ab6880675"),
        ("Prepare exact comparison source", "run", "f20a5d03182663678f36f54aea37da0d283e53c4f38209e06c1ff8ef3b13d7dc"),
        ("Download local evidence", "uses", "c1d9d50816e5a2b31af9e3b3ba775332a214953d85c2f0df9aadde1f903369e9"),
        ("Download GitHub evidence", "uses", "2d5bf0df8b158a8aa9c00f7daf81c437eb37d1cbdf1c46c43935ea6257b22a8e"),
        ("Verify, merge, and compare exact evidence", "run", "f721342c6b22955b82f45a62293ae8b152f97743ec18e44e50501c70e98b67a8"),
        ("Upload exact compared live parity evidence", "uses", "34a84dd700bc4e2681239445348ee52e5eb8e549b9f3d6149107830913911da3"),
    ),
    (".github/workflows/ci.yml", "live_parity_github"): (
        ("Check out exact source without persisted credentials", "uses", "9a191443447054d3de9528d06cd6054992d0e39ff3e5c93df440230ab6880675"),
        ("Prepare credential-only boundary", "run", "6aa3747bbfac1be4fdff2cbb35e41ccec71122e2254efa4024b91100415e595e"),
        ("Collect GitHub evidence with no candidate present", "run", "cf25a1816baf1f8ba39032a6e691f64d2f0e3d90353f5375abca327a7fc750ef"),
        ("Seal GitHub evidence", "run", "eee71f9a06f1f2a5eb0e73ddb9ba853ebfe572e85b35b3bafc953d4c973a18d1"),
        ("Upload exact GitHub evidence bundle", "uses", "ac97569975a12b497314492107a2da26fa4010fd65a91716ef95254559307e0c"),
    ),
    (".github/workflows/ci.yml", "live_parity_local"): (
        ("Check out exact source without persisted credentials", "uses", "9a191443447054d3de9528d06cd6054992d0e39ff3e5c93df440230ab6880675"),
        ("Prepare branch-backed source identity", "run", "3b3182cf1de0ca26e21966d78a2d620830bc473807817be4d8b018dbd88a4808"),
        ("Provision pinned Rust toolchain", "run", "3cd2e27207eedcadb97bf9e08c3ae967b8bdae43dcf621d75c1e0b54fba57225"),
        ("Require live parity capabilities", "run", "dd87c9bef74d24dd1efe42cc121841c8ab9a9a596522fbcee9e5ec85442dac7b"),
        ("Build exact release binary outside the checkout", "run", "37d6019e91d0536bedaecebc0f5b7904129230e618c0272cccdc3e186a3795cb"),
        ("Produce tokenless local evidence", "run", "502393ad434963eedac645a81f7b6914dc2ba02338b6344338a6e7304dc8d2bf"),
        ("Seal local evidence and binary", "run", "6a74532c3e401d62689ad1cff2e691c24fcbbe65e20bc66d03f9c17906da6ba3"),
        ("Upload exact local evidence bundle", "uses", "632de9b36110790dc8398e37c9f59b15276247fcd177daa284d5796fd14c500a"),
    ),
    (".github/workflows/ci.yml", "performance-policy"): (
        ("Check out repository", "uses", "a46f6f0525346f070ee0a4545858a5d64b7a22551556dde99a5b12759c31ba1d"),
        ("Provision pinned Rust toolchain", "run", "3cd2e27207eedcadb97bf9e08c3ae967b8bdae43dcf621d75c1e0b54fba57225"),
        ("Require native Docker capability", "run", "53093345563f4dbf98e2cc3ae6e08e285313869ca8ae91974b8297012478dafb"),
        ("Clean, hermetic, and native performance policy", "run", "e804ccb0b2849958c830dd3c82cc88c674ed77b6ee7caf08a262e9e02aca9d0d"),
    ),
    (".github/workflows/ci.yml", "provider-and-policy"): (
        ("Check out repository", "uses", "a46f6f0525346f070ee0a4545858a5d64b7a22551556dde99a5b12759c31ba1d"),
        ("Provision pinned Rust toolchain", "run", "3cd2e27207eedcadb97bf9e08c3ae967b8bdae43dcf621d75c1e0b54fba57225"),
        ("Cache cargo registry and build artifacts", "uses", "01cd0291858fd7218948c5dc72b333364b7223ca577a805debfcce5f8bb4245b"),
        ("Require native Docker capability", "run", "53093345563f4dbf98e2cc3ae6e08e285313869ca8ae91974b8297012478dafb"),
        ("Direct containerd/stargz provider", "run", "5cbc9d9c37b5cfec31278c2ca356856b03703c696f435be32f4e64edb01c2a3c"),
    ),
    (".github/workflows/ci.yml", "runtime-integration"): (
        ("Check out repository", "uses", "a46f6f0525346f070ee0a4545858a5d64b7a22551556dde99a5b12759c31ba1d"),
        ("Provision pinned Rust toolchain", "run", "3cd2e27207eedcadb97bf9e08c3ae967b8bdae43dcf621d75c1e0b54fba57225"),
        ("Cache cargo registry and build artifacts", "uses", "01cd0291858fd7218948c5dc72b333364b7223ca577a805debfcce5f8bb4245b"),
        ("Require native Docker capability", "run", "53093345563f4dbf98e2cc3ae6e08e285313869ca8ae91974b8297012478dafb"),
        ("Provision privileged copy-strategy prerequisites", "run", "5b8cf7524869cf019feac6c32be20bb85e46a5c480e83d21f3ba3a0b9c13682b"),
        ("Reflink and bounded-stream copy strategies", "run", "84d93b31860fa78b7198ba3ee6b5ddf83be06b70b9792619bd4b9e482c870e14"),
        ("Prepare pinned runner profile", "run", "90d3ff3a2b0a55e1d748fe6accb4c678825e72b99c7dcf36f3661dffb2f446a6"),
        ("Run real-daemon executor tests", "run", "4e93c5680a3043b877ae3454c6c207d8d390e1991cebd8b4d9a79e62d1f99d26"),
    ),
    (".github/workflows/release.yml", "finalize"): (
        ("Check out exact source without persisted credentials", "uses", "9a191443447054d3de9528d06cd6054992d0e39ff3e5c93df440230ab6880675"),
        ("Prepare exact source and capabilities", "run", "79f476c66cd5a8491f863d2b8078fa55ba765b31274e4f7c41d55befd9244d07"),
        ("Download prepared bundle", "uses", "44bbf68fa5287483bf49ae006221f2591a8e7b7a6c035b426bb22fc502bafed3"),
        ("Download local evidence bundle", "uses", "b9a2f95c95d6ea78263b519d7f217ab2ee3be5117b37ae035b017416359e452c"),
        ("Download GitHub evidence bundle", "uses", "c5e832f82ecc693c15d4525fcd2c86081c8f72dd49870c6ad85b244373e8e601"),
        ("Reconstruct and verify exact tokenless candidate", "run", "29fd5084dc5038951c091a8b0a6a6427d0c5890db763f2a1606469053887ee82"),
        ("Run manifest-owned native beyond-PATH_MAX invariant", "run", "fe51533315a02a639a885aa1ce0f40351360f84a9a1ebda053d9c13244e8c81c"),
        ("Provision isolated keyring prerequisites", "run", "aa885c0d983114fb49cc36d8f5f991763c9a3177ebeb5feded0162d101760ca0"),
        ("Run production persistent-keyring acceptance", "run", "106a16c28defc247eaeb29851cdea75186b2df18745f695a0e707b5a9b0e1411"),
        ("Provision privileged copy-strategy prerequisites", "run", "5b8cf7524869cf019feac6c32be20bb85e46a5c480e83d21f3ba3a0b9c13682b"),
        ("Reflink and bounded-stream copy strategies", "run", "05d81d01c2cf25937ff089215005a95684d15ccb82b87264e81f53f651b120cd"),
        ("Run real-daemon executor tests", "run", "4e93c5680a3043b877ae3454c6c207d8d390e1991cebd8b4d9a79e62d1f99d26"),
        ("Direct containerd/stargz provider", "run", "5cbc9d9c37b5cfec31278c2ca356856b03703c696f435be32f4e64edb01c2a3c"),
        ("Clean, hermetic, and native performance policy", "run", "e804ccb0b2849958c830dd3c82cc88c674ed77b6ee7caf08a262e9e02aca9d0d"),
        ("Run and verify repository gates under release litci twice", "run", "fa3f4018767c4a3b78800a8f7f232d3398cc4691a62cfce38703e241e774305d"),
        ("Recompare and seal exact candidate immediately before upload", "run", "2d9673925ae2ab1d549f3ca6d058bed60662c311227fe4ca3f8760626cdbd3d5"),
        ("Upload one mode-preserving sealed release candidate", "uses", "98a26e5cdb3dc56c663d7afbdd616e3687ed10db5abe1582f17a4088bbbad83e"),
    ),
    (".github/workflows/release.yml", "github_parity"): (
        ("Check out exact source without persisted credentials", "uses", "9a191443447054d3de9528d06cd6054992d0e39ff3e5c93df440230ab6880675"),
        ("Prepare credential-only source boundary", "run", "0c378e24280296530e2899a5453cca641e69b59d7e501389d6bc64cc43c09e00"),
        ("Collect GitHub evidence with no candidate present", "run", "15fc10609030d11a2c56edc80cfea0ada7a03e12e039580102f46ed0f6edfc7d"),
        ("Seal exact GitHub evidence", "run", "34b551a3cbc3d757dd68882cb0181ff86f765fcc63616facd9736cdea31ea33a"),
        ("Upload exact GitHub evidence", "uses", "d152f65326196c7fab40d8560a2fcf9b86158036fcc0aced54712bac8c035a44"),
    ),
    (".github/workflows/release.yml", "local_parity"): (
        ("Check out exact source without persisted credentials", "uses", "9a191443447054d3de9528d06cd6054992d0e39ff3e5c93df440230ab6880675"),
        ("Prepare exact source and runtime", "run", "a7bbc61a571f23e65eba068c50035d721898e6d0b7218e96ab817cbce010e68f"),
        ("Download exact prepared bundle", "uses", "7376f64140e08d7e8865dc41b0e2f2d56fdcb24cb369f77682a15b3c027204e7"),
        ("Verify and unpack prepared binary", "run", "81904c047cde5a4f198d4ce8eb1180715990199d79f0fad6c39e0fb170438123"),
        ("Produce credential-free local evidence", "run", "9bdc298dfc4976c7773c6930296e44b3229087fd250a286ebad5991d0a815d7c"),
        ("Seal exact local evidence", "run", "4b19d11d8544e7bf017524e6c138827eccab6d5b48a473d45b13641770a5bab6"),
        ("Upload exact local evidence", "uses", "c1f8f395b03ebd6a0686fedec2589f7a0f6686be3c902ce4eb78fd059b3be305"),
    ),
}
MAX_WORKFLOW_BYTES = 1024 * 1024


def validate_route_policy(routes: list[dict[str, Any]]) -> None:
    """Require the schema-validated route inventory's reviewed command identity."""

    try:
        canonical = json.dumps(
            routes,
            allow_nan=False,
            ensure_ascii=True,
            separators=(",", ":"),
            sort_keys=True,
        ).encode("utf-8")
    except (TypeError, ValueError) as error:
        raise GateError(
            f"workflow routes cannot be canonicalized: {error}"
        ) from error
    route_policy = b"greenlit-capability-routes-v1\0" + canonical
    if hashlib.sha256(route_policy).hexdigest() != REQUIRED_ROUTE_POLICY_SHA256:
        raise GateError(
            "workflow routes differ from the checker-owned command policy"
        )


def _read_workflow(root: Path, relative: str) -> tuple[bytes, list[str]]:
    path = root / relative
    with SourceTree(root) as tree:
        raw = tree.read_regular(relative, MAX_WORKFLOW_BYTES)
    try:
        return raw, raw.decode("utf-8").splitlines()
    except UnicodeError as error:
        raise GateError(f"{path}: workflow must be UTF-8: {error}") from error


def load_governed_workflows(
    root: Path,
    governed_workflows: set[str],
) -> dict[str, tuple[bytes, list[str]]]:
    """Read each governed workflow once without hiding semantic diagnostics."""

    if set(REQUIRED_WORKFLOW_POLICY_SHA256) != governed_workflows:
        raise GateError(
            "checker-owned workflow policies differ from the route inventory"
        )
    result: dict[str, tuple[bytes, list[str]]] = {}
    for relative in sorted(governed_workflows):
        raw, lines = _read_workflow(root, relative)
        result[relative] = (raw, lines)
    return result


def validate_step_policy(
    identity: tuple[str, str],
    actual: list[WorkflowStep],
) -> None:
    """Bind every named or unnamed step in exact order and multiplicity."""

    expected = REQUIRED_STEP_POLICY.get(identity)
    if expected is None:
        raise GateError(f"{identity!r}: governed job has no complete step policy")
    observed = tuple(
        (step.name, step.kind, step.block_sha256)
        for step in actual
    )
    if observed != expected:
        raise GateError(
            f"{identity!r}: complete ordered workflow steps differ from policy; "
            f"expected={expected!r}, observed={observed!r}"
        )


def validate_workflow_bytes(
    workflows: dict[str, tuple[bytes, list[str]]],
) -> None:
    """Bind complete bytes after structural checks have run."""

    for relative, (raw, _lines) in workflows.items():
        if (
            hashlib.sha256(raw).hexdigest()
            != REQUIRED_WORKFLOW_POLICY_SHA256[relative]
        ):
            raise GateError(
                f"{relative}: workflow differs from checker-owned policy"
            )
