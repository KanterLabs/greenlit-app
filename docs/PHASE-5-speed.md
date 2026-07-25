# Phase 5 — Resolution evidence

**Prerequisites:** Phase 4 complete and the evidence-first v0 replacement approved.
**Crates:** `greenlit-workflow`, `greenlit-engine`, `greenlit-runtime`,
`greenlit-store`, `greenlit-app`, and `greenlit-metrics` (extend; no new
crates).

## Objective

Every invocation freezes its source, reports compatibility before execution,
and persists immutable resolution evidence. A mutable reference or unknown
semantic can never silently contribute to a green result.

## Tasks

- Freeze tracked current bytes, tracked deletions, and untracked nonignored
  files into a canonical SHA-256 source tree. Exclude `.litci/`, preserve a
  self-contained Git checkout, retry races, and never read the live tree after
  locking.
- Add versioned canonical-JSON `RunLock`, `JobLock`, support-report, trace, and
  result schemas. Record secret revisions without values.
- Resolve action refs recursively to commits and OCI aliases to index/platform
  digests. Recheck mutable aliases before lock finalization.
- Parse and inventory reusable workflows, permissions, environments, and
  concurrency. Unknown behavior is unsupported; known differences are
  explicit degraded findings.
- Classify execution, compatibility, and assurance independently. Unsupported
  behavior cannot pass; only external GitHub evidence can confirm a result.
- Extend `litci plan --json`, add `litci inspect`, and persist each run below
  `~/.litci/runs/<run-id>/`.

## Exit criteria

1. Dirty and clean source snapshots are stable, race-safe, and exclude a large
   ignored `target/`.
2. Editing source after run start does not alter any job or local action.
3. Action/container aliases changing during or after resolution cannot change
   locked execution.
4. Unknown and unsupported fixtures cannot receive a passing classification.
5. Lock/result JSON is byte-stable, contains no secret values, and explains
   every classification.
6. The cumulative `TESTING.md` pipeline and Greenlit dogfood pass.
