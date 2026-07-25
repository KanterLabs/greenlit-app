# Phase 9 summary — Lazy and hermetic execution

Status: **complete**. All six `docs/PHASE-9-lazy-hermetic.md` exit criteria
pass, the cumulative pipeline passes, the real eager/lazy provider suite
passes, and Greenlit's dogfood workflow passes through the release binary.

## What was built

- Backend-neutral `RunnerProvider` and `Snapshotter` interfaces separate
  immutable OCI resolution from execution materialization. The universal
  eager Docker snapshotter consumes the same verified runner identity as the
  direct containerd/stargz provider.
- The lazy provider uses direct tonic gRPC over the configured containerd Unix
  socket, requires linux/amd64 remote-snapshot support and eStargz TOC
  annotations, and never invokes a CLI subprocess. Its pinned live suite
  starts at approximately 6.2% materialization, reads an unprefetched file on
  demand, observes fetched bytes increase, and matches eager output and
  digest evidence.
- `--clean`, `--hermetic`, and `--offline` are independent recorded policies.
  Clean mode disables Greenlit mutable build caches and local shims while
  retaining immutable CAS and dependency-download reuse. Hermetic mode implies
  clean, rejects late mutable checkout, and defaults external job traffic to
  reject after internal exceptions.
- RunLocks and result evidence include provider, snapshotter, runtime, kernel,
  architecture, and privileged-infrastructure fingerprints. Execution now
  crosses the worker boundary with the exact finalized runner image rather
  than selecting a hardcoded profile during boot.
- Greenlit's internal network guard is a pinned, tool-bearing immutable image,
  eliminating the former per-job package installation. Every setup fetch is
  reported as a hit, prefetch, or current download.
- Worker admission is machine-wide through crash-releasing kernel file locks.
  One run cannot consume every machine slot, while deterministic DAG, matrix,
  and per-project controls remain intact.
- CI has a required provider-and-policy job. The provider harness provisions a
  pinned, isolated privileged Ubuntu/containerd/stargz stack and cannot
  silently self-skip.

## Deviations and deferred scope

- The official pinned ARC runner images currently have ordinary gzip layers,
  so they correctly use the eager fallback. The configured live suite uses a
  pinned eStargz fixture to prove the complete lazy contract. Creating or
  publishing a Greenlit-maintained eStargz ARC image is not authorized by this
  phase and no public artifact was published.
- The brief's “use eStargz access profiles” is implemented at the immutable
  artifact boundary: prioritized paths must be baked into the eStargz image.
  A runtime label claiming workflow-derived access prioritization was removed
  because it did not change layer layout or reads.
- Dogfood intentionally uses a fresh Rust build target, so its 117.40-second
  execution is workflow-owned compilation rather than Greenlit setup traffic.
  The Phase 10 controlled warm fixture enforces the product performance
  budgets.

## New dependencies

- `prost 0.14.4`, `prost-types 0.14.4`, `tonic 0.14.6`, and
  `tonic-prost 0.14.6` in `greenlit-runtime`: typed direct containerd transfer,
  image, content, and snapshotter gRPC protocols.
- `tower 0.5.3` and `hyper-util 0.1.20` in `greenlit-runtime`: direct,
  asynchronous Unix-socket transport for tonic without subprocess wrappers.

## Tests added/deleted

Added compiled-binary policy coverage for clean and hermetic runtime state and
evidence. Added the direct live stargz provider test plus
`tools/test-stargz-provider`, which proves partial materialization,
unprefetched demand reads, digest verification, and eager/lazy semantic
parity. Extended fake-engine semantics for locked runner identity, scheduler
fairness/cancellation, and hermetic network policy.

One private protobuf-helper test was removed after its behavior moved behind
the real provider boundary; user-visible provider coverage increased. No
behavior coverage was removed.

## Verification record

The following completed successfully after the final implementation:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo deny check`
- `python3 tools/check-stubs`
- `cargo test --workspace`
- `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps`
- `cargo bench --workspace`
- `LITCI_TEST_LIVE_FULL_CI=1 cargo test -p greenlit-app --test full_ci_smoke -- --nocapture`
- `LITCI_TEST_LIVE_FULL_CI=1 cargo test -p greenlit-app --test cli_behavior policy_modes -- --nocapture`
- `tools/test-stargz-provider`
- `target/release/litci run --no-daemon -W .github/workflows/dogfood.yml -e workflow_dispatch --no-input`

The live full-CI fixture passed in 125.95 seconds and the real provider suite
reported “lazy start, verified demand read, eager parity.” Dogfood result
`000000000000000018c5a37f31e6a559-000f8909-0000` passed with explicit
degraded/local classification in 117.40 seconds. Runner, netguard, and
toolchain image preparation were verified cache hits with zero
Greenlit-controlled downloaded bytes.

Criterion remained record-only as required through Phase 9. The parser
microbenchmark showed a small noisy approximately 2.4% regression while the
other measurements were unchanged or improved; controlled budget enforcement
begins in Phase 10.

## Stubs

None created and none realized. `tools/check-stubs` reports zero markers and
zero registered rows.

## Conflicts flagged

No product-behavior conflict was found. The access-profile task required
clarifying that prioritization is encoded in immutable eStargz layer ordering,
not inferred at runtime; the implementation fails closed instead of presenting
an ineffective label as evidence.

