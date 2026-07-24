# Phase 3 summary — Actions, variables, secrets, auth

Status: **complete**. All seven `PHASE-3-actions.md` exit criteria pass via
their listed verification commands; the complete TESTING.md pipeline passes
(fmt, clippy `-D warnings`, deny, check-stubs, 503 workspace tests, doc
`-D warnings`, criterion benches recorded). Three prior waves (action
resolution/store, auth/secrets/variables, full action execution) landed the
Phase 3 surface; this closeout wave built the required `fixtures/actions-ci`
fixture, found and fixed two real defects the fixture's first real run
surfaced, ran the complete cumulative pipeline, ran the Phase 2 dogfood from
a clean clone, and attempted the capstone reality check — running this
repository's own `ci.yml` under `litci` itself.

**Dogfood record (exit criterion 7):** 2026-07-24, from a clean clone of
commit `1ba3269` at `/tmp/greenlit-clean-clone` (never in-place — the
working tree's own `target/` is tens of gigabytes), `litci run -W
.github/workflows/dogfood.yml -e workflow_dispatch` — green in 70.6 s
wall-clock (provision 4.0 s, fmt 0.7 s, clippy 15.7 s, check-stubs 0.9 s,
test 46.1 s; exec 67.3 s of 70.6 s total). Isolation path: copy-in fallback
(this daemon's rootful Docker still denies unprivileged overlayfs with
EPERM, exactly as in the Phase 2 record — the fallback marker appears in
the container log). Unchanged from Phase 2 in every particular that
matters: still a `rust:1.96.0` job container, still green, still proving
`litci` runs its own gates.

**`ci.yml` capstone record (the headline — exit criterion 3):** same clean
clone, `litci run -W .github/workflows/ci.yml` (default `push` event,
`litci` built via `cargo build -p greenlit-app`) — **honest failure**, for a
reason squarely outside Phase 3's declared scope. `actions/checkout@v4`
self-satisfied and succeeded (0 ms); the run then failed at the very next
step, **"Provision pinned Rust toolchain"** (`run: rustup show`), with
`rustup: command not found`. `runs-on: ubuntu-latest` resolves to Greenlit's
convergent base image (`images/base/Dockerfile`: bash/git/curl/wget/jq/tar/
unzip/build-essential/ca-certificates), which does not and should not yet
carry a Rust toolchain — installing the *exact toolchain a repository's
`rust-toolchain.toml` declares* is explicitly **"convergent tool images"**
and **"command-level lazy provisioning,"** both named verbatim on
`PHASE-3-actions.md`'s own Out-of-scope line and covered by `PHASE-4-
environment.md`, not this phase. The two marketplace actions further down
the job (`Swatinem/rust-cache@v2`, `taiki-e/install-action@cargo-deny`)
never got the chance to run at all — the failed step short-circuits the
rest of the job — but both were already resolved and fetched during the
job's action pre-pass (`action-fetch`: 0 hit, 2 miss; `action-runtime-fetch`:
0 hit, 2 miss — the pre-pass resolves every `uses:` step's manifest up
front, independent of whether an earlier step's failure will ever let
execution reach it), so this run says nothing at all about
`actions/cache`-based degradation one way or the other; that remains
untested and is Phase 4's to prove. Per-step outcomes: `Check out
repository` ✓ (0 ms), `Provision pinned Rust toolchain` ✗ (230 ms), every
remaining step skipped, `Post actions/checkout@v4` ✓ (0 ms, the documented
self-checkout no-op). Stage timings: parse 0.8 ms, eval 0.4 ms, plan 0.5 ms,
detection 1.3 ms, action-resolve 406 ms, action-fetch 4.4 s,
action-runtime-ensure 7.8 s, image-ensure 13 ms, container-boot 508 ms,
overlay-setup 322 ms, exec 232 ms, **total 12.1 s**. Nothing here is a Phase
3 defect: no in-scope action-execution behavior (resolution, fetch,
checkout, pre/post, env layering) is implicated, so nothing was "fixed to
make it pass" — that would have been exactly the silent scope creep
`AGENTS.md` forbids. **What Phase 4 needs**: `ubuntu-latest`/`ubuntu-24.04`/
`ubuntu-22.04` job containers must detect a repository's declared Rust
toolchain (`rust-toolchain.toml`, here pinning channel `1.96.0` plus
`rustfmt`/`clippy`) and provision it before the first `run:` step that needs
it — the label-specific convergent image and command-level lazy
provisioning work `PHASE-4-environment.md` already scopes. Once that lands,
this exact capstone command is the natural next thing to re-run; if it then
reaches `Swatinem/rust-cache@v2`, that is the first real signal on
`actions/cache` degradation this project will have.

## What was built

Across all four Phase 3 waves:

- **`greenlit-actions`** (new crate) — `uses:` parsing for all four
  documented forms; ref → commit SHA resolution (`GitHubApiResolver`/
  `GitLsRemoteResolver`, with a zero-network fast path for an already-full
  SHA); a content-addressed action store (`~/.litci/actions/<owner>/<repo>/
  <sha>/`) with a tarball-then-git-clone fallback fetcher and a hit/miss
  invariant test proving zero network fetches for a second resolution of a
  pinned SHA; a from-scratch `action.yml`/`action.yaml` parser (own
  `saphyr-parser`-driven span-preserving raw tree, matching
  `greenlit-workflow`'s duplicate-key discipline) modeling `inputs`/
  `outputs`/`runs` for `node20`/`node24`/`composite`/`docker`.
- **Action execution (`greenlit-runtime::executor::actions`)** — a per-job
  resolve pre-pass (recursing into composites up to GitHub's documented
  10-level depth) that fetches every action's source and pinned Node
  runtime *before* the job container boots; JavaScript actions via the
  checksum-pinned `actions/runner` v2.336.0 Node20/Node24 runtime bundles
  (standard + Alpine, libc-probed at exec time); the full `INPUT_<NAME>`/
  workflow-command-file/`STATE_`/pre-post protocol (post steps drained
  LIFO at job end regardless of failure); composite actions (nested
  `inputs`/`steps`/blocked `secrets`, nested `uses:`); Docker actions
  (build-from-Dockerfile or pull, run as an isolated sibling sharing the
  job's workspace through a run-scoped named volume, never a host socket);
  `actions/checkout` self-satisfied with zero network for the workflow's
  own repository, a real authenticated clone otherwise.
- **Variables (`greenlit-engine` + `greenlit-app`)** — the final resolution
  chain (`--var` → process env → `.litci/vars` → authenticated GitHub
  repository/organization configuration variables, repository overriding
  organization), a named-lookup and a dynamic-map-fetch path, empty-string
  parity for an authenticated-absent name, a permission-error path naming
  the exact fix, and a hard stop for `litci auth` before engine detection
  when nothing local resolves a reference and no token is configured.
- **Secrets (`greenlit-app`)** — `-s` → process env → `.litci/secrets`
  (dotenv, `0600`, auto-gitignored) → an interactive no-echo prompt
  offering to persist; every referenced-but-unresolved name is collected
  before any container starts; every resolved value registers with the
  masker before any step runs; `secrets.GITHUB_TOKEN` is excluded from the
  ordinary chain and resolved separately (local override → stored auth
  token → empty string, never a hard failure).
- **Auth (`greenlit-app`)** — `litci auth` device flow against the embedded
  `greenlit-app` GitHub App client ID, `--pat` (with printed permission
  guidance) and `--gh` (`gh auth token`, with a broad-scope warning)
  fallbacks, kernel-keyring-first token storage with transparent refresh,
  workflow-token injection gated on actual `secrets.GITHUB_TOKEN`/
  `github.token` reference, and a `permissions:`-vs-token-grant notice.
- **This closeout wave** — `fixtures/actions-ci/` (below), the two defects
  it surfaced and their fixes (below), the complete cumulative pipeline,
  the Phase 2 dogfood re-run, and the `ci.yml` capstone attempt, all above.

### `fixtures/actions-ci/`

Modeled directly on `fixtures/shell-ci/`. One workflow, one job, gated on a
local variable (`vars.LOCAL_MODE`, supplied via `--var`):

1. `actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683` (`v4.2.2`) —
   self-satisfied, zero network for this already-present local repo.
2. `actions/setup-node@8f152de45cc393bb48ce5d89d36b731f54556e65` (`v4.0.0`,
   `node20`) — a real marketplace `setup-*` action, pinned by full commit
   SHA so resolution needs no ref lookup; `cache:` is left unset so its own
   `post:` phase (`dist/cache-save/index.js`) takes its documented early
   return without ever calling the `actions/cache` service, keeping this
   fixture inside Phase 3's own scope.
3. `./.github/actions/composite` — a tiny in-fixture composite action:
   nested `inputs` scoping, nested expression evaluation, and output
   mapping (`outputs.greeting.value: ${{ steps.echo.outputs.greeting }}`).
4. `./.github/actions/docker-action` — a tiny in-fixture Docker action (a
   3-line Dockerfile): proves sibling-container execution through the
   shared job workspace (see the Docker-action known-issue entry below for
   why it does not use `$GITHUB_OUTPUT`).
5. A final `run:` step asserting every one of the above at once: the
   local (`vars.LOCAL_MODE`) and remote (`vars.REMOTE_GREETING`) variable
   values, both secrets (`-s CLI_SECRET=...`, process-env `ENV_SECRET`),
   `actions/checkout`'s own outputs, `setup-node`'s output, the composite's
   mapped output, and the Docker action's shared-workspace log.

`crates/greenlit-app/tests/actions_ci_smoke.rs` drives it through the
compiled `litci` binary (`support::Sandbox`, copying the fixture into a
temp directory and `git init`-ing it, exactly like `shell_ci_smoke.rs`'s own
consumption of `fixtures/shell-ci`): `support::fake_github::FakeGitHub`
stands in for GitHub's `GET .../actions/variables/{name}` endpoint (env-
injected `LITCI_TEST_GITHUB_API_BASE_URL`) for the remote-variable leg, and
`Sandbox::seed_auth_token` supplies the stored token that lookup needs.
That same stored token is also what the real, unmocked action-fetch
machinery uses for the real `actions/setup-node` tarball download — host-
side action fetching is not workflow-token injection, and
`greenlit-actions`' resolver/fetcher has no test-only base-url override (see
`ARCHITECTURE.md`'s `GITHUB_TOKEN` entry) — so a syntactically-fake token
would 401 against real `api.github.com` even for a public repository; the
test instead shells out to the already-authenticated `gh auth token` on the
machine running it (mirroring `litci auth --gh`'s own fallback) and
self-skips with a clear notice if `gh` is not authenticated. The test is
opt-in behind `LITCI_TEST_LIVE_ACTIONS_CI=1` — the same convention
`actions_nodejs_live.rs` already established — because a green run
downloads a real marketplace action tarball, a real Node.js distribution
(via `setup-node`'s own toolcache logic), and both pinned ~100 MiB node20
runtime bundles into the test's own isolated `$HOME`; it was run explicitly
for this closeout and is green (`LITCI_TEST_LIVE_ACTIONS_CI=1 cargo test -p
greenlit-app --test actions_ci_smoke`, ~14 s once Docker image layers are
warm), asserting the green result, that the remote-variable request
actually reached the mocked endpoint, that outputs propagated (the
"all checks passed" line only prints if every `test` in the verification
step's script passed), that `setup-node`'s `post:` phase ran, and that no
secret or token value ever appears in any output.

## Defects `fixtures/actions-ci` surfaced (both fixed in-phase)

Building the *real* fixture (not a fake-boundary unit test) immediately
found two real defects in composite-action execution — invisible until now
because no test had ever run a composite action's `run:` step against a
real daemon:

1. **A composite step's shell was resolved against the wrong script path.**
   `crate::executor::actions::composite::run_nested_step` built the script
   path for shell resolution as a hand-written `{cmdfiles_dir}/script`, one
   directory short of where `CommandFilePaths::new` actually places it
   (`{cmdfiles_dir}/step-0/script`) — every composite `run:` step failed
   outright with "no such file". Fixed by resolving the shell against
   `paths.script` (the same value `cmdfiles::prepare` writes to).
2. **A composite step's environment was built from an empty base/workflow/
   job layer**, seeing only its own `GITHUB_ENV` accumulation — never the
   job's static `env:` blocks, and, critically, never the container's own
   live `PATH`. The instant one earlier step called `core.addPath()` (as
   `setup-node` and most `setup-*` actions do), *every* later step's
   layered environment — composite or not — started carrying an explicit
   `PATH=<additions>` entry that Docker's exec-env merge applies over the
   container's inherited default, silently dropping `/usr/bin` and
   friends. Fixed by querying the freshly booted container's own default
   `PATH` once (`crate::executor::job::seed_container_path`) and seeding
   it into the job's `base_env`, and by threading `base_env`/
   `workflow_env`/`job_env` through `CompositeEnv` (mirroring
   `crate::executor::step::layered_env`) so a composite step is part of
   the same job-wide environment an ordinary top-level step sees.

Both are pinned directly by the new
`crates/greenlit-runtime/tests/actions_composite.rs`, independent of the
network-heavy fixture. Full detail (including the exact upstream citations)
is recorded in `ARCHITECTURE.md`'s known-issues log.

## Deviations from the phase file

- **Docker actions do not receive the workflow command-file protocol**
  (`GITHUB_ENV`/`GITHUB_OUTPUT`/`GITHUB_PATH`/`GITHUB_STEP_SUMMARY`) that
  every other step kind gets, unlike GitHub's real runner, which exposes
  these uniformly regardless of handler type. `PHASE-3-actions.md`'s own
  Docker-action bullet does not name the workflow-command protocol the way
  its JavaScript-action bullet explicitly does, so this is recorded as a
  scoped, documented gap (`ARCHITECTURE.md` known-issues log, with what
  closing it would need) rather than silently fixed or silently shipped
  unnoted — `fixtures/actions-ci`'s own Docker action was written to prove
  execution through the shared workspace instead, matching
  `crates/greenlit-runtime/tests/actions_docker.rs`'s already-established
  pattern.
- Every other Phase 3 deviation (composite nested-`pre` hoisting
  simplification, a Docker action nested inside a composite being rejected
  outright, a manifest `default:` value used literally rather than
  expression-evaluated, the keyring-over-cross-platform-crate token-store
  choice, the `deny.toml` license-allowlist extension for the `ureq`/
  `rustls` stack) was already recorded by the prior waves in
  `ARCHITECTURE.md`'s known-issues log; nothing here supersedes them.

## New dependencies (one-line justifications)

All introduced by the three prior waves (this closeout wave added none):

- `async-trait` (`greenlit-actions`, `greenlit-runtime`) — object-safe async
  trait methods for the `RefResolver`/`ActionFetcher`/`RuntimeBundleFetcher`
  boundaries tests fake (`TESTING.md`: "mock only true externals").
- `saphyr-parser` (`greenlit-actions`) — reuses Phase 1's YAML-parsing
  choice for `action.yml`'s own span-preserving, duplicate-key-rejecting
  parse.
- `indexmap` (`greenlit-actions`; already a `greenlit-app` dep) —
  declaration-order-significant maps for manifest `inputs`/`outputs` and
  (dev-only) `PermissionsPlan` test construction.
- `serde`/`serde_json` (`greenlit-actions`, `greenlit-app`) — deserializing
  GitHub's REST responses (ref resolution, configuration variables) and the
  stored-token file.
- `flate2` + `tar` (`greenlit-actions`, `greenlit-runtime`) — gzip
  decompression and tar extraction for the action-source tarball fetch and
  a Docker action's build-context assembly.
- `ureq` + its `rustls` stack (`greenlit-actions`, `greenlit-app`,
  `greenlit-runtime`) — this workspace's first outbound HTTPS client
  (GitHub API, device flow, tarball download); `rustls` over `native-tls`
  keeps the single static host binary (full reasoning and the license-
  allowlist extension it required: `ARCHITECTURE.md` known-issues log).
- `dialoguer` (`greenlit-app`) — interactive secret/workflow prompts and
  `litci auth --pat`'s no-echo entry.
- `linux-keyutils` (`greenlit-app`) — kernel-keyring token storage, chosen
  over the cross-platform `keyring` crate's default D-Bus backend (full
  reasoning: `ARCHITECTURE.md` known-issues log).
- `sha2` (`greenlit-runtime`, dev-only) — checksum verification for pinned
  Node runtime bundles and a test helper's own bundle hashing.

## Tests added/deleted

503 workspace tests, up from Phase 2's 271 (+232 across all of Phase 3);
none deleted at any point in the phase. This closeout wave's own share is
+2 (`crates/greenlit-runtime/tests/actions_composite.rs`,
`crates/greenlit-app/tests/actions_ci_smoke.rs`) plus the required
`fixtures/actions-ci/` fixture itself — both net-new named behaviors
(`PHASE-3-actions.md` exit criterion 1's fixture, and the composite
env/PATH regression the fixture's first real run found), not duplicates of
anything an existing table or fixture already covered.

## Stubs

None created, none realized this phase. `tools/check-stubs`: 0 markers, 0
registered rows. Registry empty.

## Conflicts flagged

None new this closeout wave. The prior waves' own conflicts/deviations are
recorded above and in `ARCHITECTURE.md`'s known-issues log.

## Open owner item

`PHASE-3-actions.md`'s Variables section: "Verify the installed app's
current permissions and API endpoint requirements during implementation; if
the supplied app lacks them, stop for an owner-side permission update."
Every code path this requires — repository/organization variable lookup,
precedence, permission-error handling, the exact endpoints and fine-grained
scopes — is covered by mocked-endpoint integration tests
(`crates/greenlit-app/tests/cli_behavior/variables_remote.rs`), verified
against GitHub's current REST reference during implementation
(`crates/greenlit-app/src/vars/remote.rs`'s own module doc comment cites the
endpoints and required permissions). What remains is a **live** check
against the real installed `greenlit-app` GitHub App
(`Iv23liyZuAdn5DSMxtyh`, owned by `@ShaneKanterman04`) and a real repository
it is installed on — this requires the owner to run `litci auth`
interactively (device flow: print a user code, open a browser, approve)
since device-flow login cannot be scripted or run non-interactively by an
agent. **This is pending owner action**; nothing in Phase 3's own exit
criteria depends on it (exit criterion 5 explicitly scopes both device flow
and variable endpoints to a mocked external GitHub endpoint), so it does not
block marking this phase complete, but the owner should run `litci auth`
against a real repository and confirm a `vars.*` reference resolves
correctly before relying on the live path in anger.
