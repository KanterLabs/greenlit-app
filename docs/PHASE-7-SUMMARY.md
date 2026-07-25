# Phase 7 summary — Fresh execution

Status: **complete**. All six `docs/PHASE-7-fresh-execution.md` exit criteria
pass, the complete `TESTING.md` pipeline passes, Greenlit's dogfood workflow
passes through `litci`, and the live full-CI fixture proves cold, warm,
offline, and exact-matrix evidence behavior.

## What was built

- The executor now schedules dependency-ready jobs concurrently while
  preserving deterministic report order. Global worker limits, matrix
  `max-parallel`, fail-fast, dependency gating, and output propagation are
  enforced without replaying user steps.
- Matrices that depend on `needs` outputs materialize at runtime, including
  axes, include/exclude, controls, conditions, names, runner labels, steps,
  and outputs.
- Cancellation reaches queued jobs, active steps, action and runtime
  preparation, image downloads, services, health waits, and container startup.
  Cleanup continues after cancellation and removes the private resources.
- Each concrete job leg receives unique containers, networks, command-file
  volumes, service identities, and host-port bindings. Configured CPU, memory,
  process, and writable-layer limits reach both jobs and services before
  startup.
- Workspace creation is reflink-first with a bounded streaming-copy fallback.
  Every job receives a fresh writable layer; only immutable runner/source
  content and policy-controlled stores are shared.
- Workflow and job concurrency policies are planned and retained. Job groups
  are case-insensitive, serialize owners, and implement `cancel-in-progress`.
- `litci run --job … --matrix KEY=JSON_VALUE` selects exactly one concrete
  case. Write-back requires that exact scope, and only the selected leg
  receives a JobLock.
- Service health failures identify the service and retain bounded,
  secret-masked logs.

## Deviations and deferred scope

- Machine-wide fairness and workflow concurrency across separate CLI
  processes require the durable daemon scheduler and therefore land in Phase
  8. Phase 7 enforces the same policies within one run.
- Warm runs still execute workflow-owned installs and builds unless the
  workflow declares a cache. The verified immutable preparation path performs
  zero downloads; daemon prewarming and persistent policy-controlled download
  caches are Phase 8 work.
- The host's unprivileged overlay mount is unavailable, so the verified live
  run exercised the bounded copy fallback. Dedicated CI must exercise both
  reflink and copy paths as required by `TESTING.md`.

## New dependencies

None. `indexmap`, already present in the workspace, became a production
dependency of `greenlit-app` for typed exact-matrix filters.

## Tests added/deleted

Extended the existing fake-engine execution fixture to cover concurrent ready
jobs, runtime matrices and runners, max-parallel/fail-fast, concurrency-group
cancellation, service log retention, resource propagation, unique job
resources, and cancellation during active, queued, download, and health-wait
states.

Extended the existing live full-CI fixture to compare cold and warm semantic
result evidence byte-for-byte and verify that exact matrix selection persists
one concrete JobLock. Extended `greenlit-init` coverage for reflink-first and
bounded-copy workspace materialization. No behavior tests were deleted.

## Verification record

The following completed successfully after the final implementation:

- `cargo fmt --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo deny check`
- `python3 tools/check-stubs`
- `cargo test --workspace`
- `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps`
- `cargo bench --workspace`
- `litci run -W .github/workflows/dogfood.yml -e workflow_dispatch --no-input`
- `LITCI_TEST_LIVE_FULL_CI=1 cargo test -p greenlit-app --test full_ci_smoke -- --nocapture`

The dogfood result
`000000000000000018c59d6db62c6e19-000a4077-0000` passed with explicit
degraded/local classification in 83.85 seconds. Greenlit-controlled image
preparation reused verified content with zero downloaded bytes; source freeze
took 500 ms, workspace copy-in 44 ms, and image ensure 11 ms. The final live
acceptance passed its cold, warm, offline, and selected-matrix runs in 79.95
seconds.

Criterion remains record-only through Phase 9. This run recorded statistically
significant regressions against the local historical baseline; Phase 10's
budget gate must establish controlled-host baselines before treating those
comparisons as release failures.

## Stubs

None created and none realized. `tools/check-stubs` reports zero markers and
zero registered rows.

## Conflicts flagged

The Phase 7 brief asks for fair machine/project limits and workflow concurrency
while the durable cross-process scheduler is explicitly introduced in Phase 8.
Phase 7 implements per-run fairness and job concurrency; Phase 8 owns the
machine-wide and cross-run forms so they are not approximated with unsafe
process-local state.
