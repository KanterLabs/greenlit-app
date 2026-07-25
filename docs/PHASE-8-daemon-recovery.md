# Phase 8 — Daemon, recovery, and storage lifecycle

**Prerequisites:** Phase 7 complete.
**Crates:** `greenlit-app`, `greenlit-runtime`, `greenlit-store`, and
`greenlit-metrics`.

## Objective

An optional auto-managed daemon accelerates preparation without owning
correctness. Crashes, reboots, disk failures, and cleanup failures preserve
evidence and never expose a dirty sandbox.

## Tasks

- Add a same-binary per-user daemon over a UID-checked versioned Unix-socket
  protocol. Auto-start/upgrade it, support `--no-daemon`, and fall back to the
  identical in-process path.
- Watch workflow, local-action, container, lock, toolchain, and Git state;
  cancel stale low-priority work and prepare one-use clean templates.
- Persist resource intent and transitions, heartbeat leases, reconcile engine
  resources on startup, and mark interrupted jobs aborted.
- Implement `doctor`, reference-aware GC, run pinning, retention, reclaimable
  byte previews, and safe `clean`.
- Replace plaintext persisted secrets with an encrypted vault; keep secret
  values out of daemon persistence and redact direct/common encoded forms
  across chunk boundaries.

## Exit criteria

1. Daemon, in-process, and `--no-daemon` paths produce identical results.
2. Killing the daemon mid-download resumes; killing it mid-job leaves no
   reusable resource.
3. Active leases block GC and inconsistent metadata blocks destructive GC.
4. Abandoned overlays/services are reclaimed before immutable content.
5. Direct, split, encoded, error, structured, and service secret output is
   redacted.
6. The cumulative pipeline and dogfood pass.
