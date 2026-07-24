# Phase 5 — Speed

**Prerequisites:** Phase 4 complete (full workflows run with fidelity).
**Crates:** `greenlit-engine`, `greenlit-runtime` (extend); bench harness in `crates/greenlit-app/benches/` + CI.

## Objective

Hit the spec's performance budgets — < 2s from `litci run` to first step executing; < 30s warm re-run of a typical test workflow — enforced automatically on the supported Linux x86_64 benchmark host, without violating fidelity or security invariants.

## Tasks

### Parallel execution (greenlit-engine)

- Jobs run concurrently in DAG order: a job starts the moment all direct `needs` results and outputs are finalized. Matrix instances fan out in parallel honoring `max-parallel` and `fail-fast` (cancel siblings on failure exactly as GitHub does).
- Concurrency limit: default = host CPU count, `--jobs N` override.
- **Invariant:** steps within a job remain strictly sequential; no step-level result caching, skipping, or replay.
- Log multiplexing: interleaved live output prefixed per job, plus `--quiet` (status lines only) and per-job ordered replay at completion.

### Pipelining (greenlit-runtime)

- While job N runs, prefetch job N+1's resolved runner/job images, action sources, service images, and pinned action runtimes. Prefetch tasks are cancellable and never block a running job's I/O.

### Warm reuse (greenlit-runtime)

- Keep completed job containers in a warm pool keyed by resolved image + job-container configuration + repo; on re-run, reset state instead of recreating: fresh overlay upper, fresh workflow-command files, cleared action state, and re-applied env. State reset must be provably complete — a warm run and a cold run of the same workflow produce identical outcomes and outputs.
- Pool bounds: max containers and max age, configured by internal constants for v0; `litci clean` empties it.

### Benchmarks

- Extend the Phase 1 criterion suite and consume accumulated local NDJSON history as baseline: time to first step (cold and warm), full-run wall time on `fixtures/full-ci/`, and per-stage breakdown from existing spans.
- Run on a pinned Linux x86_64 CI benchmark environment with the engine and required images already warm for the <2s start gate. Record cold setup separately; never hide it inside the warm measurement.
- Budget violations fail CI. Output a versioned JSON series consumed by the Phase 6 dashboard plus GitHub-vs-cold-vs-warm chart data.

## Out of scope

tmpfs upper layers and lazy image-layer loading (deferred until benchmarks justify them), parity suite, launch assets.

## Exit criteria and verification

1. Benchmarks green in the pinned CI environment: < 2s to first step with warm engine/images and < 30s warm re-run on `fixtures/full-ci/`; cold numbers remain published.
2. Determinism invariant: cold and warm runs of the same fixture produce identical step outcomes, outputs, artifacts, and observable workspace state.
3. Cancellation behavior: `fail-fast` matrix cancels running siblings according to GitHub semantics; prefetch tasks cancel cleanly on run completion.
4. All Phase 2–4 integration and invariant tests pass unchanged under parallelism, including no script replay, host isolation, masking, and direct-`needs` output readiness.
5. The complete TESTING.md pipeline and Greenlit dogfood run pass.
