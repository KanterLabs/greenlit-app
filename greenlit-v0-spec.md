# Greenlit — v0 Spec

Product and repository: `greenlit-app`. User command: `litci`.

**One line:** Run your GitHub Actions workflows locally, fast, with results you can trust. Green locally means green on GitHub.

## Problem

You can't run GitHub Actions locally. Debugging CI means push → wait 5–15 min → read logs → repeat. `act` exists but is an approximation: no `vars` context, incomplete `github` context, no cache emulation, unreliable service containers. Developers use it and still don't trust it.

## Product principles

1. **Fidelity** — *if it passes here, it passes on GitHub.* Every engine decision serves this claim.
2. **Zero config** — `litci run` works in any repo with no flags, no image selection, no setup file. If a workflow uses something unsupported, fail immediately with a precise message ("reusable workflows: not in v0"), never mysteriously.
3. **Contained by default** — running marketplace actions means executing untrusted code on your laptop. Nothing an action does can touch the host. Security is not configurable off.
4. **Fast by default** — performance targets are spec commitments, not aspirations: first step executing < 2s after `litci run`; warm re-runs in seconds.

## v0 scope (in)

1. **Workflow engine** — full YAML parse: jobs, steps, `needs`, job outputs, matrix, job-level containers, and `if:` conditions.
2. **Expression evaluator** — complete `${{ }}` support: `github`, `env`, `secrets`, `vars`, `needs`, `matrix`, `steps`, `runner` contexts; all built-in functions. This is act's most-hit gap.
3. **Stable Linux x64 runners, convergent slim images** — on Linux x86_64 hosts, `runs-on: ubuntu-latest`, `ubuntu-24.04`, and `ubuntu-22.04` start from a slim base (shells, git, curl, build tools). `ubuntu-latest` maps to 24.04 for v0. Missing tools are detected (static analysis of `uses:`/`run:` where possible, command-level lazy provisioning otherwise) and installed *at the exact versions listed in the matching GitHub runner-images manifest*, then cached as per-repo layers. Each repo converges to a small image containing exactly what it uses, at real-image versions. Every automatic install is logged visibly, and a user script is never restarted to recover a missing tool.
4. **Action types** — JavaScript actions, composite actions, Docker actions. Covers ~all of the marketplace top 100.
5. **Cache emulation** — `actions/cache` backed by a local store. Same keys, same restore semantics. Second most-hit gap; also what makes re-runs fast.
6. **Service containers** — `services:` blocks with health checks and correct networking.
7. **Artifacts** — `upload-artifact` / `download-artifact` against a local store.
8. **Secrets/vars** — values can be overridden locally. Secrets resolve `-s KEY=VAL` → process environment → `.litci/secrets` (dotenv, `0600`, auto-gitignored) → interactive prompt. Greenlit statically detects every `secrets.*` reference and prompts for missing values before the run starts. Variables resolve `--var KEY=VAL` → process environment → `.litci/vars` → authenticated GitHub repository/organization variables; repository values override organization values. If a referenced value is still unresolved and the user is not authenticated, fail before execution with `litci auth` as the fix. A name absent after a successful API lookup resolves to an empty string. `GITHUB_TOKEN`: `litci auth` uses GitHub App device flow — the `greenlit-app` public client ID is embedded, tokens are read-only and limited to installed repositories, and refresh credentials are stored in the system keyring. Fallbacks: fine-grained PAT paste, or `gh` CLI passthrough with a broad-scope warning. Host-side variable lookup never injects the token into the workflow; workflows that never reference a token get none.
9. **Output & metrics** — per-step logs with GitHub's grouping, exit codes, and an end-of-run table of step and stage timings. Every `plan` or `run` invocation appends a local metrics record (`~/.litci/metrics/`); read-only `litci stats` shows history and trends without adding a record. Metrics never leave the machine — no telemetry, ever.

## Out (v0)

- macOS and Windows hosts or runners; Linux hosts other than x86_64
- `ubuntu-slim`, ARM, preview Ubuntu images, self-hosted labels, larger-runner labels, and custom runner groups
- `concurrency`, environments/deployments, reusable workflows (`workflow_call`), OIDC
- Any GUI, editor plugin, or hosted service
- Step-level result caching or "smart skip" (changes semantics; breaks the fidelity claim)

## Security model

`act` bind-mounts your repo into the container and can hand actions the host Docker socket — a malicious or compromised action gets your files, and via the socket, effectively root on your machine. v0 does the opposite:

- **Repo mounted read-only + writable overlay.** The container sees a writable checkout, but all writes land in a throwaway overlay layer — actions can never modify or plant files in your working tree, and there's no copy cost even on huge repos. An opt-in `--write-back` exports the overlay diff only after listing changed paths and receiving confirmation; the workflow container itself never gets host write access.
- **No host Docker socket, ever.** Workflows that build/run images get an isolated Docker-in-Docker sidecar instead.
- **Network: internet yes, host LAN no.** Actions can pull dependencies but can't reach `localhost`, your homelab, or anything on your subnet.
- **Secret hygiene** — GitHub-style masking in all log output; `.litci/secrets` created `0600` and `.litci/` auto-added to `.gitignore`.

"Safer than act" is a launch talking point, not just a property.

## Fidelity contract (the differentiator)

- Build a **parity test suite**: clone the workflows of ~50 popular OSS repos, run each on GitHub and on Greenlit, and diff top-level workflow-step outcomes.
- Raw parity is exact conclusion matches divided by the union of in-scope expanded workflow-step instances; a missing or extra step is a mismatch. `environment-drift` and `flaky` mismatches lower the raw score and are also published as separate counts. Repositories outside v0 are excluded before the denominator and listed with reasons.
- Ship the raw parity score in the README and run the parity gate on every merge to main from Phase 6 onward. Launch gate: ≥95% raw step-level parity **and zero open `greenlit-defect` mismatches**. Defects get fixed, not filed away; classified environment drift and flakes may remain only with counts and rationale published.
- Every `greenlit-defect` parity failure has a public issue. The suite is the roadmap and the marketing.
- Host the parity results as a live dashboard on the owner's server/domain, updated by CI — the public credibility artifact and launch centerpiece.

