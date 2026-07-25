# Phase 4 summary — Environment completeness

Status: **complete**. All seven `docs/PHASE-4-environment.md` exit criteria
pass via their listed verification commands; the complete TESTING.md pipeline
passes (fmt, clippy `-D warnings`, deny, check-stubs, 591 workspace tests, doc
`-D warnings`, criterion benches recorded).

**The headline — the Phase 3 capstone, now green.** `PHASE-3-SUMMARY.md`
closed with `litci run -W .github/workflows/ci.yml` failing at
`rustup: command not found`, and nominated re-running it as the Phase 4
capstone. From a clean clone it now runs this repository's entire CI green in
180 s: `actions/checkout`, `rustup show` provisioned at the version the runner
image carries and picking up `rust-toolchain.toml`, `Swatinem/rust-cache@v2`
against the local shim, `cargo fmt`, `cargo clippy`,
`taiki-e/install-action@cargo-deny`, `cargo deny`, `python3 tools/check-stubs`,
the full workspace test suite, `cargo doc -D warnings`, criterion benches, and
both post-steps. Isolation path: copy-in fallback (this daemon's rootful
Docker denies unprivileged overlayfs with EPERM, as in Phases 2 and 3).

**Exit criterion 1 record.** `fixtures/full-ci/` driven twice by
`crates/greenlit-app/tests/full_ci_smoke.rs` (53.7 s, real daemon): run 1
reports `Cache not found for input keys` and saves; run 2 reports
`Cache restored from key: full-ci-fixture-v1` with `cache-hit=true`. The
artifact uploaded in `build` is downloaded byte-identical in `consume`. The
provisioned tool comes from the converged image on run 2 — that step drops
from 4.4 s to 222 ms with no `greenlit: installed` line. A `postgres:16-alpine`
service is health-gated and answers a real `psql -h postgres` query.

## What was built

- **`greenlit-store`** (new crate) — the `actions/cache` backing store with
  its selection rule isolated as a pure module (version scopes everything,
  first key exact, later keys prefix, first key that matches wins, newest
  within a key); the artifact store; and the axum shim that serves both. Ids
  are allocated by *creating* a directory, so `create_dir` is the lock and two
  concurrent reservations cannot collide across processes; commits are
  renames, so an interrupted upload is never restorable as though whole.
- **The wire protocols, read from toolkit source rather than memory** — cache
  v1 REST (`getCacheServiceVersion` returns v1 unless `ACTIONS_CACHE_SERVICE_V2`
  is set, so the smaller protocol is sufficient and the semantics are
  identical), and for artifacts five JSON twirp methods plus the Azure Block
  Blob subset `blockBlobClient.uploadStream` produces.
- **Engine port extensions** — `inspect_container` (health), volume
  create/remove, image list/remove, and spec fields for ports, health probes,
  network aliases, hostname, `cap_add`, and `privileged`. `engine.rs` split
  into `engine/spec.rs` (data) and `engine/mod.rs` (operations) at the
  400-line rule.
- **Service containers** — per-job bridge, service id as hostname, health
  gating that honours `--health-*`, real port publishing bound to loopback,
  teardown with the job including on failure. Before this, a workflow with
  `services:` planned cleanly and then ran with nothing started.
- **Network policy** — a `CAP_NET_ADMIN` sidecar installs filter rules inside
  the workflow container's own namespace and exits. Internet reachable, the
  shim reachable on exactly its port, every other host address and RFC1918
  range dropped, including `169.254.0.0/16` and the cloud metadata endpoint.
- **Docker-in-Docker** — an isolated `docker:dind` sidecar plus a managed
  `docker` wrapper prepended even when the job image ships a CLI, started only
  when the job's own scripts call `docker`.
- **Convergent images and lazy provisioning** — the runner-images manifest
  pinned to one commit and cached by source SHA; shims for manifest-known
  commands absent from the slim base; and a per-repo converged image committed
  from a *clean* base container.
- **`litci clean`** — the subcommand the spec has listed since v0.
- **Metrics schema 2** — `bytes served`, plus a snapshot fixture that actually
  pins `steps` and `hit_miss` contents.

## Defects the fixtures surfaced (all fixed in-phase)

