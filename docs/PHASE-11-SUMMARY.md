# Phase 11 summary — Durable events and terminal output

Status: **complete**. All five `docs/PHASE-11-terminal-events.md` exit
criteria pass, including real-daemon compact/failure/JSONL coverage, the
cumulative pipeline, stargz provider acceptance, enforced performance
budgets, and two installed-release-binary dogfood runs.

## What was built

- `greenlit-runtime` exposes typed job/step lifecycle events and job-scoped
  masked log chunks. Its original flat-writer API remains as a compatibility
  adapter; the CLI no longer parses or trusts human workflow output as
  lifecycle state.
- Every run persists a sequenced, timestamped, schema-versioned
  `events.ndjson`. It records preparation/content/cache, job, action
  pre/main/post, step, log, cancellation, and terminal result observations.
  The journal is synchronized before successful evidence publication and
  after its terminal record. Aborted preparation also closes the journal.
- Default plain output is compact and outcome-oriented. Successful bodies
  stay in the journal; `--log-mode full` streams them. A failed step shows at
  most the final 200 lines or 256 KiB and prints its exact replay command.
- `litci run --format jsonl` writes the exact journal records to stdout.
  `--color auto|always|never` and `NO_COLOR` control styling, with ASCII
  output when unstyled.
- `litci logs` replays the latest or selected journal with job, concrete
  instance, step id/ordinal/event id, tail, follow, and plain/JSONL filters.
  Old runs without journals fail with an actionable version boundary.
- The obsolete direct progress renderer was removed; preparation observations
  now use the same durable record and projection path as execution.

## Deviations and external operations

- The approved scope deliberately ships no full-screen TUI. Plain output is
  the reviewable foundation for any later interactive renderer.
- The stargz acceptance fixture now uses the CI runner's job-scoped temporary
  directory. This prevents unrelated shared `/tmp` cleanup from removing its
  bind source between creation and Docker mount validation.
- The implementation was published through the Phase 11 pull request; no
  release, package, container image, or external message was published.
- The release binary was installed locally at
  `/home/shane/.cargo/bin/litci`. The previous binary is recoverable at
  `/home/shane/.cargo/bin/litci.backup-phase11-20260726`.

## New dependencies

None.

## Tests added/deleted

- Added compiled-binary `litci logs` integration coverage for latest-run
  selection, job/step/tail filtering, exact JSONL replay, and pre-journal run
  rejection.
- Extended the existing performance fixture to prove, on a real daemon, that
  compact success bodies and a workflow-forged success marker remain journal
  logs, failure output obeys the 200-line bound and points to replay, and
  JSONL stdout is byte-identical to `events.ndjson`.
- Existing live action/full-CI fixtures explicitly select full log mode where
  their semantic assertions consume workflow body text.
- Deleted the old direct progress-renderer tests with the renderer itself.
  No product-behavior test was deleted.

## Verification record

The final implementation passed:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo deny check`
- `python3 tools/check-stubs`
- `cargo test --workspace`
- `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps`
- `cargo bench --workspace`
- `LITCI_TEST_PERFORMANCE=1 cargo test -p greenlit-app --test performance_budgets -- --nocapture`
- `tools/test-stargz-provider`
- two installed release-binary runs of `.github/workflows/dogfood.yml`

The enforced warm measurements were 354.93 ms sandbox p95 and 3,141.04 ms
whole-workflow p95, with zero Greenlit setup downloads across all 20 warm
runs. The provider suite reported “lazy start, verified demand read, eager
parity.”

Dogfood runs
`000000000000000018c5e2fc6b8ffdff-000bf5b8-0000` and
`000000000000000018c5e31585bd29c1-000c75e5-0000` passed in 104.03 and
94.09 seconds. The unchanged second run reused all Greenlit-managed content.
Installed-log plain and JSONL replay both succeeded against the second run.

`litci doctor --json` reported a consistent catalog, zero active leases, zero
partial downloads, and no issues. The installed binary SHA-256 is
`702a0c1449e23fb1a8c11c48fe58f47309df87e32ca11492071aad6d90946139`;
the backup SHA-256 is
`985c91d66338b3879194a559d68e3f936b4b1eaaf891ecc9ad7bfa854fa4cab7`.

## Stubs

None created and none realized. `tools/check-stubs` reports zero markers and
zero registered rows.

## Conflicts flagged

No product-behavior or test-governance conflict was found.
