# Phase 10 summary — GitHub confirmation and completion

Status: **complete**. All five
`docs/PHASE-10-confirmation.md` exit criteria pass, including the cumulative
pipeline, the configured eager/lazy provider suites, enforced native
performance budgets, recovery reconciliation, release packaging, and two
release-binary dogfood runs.

## What was built

- `litci export` creates a separate, fully pinned GitHub workflow and
  `greenlit-evidence-v1.json` from a completed clean run. It pins action
  commits and container digests, gives unnamed steps stable names, embeds one
  pinned evidence-upload job, and durably retains the export alongside the
  local result. It never edits the repository workflow, commits, pushes,
  dispatches, or messages externally.
- `litci confirm` imports GitHub run, workflow, job, step, and artifact data
  through read-only REST calls. It verifies the exact source commit, event,
  semantic workflow digest, distinct successful jobs, ordered successful
  steps, unique unexpired artifact, archive digest, and canonical evidence
  before setting `github-confirmed`.
- The external evidence schema is canonical and lock-matched. Confirmation
  remains impossible for unsupported, degraded, non-clean, or non-hermetic
  local evidence even when a matching GitHub run passed.
- The exported evidence uses a stable source placeholder which its GitHub job
  replaces with `GITHUB_SHA`. This removes the otherwise circular requirement
  for a committed workflow to contain the SHA of the commit that contains
  that workflow. The documented two-pass export verifies that committing the
  separate workflow did not change its semantic digest.
- The native Linux x86_64 whole-run gate executes one cold and 20 unchanged
  warm runs. It rejects any Greenlit-controlled setup download and enforces
  sandbox p95 below two seconds and workflow p95 below 30 seconds.
- Preparation progress distinguishes checking cached images from fetching
  bytes. Greenlit's network-policy helper is itself a pinned immutable image,
  so a warm job no longer performs an `apk add` or another setup download.
- `tools/release-check` verifies and packages the optimized binary plus all
  eight publishable crates as one Cargo workspace operation. The release
  workflow uploads the candidate and permits crates.io publication only after
  an explicit `publish` input and protected `release` environment approval.
- The final release binary is installed at
  `/home/shane/.cargo/bin/litci`. The pre-Phase-10 binary remains recoverable
  at `/home/shane/.cargo/bin/litci.backup-phase10-20260725`.

## Deviations and external operations

- The GitHub confirmation boundary is exercised against a compiled-binary
  integration fixture serving GitHub's real REST and artifact shapes. No
  workflow was committed to or dispatched on a public remote because Phase 10
  expressly forbids doing so without separate owner authorization.
- No crate, binary, container image, dashboard, release, or launch
  announcement was published. The release workflow and archives are
  preparation only.
- The dogfood workflow deliberately uses a fresh `/tmp/dogfood-target`, so
  its approximately two-minute Rust build is workflow traffic. The controlled
  warm fixture isolates and enforces Greenlit's own latency and download
  budgets.

## New dependencies

- `sha2 0.11.0` in `greenlit-app`: hashes exported workflows, GitHub artifact
  archives, and canonical confirmation evidence at the CLI trust boundary.
- `zip 7.2.0` with the `deflate-flate2` feature in `greenlit-app`: reads the
  bounded evidence archive returned by GitHub. The selected backend avoids
  the disallowed transitive license introduced by the default zlib backend.

## Tests added/deleted

Added compiled-binary integration coverage for separate export, exact matching
GitHub evidence, ineligible local-result refusal, and duplicate job-name
non-reuse. Added the 20-sample native whole-run performance gate and its small
representative fixture. Extended evidence contracts so every equivalence
field must match and external confirmation cannot bypass support/hermetic
requirements.

No tests were deleted. The new tests cover the Phase 10 external boundary and
performance exit criteria rather than duplicating private helper behavior.

## Verification record

The following completed successfully after the final implementation:

- `tools/release-check`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo deny check`
- `python3 tools/check-stubs`
- `cargo test --workspace`
- `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps`
- `cargo bench --workspace`
- `LITCI_TEST_LIVE_FULL_CI=1 cargo test -p greenlit-app --test full_ci_smoke -- --nocapture`
- `LITCI_TEST_LIVE_FULL_CI=1 cargo test -p greenlit-app --test cli_behavior policy_modes -- --nocapture`
- `LITCI_TEST_PERFORMANCE=1 cargo test -p greenlit-app --test performance_budgets -- --nocapture`
- `tools/test-stargz-provider`
- two installed release-binary runs of `.github/workflows/dogfood.yml`

The enforced warm measurements were 398.44 ms sandbox p95 and 3,315.01 ms
whole-workflow p95, with zero Greenlit setup downloads across all 20 warm
runs. The live cold/warm/offline fixture passed in 105.10 seconds. The direct
provider suite reported “lazy start, verified demand read, eager parity.”

Dogfood runs
`000000000000000018c5a49b97a836b0-0010ad63-0000` and
`000000000000000018c5a4b87223f3d6-00112de1-0000` passed in 115.56 and
104.07 seconds respectively. Both reused Greenlit-managed content; the second
performed zero Greenlit-controlled downloads.

`litci doctor --json` reported a consistent catalog, zero active leases, zero
partial downloads, and no issues. Historical interrupted-run evidence from
the injected recovery tests remains recorded as designed, while no
Greenlit-labelled container, network, or volume remains.

The installed binary SHA-256 is
`336b225ee0d16b83f12aff8591120bf93a1c67a425fac22654e046ef8629eba3`.
The backup SHA-256 is
`bf17fc6a8c28f19ef805d2eede8eb2cd164a2feb7f72032053260775caf669db`.

## Stubs

None created and none realized. `tools/check-stubs` reports zero markers and
zero registered rows.

## Conflicts flagged

No product-behavior conflict was found. Cargo's single-package prepublication
check cannot resolve an unpublished sibling crate from crates.io; the release
gate therefore packages the publishable workspace together, while the actual
release workflow still publishes crates in dependency order.
