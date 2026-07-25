# Phase 6 summary — Verified content

Status: **complete**. All six `docs/PHASE-6-parity-launch.md` exit criteria
pass, the complete `TESTING.md` pipeline passes, Greenlit's dogfood workflow
passes through `litci`, and the live full-CI fixture proves warm and offline
replay.

## What was built

- Actions, Node runtimes, source objects, and verified downloads publish into
  the machine-wide SHA-256 CAS. Legacy verified action and runtime entries
  migrate lazily; legacy converged runner images are never reused.
- CAS publication is atomic and digest checked. Cross-process single-flight,
  resumable Range downloads, corruption quarantine and refetch, and the
  SQLite-WAL catalog cover immutable objects under concurrent execution.
- OCI tags resolve directly through the Distribution API to Linux amd64
  platform manifests. Jobs, services, Docker actions, and internal sidecars
  receive immutable digest references only.
- Exact action-ref aliases and OCI platform resolutions persist for fully
  cached offline replay. Missing offline content identifies the exact action
  ref, image, runtime, or digest and never substitutes another version.
- `ubuntu-24.04` and `ubuntu-22.04` now map to pinned official GitHub Actions
  runner-controller images and immutable environment fingerprints. Dynamic
  apt convergence, inferred command shims, and reusable dirty runner images
  were removed.
- External immutable runner images receive the private `greenlit-init` helper
  as a read-only bind. Greenlit records that the ARC profile and root execution
  differ from a hosted GitHub runner, so successful local execution remains
  explicitly degraded.
- Preparation progress separately identifies image, action, runtime, and
  workspace setup. Workflow-owned traffic remains inside the named step's
  output rather than being reported as Greenlit setup.

## Deviations and deferred scope

- The pinned ARC images are complete, immutable logical runner profiles but
  are not byte-identical GitHub-hosted runner images. This difference is
  recorded and disqualifies hermetic assurance.
- OCI layers remain verified by Docker's engine-native content store while
  Greenlit catalogs their immutable platform identities. Lazy layer/chunk
  materialization belongs to Phase 7.
- Background prewarming, durable leases and recovery, and parity evidence
  import belong to Phase 7.

## New dependencies

None.

## Tests added/deleted

Added coverage for action and Node-runtime CAS migration, exact offline action
aliases, direct OCI platform selection and architecture rejection, warm
zero-download replay, missing offline identities, offline full-CI replay, and
locked runner-profile selection.

No tests were deleted.

## Verification record

The following completed successfully after the final implementation commit:

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo deny check`
- `python3 tools/check-stubs`
- `cargo test --workspace`
- `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps`
- `cargo bench --workspace`
- `litci run -W .github/workflows/dogfood.yml -e workflow_dispatch --no-input`
- `LITCI_TEST_LIVE_FULL_CI=1 cargo test -p greenlit-app --test full_ci_smoke -- --nocapture`

The dogfood result
`000000000000000018c599bf4f587c55-0006b0cd-0000` passed with explicit
degraded/local classification. Its frozen source was 16 MiB, workspace
copy-in took 42 ms, and warm image ensure took 12 ms. The live full-CI
acceptance passed its miss, warm-hit, and offline runs in 72.26 seconds.

## Stubs

None created and none realized. `tools/check-stubs` reports zero markers and
zero registered rows.

## Conflicts flagged

The earlier Phase 4 convergence design prescribed dynamic package installs and
per-repository converged images. The replacement specification and Phase 6
brief prohibit treating an inferred minimal image as a complete runner.
Phase 6 therefore removes that implementation in favor of immutable locked
runner profiles and records their known differences as degraded evidence.
