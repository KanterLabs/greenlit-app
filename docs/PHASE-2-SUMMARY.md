# Phase 2 summary — Execution: containers and `run:` steps

Status: complete. All seven exit criteria verified by running their commands; the complete
TESTING.md pipeline passes (fmt, clippy `-D warnings`, deny, check-stubs, 271 workspace
tests, doc `-D warnings`). The daemon-gated integration suites were run against a live
Docker 29.6.1 daemon with skip-detection (no self-skips occurred), covering the shell-ci
fixture end to end, both isolation halves of the `rm -rf "$GITHUB_WORKSPACE"` invariant,
the write-back confirmed/cancelled/symlink flows, and all three engine-detection states.

**Dogfood record (exit criterion 7):** 2026-07-23, `litci run -W
.github/workflows/dogfood.yml -e workflow_dispatch` from a clean clone — green in 66 s
wall-clock (provision 4.7 s, fmt 0.4 s, clippy 17.0 s, check-stubs 0.6 s, test 38.3 s;
exec 62.0 s of 65.8 s total). Isolation path: copy-in fallback (rootful Docker denies
unprivileged overlayfs with EPERM; the fallback marker appears in the container log).
The run executes inside a `rust:1.96.0` job container, exercising the Phase 2 custom
`jobs.<id>.container` path, `defaults.run.shell`, and per-step `timeout-minutes` on a
non-greenlit image. Dogfooding immediately earned its keep — the first two runs surfaced
three real defects (see below), all fixed before the green run.

## What was built

- **`greenlit-runtime`** — `ContainerEngine` trait (async pull/build/commit, container
  lifecycle, streamed exec, networks, `export_path`, in-namespace `terminate`) with a
  pure-bollard Docker backend; three-state engine detection over an injected prober
  (`DOCKER_HOST` → Docker socket → Podman socket; rootful/rootless daemon-stopped fixes;
  absent → `litci setup`), with remote/ssh `DOCKER_HOST` rejected before any probing;
  Linux x86_64 host validation and the supported runner labels mapped to versioned
  Ubuntu images; base-image build through the engine API tagged with a content hash.
- **`greenlit-init`** (private, `publish = false`, embedded in `litci`, never a host
  command) — overlayfs workspace over the Docker-level read-only repo bind, errno-classified
  copy-in fallback with a greppable log marker; the workspace's one `unsafe` mount call
  confined to a documented module.
