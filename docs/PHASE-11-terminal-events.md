# Phase 11 — Durable events and terminal output

## Objective

Make execution presentation-neutral. Greenlit records one typed, redacted,
ordered event stream and projects it as compact human output, exact JSONL, or
stored-log replay. Workflow output is data and can never impersonate a
Greenlit lifecycle transition.

## Scope

- Add typed runtime ports for job-scoped log chunks and job/step lifecycle
  events. Preserve the existing flat-writer executor API as an adapter.
- Persist schema-versioned `events.ndjson` beside every run's lock, trace, and
  result. Records carry run id, sequence, wall-clock timestamp, elapsed time,
  job/instance/step identity, and typed payload.
- Record resolution, compatibility, content, container, workspace, service,
  cache, job, step, log, and terminal-result observations.
- Make `litci run --format plain|jsonl` select only a projection. JSONL stdout
  is byte-for-byte the same records stored in the journal.
- Make compact logs the default. Successful step bodies remain journaled but
  hidden; failures print at most the last 200 lines or 256 KiB and the exact
  replay command. `--log-mode full` streams every redacted line.
- Implement `--color auto|always|never`; `NO_COLOR` disables automatic
  styling. Unstyled output is ASCII.
- Add `litci logs [RUN_ID] [--job ID] [--step ID|ORDINAL|EVENT_ID]
  [--tail N] [--follow] [--format plain|jsonl]`.
- Keep the terminal projection non-interactive. A full-screen TUI is outside
  this phase and requires owner review of the shipped plain output first.

## Invariants

- Lifecycle events originate only in Greenlit code. Repository-authored bytes
  can produce only `log` records.
- Masking precedes every journal and projection write.
- Event journal failures fail the invocation and cannot silently leave a
  successful CLI result.
- Job attribution remains correct under matrix and cross-job concurrency.
- Compact mode changes visibility only, never execution, evidence, or retained
  logs.
- `--format jsonl` writes no human text to stdout.

## Tests

- Compiled-binary log replay covers latest-run selection, job/step/tail
  filters, exact JSONL records, and old runs without journals.
- A real-daemon smoke run proves successful bodies are absent from compact
  output, present in the journal/replay, and unable to forge step state.
- A failing real step proves the compact tail bounds and exact replay command.
- Existing action, service, cache, cancellation, matrix, secret-masking, and
  dogfood fixtures continue to pass through the event recorder.

## Exit criteria

1. Plain compact output, full output, JSONL output, and stored replay work
   against a real workflow.
2. Every completed run has a durable terminal journal record and exact result
   evidence; journal failure cannot produce a successful invocation.
3. Repository output cannot create a lifecycle event, and retained logs remain
   redacted and correctly attributed under concurrency.
4. Failure tails obey both bounds and point to a working `litci logs` command.
5. The complete `TESTING.md` pipeline, stargz provider suite, performance
   budget, and two installed-binary dogfood runs pass.
