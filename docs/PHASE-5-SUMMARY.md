# Phase 5 summary — Resolution evidence

Status: **complete**. All six `docs/PHASE-5-speed.md` exit criteria pass, the
complete `TESTING.md` pipeline passes, and Greenlit's dogfood workflow passes
through `litci`.

## What was built

- A race-checked, canonical source snapshot captures tracked current bytes,
  tracked deletions, and untracked nonignored files while excluding `.litci/`
  and ignored build output. Jobs and local actions read only this frozen,
  self-contained Git checkout.
- Versioned canonical `RunLock`, `JobLock`, support-report, trace, and terminal
  result schemas persist under `~/.litci/runs/<run-id>/`. Trace publication is
  append-only and preparation failures still produce terminal evidence.
- Recursive action resolution freezes tags and branches to full commits and
  rechecks mutable aliases before finalization. Container aliases resolve to
  image digests with architecture validation. The runtime passes only those
  locked identities to jobs, services, Docker actions, and its sidecar.
- Runner evidence records authored and resolved labels, provider, image ID,
  operating system, architecture, and runtime version. Node toolchain bundles
  record exact URLs and SHA-256 identities. Secret evidence contains revision
  digests, never values.
- Compatibility analysis inventories supported, degraded, and unsupported
  behavior before execution. Execution outcome, compatibility, and assurance
  are classified independently; unsupported behavior cannot receive a passing
  result and only imported GitHub evidence can confirm parity.
- `litci plan --json` exposes compatibility findings and `litci inspect`
  renders persisted evidence.
- The machine-wide SHA-256 CAS foundation atomically publishes verified
  objects, single-flights concurrent materialization, resumes partial content,
  quarantines corruption, and records catalog state in SQLite WAL. Frozen
  source objects are ingested into it. Package download caches are shared,
  while compiled build output remains outside the clean fast path.

## Deviations and deferred scope

- Reusable workflows, environments, and concurrency are parsed and inventoried
  as unsupported rather than executed. Phase 5 required fail-closed
  compatibility evidence; their execution semantics belong to later phases.
- OCI aliases are locked through the local Docker daemon in this phase. Direct
  Distribution API resolution, immutable blob ingestion, and offline OCI use
  are Phase 6 work.
- Action, Node-runtime, runner, and image content have not yet all migrated
  into the CAS. Phase 5 established and exercised the verified store; Phase 6
  owns migration and complete offline execution.

## New dependencies

- `sha2` 0.11.0 — canonical SHA-256 identities for source, evidence, content,
  secrets, and toolchains.
- `rusqlite` 0.40.1 with bundled SQLite and default features disabled — the
  process-safe WAL catalog without a host SQLite prerequisite.

## Tests added/deleted

Added coverage for clean and dirty source snapshots, ignored large build
trees, source freezing across edits, stable evidence schemas and
classification, persisted unsupported results and traces, concurrent CAS
single-flight, interrupted-download resume, corruption quarantine/refetch,
and the engine boundary rejecting mutable image aliases.

No tests were deleted from the committed suite. Three temporary private-helper
tests used while developing the runtime lock boundary were removed before
commit because `TESTING.md` bans that test shape; the behavior is retained in
the external `ContainerEngine` acceptance test.

## Verification record

The following completed successfully after the final implementation commit:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo deny check`
- `python3 tools/check-stubs`
- `cargo test --workspace`
- `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps`
- `litci run -W .github/workflows/dogfood.yml -e workflow_dispatch --no-input`

The dogfood result was locally passing with explicit degraded compatibility,
not overstated as GitHub parity. Evidence run
`000000000000000018c5968a3dab013e-00030aeb-0000` retained its lock, trace,
support report, and result. A minimal warm probe began its first useful step
in 1.4–1.6 seconds and reused the existing runner image without downloading
layers. The full dogfood execution spent 3.7 seconds provisioning and 70.3
seconds executing its gates.

## Stubs

None created and none realized. `tools/check-stubs` reports zero markers and
zero registered rows.

## Conflicts flagged

No product-behavior conflict was found between the specification and Phase 5
brief. The brief deliberately requires unsupported inventory before later
phases implement those constructs; the implementation fails closed at that
boundary.