- **Execution semantics** — one container per job, one exec per step; `jobs.<id>.container`
  image/env/ports/named-and-anonymous volumes with per-run namespaced volume names, and
  source-spanned rejection of `--privileged`, host networking, host PID/IPC, host bind
  mounts, reserved destinations, and `credentials:` (Phase 3); GitHub shell resolution
  (`bash -e {0}` / `sh -e {0}` / `shell:` / `defaults.run.shell`, `working-directory`);
  env layering with the `GITHUB_*`/`RUNNER_*` set; fresh-per-step command files
  (`GITHUB_ENV`/`OUTPUT`/`PATH`/`STEP_SUMMARY` with GitHub's 1 MiB summary cap); log
  commands with immediate `::add-mask::`; `outcome` vs `conclusion` with
  `continue-on-error`; runtime `if:` over live `steps`/`needs` and status functions;
  range-validated per-step `timeout-minutes` enforced via a fresh-exec terminate;
  direct-dependency `needs.<id>.result`/`.outputs` with transitive chain-failure/
  cancellation propagation (`RunStatus::Blocked`), matrix last-writer-wins merge,
  redaction, and size limits.
- **`--write-back`** — exports the overlay upper as a tar, lists changed paths, requires
  interactive confirmation, and applies through a symlink-safe descriptor-based writer
  (`HostRoot`, rustix `*at()` with `O_NOFOLLOW` per component); refused with `--no-input`
  and with `--isolation copy-in`, and implies a *required* overlay (see defects below).
- **Output & metrics** — live streamed logs with group folding and per-step status lines,
  end-of-run table, and the new detection / image-ensure / container-boot / overlay-setup /
  exec stages in both the stderr timing render and the versioned `~/.litci/metrics/`
  record (`litci stats` renders them).
- **Dogfood workflow** — `.github/workflows/dogfood.yml` (fmt, clippy, check-stubs, test
  in a `rust:1.96.0` job container), gated to `workflow_dispatch` so GitHub never auto-runs
  it; a daemonless guard test pins it as plannable and dispatch-only. `cargo deny`, `cargo
  doc`, and benches are consciously omitted until Phase 3 runs the real `ci.yml` (each
  omission documented in the workflow header).

## Defects the dogfood surfaced (all fixed in-phase)

1. **`--write-back` could silently discard changes.** Under `--isolation auto` on a host
   without unprivileged overlayfs (any default rootful Docker), `greenlit-init` falls back
   to copy-in at container start; the exported upper layer exists but stays empty, so the
   run reported "made no changes" while dropping every write. `--write-back` now pins the
   strategy to a required overlay and fails loudly instead (`run_cmd::resolved_strategy`).
2. **The beyond-`PATH_MAX` hashFiles demonstration assumed a capable filesystem.**
   overlayfs (a container rootfs) reconstructs full lexical paths internally and fails
   `mkdirat` with `ENAMETOOLONG` at the `PATH_MAX` boundary; the test now self-skips there
   with a notice, mirroring the daemon-gated convention.
3. **A docker unit test conflated locality with connectivity.** bollard's
   `connect_with_unix` touches the socket path during construction, so the
   `unix:///var/run/docker.sock` acceptance case failed wherever no socket exists — e.g.
   inside the isolated job container, which has none *by invariant*. The test now accepts
   `Connect` errors and rejects only locality misclassification.

## Deviations from the phase file

- The dogfood clause is satisfied by a dedicated shell-only workflow rather than `ci.yml`
  itself: `ci.yml` is built from `uses:` steps, which Phase 2 deliberately rejects.
  From Phase 3 (`uses:` support), litci should run `ci.yml` directly; the dogfood run
  becomes a required CI step in Phase 4 per TESTING.md.
- Jobs run sequentially in DAG order (parallel jobs are explicitly out of scope).

## Known limitations / follow-ups

- **Entrypoint-death UX:** when `greenlit-init` exits at container start (e.g. required
  overlay unavailable), the run surfaces a raw Docker 409 ("container is not running")
  from the first exec instead of init's own stderr message. Surfacing it needs a
  container-logs method on `ContainerEngine` — queue for Phase 3.
- **Overlay needs privilege under rootful Docker:** unprivileged overlayfs inside a
  default container is EPERM, so ordinary runs use the copy-in fallback and `--write-back`
  requires an overlay-capable configuration (rootless daemon or privileged provisioning,
  as exercised by `tests/isolation_overlay.rs`).
- **Copy-in copies the whole repo bind** (including e.g. a multi-GB `target/`); run from a
  clean clone until the Phase 5 speed work adds an exclusion mechanism.

## New dependencies (one-line justifications)

- `bollard` (+ `bytes`, `futures-util`, `async-trait`, `tokio` rt/io/time/macros) — the
  Docker Engine API client; no shelling out to `docker`.
- `tar` — unpack the exported overlay upper for the write-back diff.
- `rustix` (fs, in runtime) — descriptor-based `*at()` traversal for symlink-safe
  write-back application.
- `libc` (greenlit-init only) — the single documented overlay `mount(2)` call.

## Tests added/deleted

271 workspace tests, up from Phase 1's 155 (+116); none deleted. Net growth is the Phase 2
surface itself: engine detection (three states, order, rootless, `DOCKER_HOST` rejection),
fake-engine execution semantics (DAG rollup, gating, masking), cross-job outputs
(success/failure/skipped, matrix merge, redaction, size caps), container-option and volume
rejection, live-daemon suites (shell-ci smoke, both isolation paths, write-back flows,
base image), `host_fs` symlink-safety units, and the dogfood plan guards.

## Stubs

None created, none realized. `tools/check-stubs`: 0 markers, 0 rows. Registry empty.

## Conflicts flagged

None between spec and phase file encountered in Phase 2 scope.
