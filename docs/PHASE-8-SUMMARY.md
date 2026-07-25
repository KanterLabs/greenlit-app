# Phase 8 summary — Daemon, recovery, and storage lifecycle

Status: **complete**. All six `docs/PHASE-8-daemon-recovery.md` exit criteria
pass, the cumulative pipeline passes, the live cold/warm/offline fixture
passes, and Greenlit's dogfood workflow passes through `litci`.

## What was built

- A same-binary, per-user daemon now auto-starts over a mode-0600,
  UID-authenticated, versioned Unix socket. `--no-daemon`, daemon failure, and
  daemon execution share the same authoritative run path.
- The low-priority daemon continuously fingerprints workflow, local-action,
  container, dependency/toolchain, source, and Git inputs. Changes cancel stale
  preparation, prefetch immutable remote actions, and publish one-use source
  templates. A client claims a template atomically and re-hashes the current
  repository before adoption; any mismatch falls back to ordinary capture.
- The CAS catalog durably tracks runs, pins, leases, downloads, and references.
  Leases heartbeat, interrupted runs recover as aborted, partial downloads
  resume, `doctor` reports without deleting, and reference-aware `clean`
  refuses inconsistent metadata or leased content.
- Every engine resource carries an exact run identity. Startup reconciliation
  deletes only terminal, unleased run identities, in container → network →
  volume order. A live SIGKILL test proved an abandoned job is recovered
  without touching two unrelated active Greenlit containers.
- Persisted repository secrets moved from plaintext dotenv to authenticated
  AES-256-GCM vaults. A random mode-0600 user key stays outside the repository;
  legacy plaintext migrates atomically and is removed only after durable
  encryption.
- Secret masking covers direct, multiline, standard/base64url, percent
  encodings, split daemon chunks, annotations/errors, structured reports, and
  retained service output.

## Deviations and deferred scope

- Source templates are clean frozen checkouts rather than paused job
  containers. They remove repeated Git clone/copy work while preserving the
  rule that no post-step sandbox is reusable. Runner-layer lazy
  materialization and warm sandbox budgeting belong to Phase 9.
- Background action prefetch is best-effort and never supplies RunLock
  authority. Foreground resolution still revalidates mutable refs and freezes
  the resolver before any step executes.
- Machine-wide worker fairness remains a cross-process scheduling concern for
  the Phase 9 performance path. Phase 8 prevents background preparation from
  competing with user commands by lowering the daemon's scheduler priority.

## New dependencies

- `ring 0.17.14` in `greenlit-app`: authenticated AES-256-GCM encryption and
  OS-backed secure randomness for the persisted secret vault. The same version
  was already present transitively through rustls.

## Tests added/deleted

Extended the existing CLI preflight fixture to prove daemon/in-process/
`--no-daemon` result identity and one-use source-template adoption. Extended
the existing secrets fixture to prove plaintext migration, ciphertext/key
permissions, absence of plaintext, and later decryptability. Extended the
fake-engine execution fixture to cover direct, chunk-split, encoded,
annotation, structured-result, and service-log masking. Extended the CAS
fixture for leases, terminal run identities, interrupted HTTP range resume,
corruption, and destructive-GC refusal.

Four private dotenv helper tests were deleted when plaintext persistence was
replaced; their user-visible contracts now live in the compiled-binary secrets
integration fixture. No behavior coverage was removed.

## Verification record

The following completed successfully after the final implementation:

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo deny check`
- `tools/check-stubs`
- `cargo test --workspace`
- `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps`
- `cargo bench --workspace`
- `LITCI_TEST_LIVE_FULL_CI=1 cargo test -p greenlit-app --test full_ci_smoke -- --nocapture`
- `litci run --no-daemon -W .github/workflows/dogfood.yml -e workflow_dispatch --no-input`

The live full-CI fixture passed cold, warm, and exact offline replay in 98.18
seconds. Dogfood result
`000000000000000018c5a011c7d50a45-000cd25e-0000` passed with explicit
degraded/local classification in 90.51 seconds. All Greenlit-controlled image
preparation was a verified cache hit with zero downloaded bytes; the dominant
cost was workflow-owned Rust compilation in a deliberately fresh target
directory.

Criterion remains record-only through Phase 9. The local comparison reported
regressions while multiple heavy builds were running concurrently; controlled
budget enforcement begins in Phase 10.

## Stubs

None created and none realized. `tools/check-stubs` reports zero markers and
zero registered rows.

## Conflicts flagged

The Phase 8 brief says to prepare one-use clean templates but does not require
them to be paused containers. Greenlit uses revalidated frozen-source
templates because they are reusable without weakening job isolation; lazy
runner snapshots and warm sandbox creation remain Phase 9 responsibilities.
