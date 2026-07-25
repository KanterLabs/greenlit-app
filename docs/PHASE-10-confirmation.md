# Phase 10 — GitHub confirmation and completion

**Prerequisites:** Phase 9 complete.
**Crates:** all existing crates; no new product crates.

## Objective

Greenlit can prove when a matching locked input passed on GitHub, and every
correctness, recovery, security, offline, and performance acceptance gate is
enforced continuously.

## Tasks

- Export a separate fully pinned GitHub workflow plus evidence artifact step;
  never edit, commit, push, or dispatch remotely.
- Import read-only workflow run, job, step, workflow, and artifact evidence;
  verify equivalent lock fields before `github-confirmed`.
- Complete crash, corruption, concurrency, source-race, secret, service,
  architecture, cache-mode, offline, and false-green acceptance coverage.
- Enforce p95 warm sandbox under two seconds, warm workflow under 30 seconds,
  and zero Greenlit downloads on an unchanged second run.
- Update public documentation and build/publish automation without performing
  publication.
- Build, back up the installed binary, install the current release binary, and
  dogfood twice.

## Exit criteria

1. GitHub confirmation is impossible without matching external evidence.
2. Every acceptance test in the v0 spec passes on its required backend.
3. Performance budgets pass on the pinned Linux x86_64 benchmark host.
4. All resources reconcile cleanly after injected failures.
5. The complete pipeline, eager/lazy provider suites, and two dogfood runs
   pass; the second dogfood run downloads nothing controlled by Greenlit.