## Speed

Targets, enforced by the benchmark suite in the project's CI:

- **Start:** `litci run` → first step executing in < 2s (images pre-baked and pulled once; no per-run builds).
- **Warm re-run:** typical test workflow < 30s.
- **Parallelism:** async orchestration (tokio) — concurrent jobs respecting `needs`, matrix fan-out, parallel image pulls and action fetches.
- **Pipelining:** prefetch job N+1's images and actions while job N runs.
- **Persistent toolcache:** `setup-node`/`setup-python`-style toolchains cached on a permanent volume; those steps drop to near-zero.
- **Warm container reuse:** keep booted containers; reset the overlay between runs.
- Publish the benchmark chart: same workflow — GitHub queue+run vs Greenlit cold vs Greenlit warm.

**Constraint:** steps within a job stay sequential and never skip — result-caching would break the fidelity contract.

**Deferred until benchmarks exist:** tmpfs for the overlay upper layer; lazy image layer loading. GPU is not a speed lever (orchestration is I/O-bound); GPU passthrough is a possible v1 *feature* for testing CUDA workflows.

## CLI

```
litci run                       # run default push-event workflows
litci run -j test               # single job
litci run -e pull_request       # simulate event
litci run -s KEY=VAL            # override a secret
litci run --var KEY=VALUE       # override a configuration variable
litci plan [--json]             # print the resolved execution plan, no containers
litci auth                      # device-flow login (read-only token)
litci setup                     # install/start the container engine
litci clean                     # remove converged images, caches, warm pool
litci stats                     # local invocation history and timing trends
```

One habit command (`run`); the rest are occasional. No config file required to start. The Cargo install package is `greenlit-app`; it installs the `litci` executable.

## Tech

- **Host platform: Linux x86_64 only for v0.** All engine access goes through one Rust trait (Docker-API client behind it), so future platforms and architectures are ports, not rewrites. WSL2 on x86_64 gives Windows users a working path.
- **Supported runner labels:** `ubuntu-latest`, `ubuntu-24.04`, `ubuntu-22.04`. Every other label is recognized and rejected during planning with the supported list and source location.
- **Zero prerequisites:** engine detection is three-state — reachable → run; installed but daemon stopped → offer to start it (`sudo systemctl start docker`, honoring socket activation and rootless `--user` daemons); absent → `litci setup` installs Docker via the official script (sudo prompt, one confirmation). Detection order: `DOCKER_HOST` → Docker socket → Podman socket. Greenlit never surfaces "cannot connect to Docker daemon" — every failure maps to a state plus the one action that fixes it.
- **Rust** — chosen for developer-fit, evaluator correctness, and launch branding. The runner remains I/O-bound orchestration.
- Docker via API (bollard crate), no shelling out to `docker`, no daemon bundled on the host.
- One distributable static host binary. `greenlit-init` is a private embedded build artifact extracted only into the base-image build context.
- Install: brew, cargo, curl script. MIT license.

## Phases

Ordered by dependency. Each phase ends at its exit criterion.

**1. Engine core — parse and evaluate**
- Workflow parser, expression evaluator, job DAG/matrix planning, job outputs/container modeling, supported-runner validation, static extraction, stable plan output, local variable overrides, local metrics, and `litci stats`.
- *Exit:* given a workflow file, synthetic event, and any required local variable overrides, `litci plan` prints the fully resolved execution plan. No containers or network yet.

**2. Execution — containers and `run:` steps**
- Engine trait + bollard backend; three-state detection; stable x64 runner mapping; base and custom job containers; overlay isolation; shell and workflow-command semantics; job-output finalization and live `needs` propagation.
- *Exit:* a real repo's shell-only workflow runs green end to end.

**3. Actions, variables, secrets, auth**
- Action resolution and fetch; pinned runner Node runtimes; JavaScript, composite, and Docker action execution.
- Secrets chain, authenticated GitHub variable lookup, `litci auth` device flow, and PAT/`gh` fallbacks.
- *Exit:* a workflow using `actions/checkout` + a `setup-*` action runs green.

**4. Environment completeness**
- Label-specific convergent images, command-level lazy provisioning, persistent toolcache, `actions/cache` emulation, artifacts, services, network policy, and DinD.
- *Exit:* a realistic full workflow — services, cache, artifacts — matches its GitHub run step for step without replaying user scripts.

**5. Speed**
- Parallel jobs and matrix fan-out; prefetch pipelining; warm container reuse.
- Benchmark suite in CI enforcing the targets (<2s to first step, <30s warm re-run).
- *Exit:* the GitHub-vs-cold-vs-warm benchmark chart is generated automatically.

**6. Parity and launch readiness**
- Parity suite: ~50 popular repos' workflows, raw step-level outcome diffing, and public issues for every Greenlit defect.
- Live parity dashboard, README, install script, release automation, and owner-reviewed launch copy. In-scope defects found here are fixed under the existing phase rules; no new feature scope is added.
- *Exit:* dashboard live at ≥95% raw parity with zero open `greenlit-defect` mismatches; launch materials reviewed and ready for separately authorized publication.

## Post-launch success goals

Front page of Hacker News with the parity table and benchmark chart. Secondary: 1k GitHub stars, and `act` users confirming their broken workflows run green. These are aspirations, not engineering completion gates.