1. **Blob URLs required a bearer header no client sends.** `@azure/storage-blob`
   treats a signed URL as self-authorizing; `actions/cache` fetches its
   `archiveLocation` with a bare `HttpClient`. Both got 401. The failure modes
   hid it: the Azure SDK turns 401 into a `RestError` whose `message` is the
   *empty string*, and `actions/cache` reports a failed restore as an ordinary
   miss — so a working save-and-restore presented as "the cache never restores
   anything" with the entry sitting correctly on disk. Blob URLs now carry a
   per-run `?sig=`, as a real SAS does, kept distinct from the bearer token so
   the token never lands in a URL a client might log.
2. **The runtime token had to be a JWT.** `upload-artifact` decodes
   `ACTIONS_RUNTIME_TOKEN` and reads backend ids from an
   `Actions.Results:<run>:<job>` scope claim.
3. **Twirp requests are snake_case, responses camelCase.** The generated
   client sends `useProtoFieldName: true` and parses without it. Because it
   also passes `ignoreUnknownFields`, a snake_case response was *ignored*
   rather than rejected.
4. **`options:` was split on whitespace**, shattering GitHub's own documented
   `--health-cmd "pg_isready -U postgres"` into four tokens and rejecting a
   valid workflow.
5. **`cargo fmt` reported "no such command"** — only three rustup proxies were
   linked, and cargo subcommands dispatch to separate binaries.
6. **`python3` had no recipe** — Python reaches the runner image through the
   toolcache, not apt, and `ubuntu:24.04` ships none.
7. **Convergence committed the finished job container**, baking the workflow's
   own `/tmp` and workspace into the image every later run starts from.
8. **A rejected service definition leaked its network**; the docker-sibling
   volume leaked one per run.

## Deviations from the phase file

- **`DOCKER-USER` → netguard sidecar.** The brief names host `DOCKER-USER`
  rules. That chain has no Docker API, so the literal reading requires
  `sudo iptables` on every `litci run`, breaking the spec's "zero
  prerequisites". `AGENTS.md` resolves the conflict toward the spec for
  product behavior. The substitute is stronger, not merely equivalent: the
  workflow container never holds `NET_ADMIN`, verified by a root-in-container
  `iptables -F` failing with "Permission denied" while the drop stayed in
  force.
- **The provisioning recipe is baked into each shim** at generation time
  rather than requested over a host control channel. The brief's requirement
  is that the boundary "accepts no arbitrary package or shell input"; this
  achieves it by having no request at all.
- **apt packages carry no pinned version.** The brief says "at the exact
  versions listed"; the manifest only versions some tools. `Recipe::pinned_version`
  keeps the distinction and the install log says `distribution default`
  instead of inventing a number.
- **`job.services.<id>` is not populated.** No `job` context is built anywhere
  today and the phase brief does not require one; recorded rather than
  expanded into at phase close.
- **DinD requires a privileged sidecar.** `CAP_SYS_ADMIN` alone makes
  `docker:dind` exit immediately. The trade is the one the spec already made:
  the sidecar exists so the *host* daemon is never exposed, and it is confined
  to one run's bridge and removed with the job.

## New dependencies (one-line justifications)

- `axum` (greenlit-store) — the shim server the phase file names by name.
- `serde_json` (greenlit-runtime) — reads the runner-images toolset manifest;
  already the workspace's chosen deserializer.
- `greenlit-store` (greenlit-runtime, greenlit-app) — internal path dependency.
- `ureq` (greenlit-store dev) — drives the shim over real HTTP in the wire
  contract tests; same crate and version `greenlit-actions` already uses.

## Tests added/deleted

Added: the cache selection oracle table; cache and artifact store behavior
tests; two real-HTTP wire-contract suites; `--health-*` and option-tokenizer
tables; DinD wrapper and netguard rule-ordering tables; manifest, shim, fetch
and converged-tag tables; `litci clean` behavior; runtime-token minting; and
`full_ci_smoke`. Deleted: none. Net growth sits in oracle tables and the
phase's named fixture, as `TESTING.md` requires; the one non-table addition,
`full_ci_smoke`, *is* exit criterion 1.

## Stubs

None created, none realized. `tools/check-stubs`: 0 markers, 0 registered rows.

## Conflicts flagged

- **`TESTING.md`'s isolation-path coverage gate is still unmet.** It requires
  CI to fail if either isolation path went untested; hosted runners only ever
  exercise copy-in, and the daemon-gated tests self-skip silently. The gap
  predates Phase 4 — it has been open since Phase 2 — and closing it is CI
  work that belongs with Phase 5's benchmark gate.
- **Phase 3's open owner item carries forward**: verifying the GitHub App's
  read-only contents/variables permissions.
