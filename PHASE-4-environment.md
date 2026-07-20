# Phase 4 — Environment completeness

**Prerequisites:** Phase 3 complete (actions, variables, secrets, and auth run green).
**Crates:** `greenlit-store` (new), `greenlit-runtime` and `greenlit-engine` (extend).

## Objective

A realistic full workflow — services, cache, artifacts, tool dependencies — matches its GitHub run step for step. Runtime tool recovery never replays a user's script. This phase closes act's remaining in-scope fidelity gaps.

## Tasks

### Convergent images (greenlit-runtime)

- Resolve `ubuntu-latest` to its pinned v0 version first. Fetch and parse the GitHub `runner-images` manifest matching the resolved `ubuntu-24.04` or `ubuntu-22.04` label; build a lookup of command → tool → version → install recipe. Cache each parsed manifest locally with its source commit and runner label.
- Static pre-provisioning: from `run:` script analysis and known `uses:` requirements, install identifiable missing tools before the job starts.
- **Command-level lazy provisioning:** generate Greenlit-controlled shims only for manifest-known commands absent from the slim image and put their directory last in `PATH`, so installed real commands always win. On first invocation, a shim asks the host control boundary to install the exact manifest-pinned tool, waits for success, and `exec`s the original argv and environment against the real command. Commands not present in the matching GitHub image have no shim and fail normally. Never restart the shell or whole step; commands and side effects before the missing command execute exactly once.
- Runner-manifest provisioning applies only to ordinary Greenlit runner images. Never inject host-runner tools or convergence layers into a user-declared job container; a command missing from that image fails as it does in the same GitHub job container.
- Provisioning failures produce the missing tool, resolved runner label, attempted version, and one fix action. Every successful automatic install logs `installed <tool>@<version> (present on <runner-label>)`.
- After a successful job, commit only the installed-tools layer as a per-repo image (`greenlit/<repo-hash>:<runner-label>`); subsequent runs start from it. `litci clean` removes converged images.

### Cache and artifacts (greenlit-store)

- The `actions/cache` and upload/download-artifact actions talk to HTTP APIs through `ACTIONS_CACHE_URL`, `ACTIONS_RESULTS_URL`, and `ACTIONS_RUNTIME_TOKEN`. Implement a local shim server (axum) on the job network exposing the endpoints those actions call, backed by `~/.litci/cache/` and `~/.litci/artifacts/`. Match cache key/restore-key semantics exactly (prefix matching, version scoping). Verify endpoint shapes against the current toolkit source, not memory.
- Persistent toolcache: mount `~/.litci/toolcache` at `RUNNER_TOOL_CACHE` so `setup-*` actions hit their cache path and skip downloads.
- The same authenticated host-control boundary used by lazy provisioning is the only workflow-network path allowed to request a manifest-approved install; it accepts no arbitrary package or shell input.
- Instrument cache-shim hit/miss and bytes served, toolcache hits, lazy-provision installs and durations, and converged-image reuse in the stage breakdown and run record.

### Service containers (greenlit-runtime + greenlit-engine)

- Per-job bridge network; `services:` containers started before the job with image, env, ports, options; health-check gating (honor `--health-*` options; poll until healthy or timeout); hostname = service key; teardown with the job.
- Job-container and service networking matches GitHub's hostname/port behavior while retaining the host-LAN block.

### Network policy and DinD (greenlit-runtime)

- Workflow and service containers: internet egress allowed; host LAN and host loopback blocked — with exactly the required authenticated pinholes for the `greenlit-store`/provisioning control boundary. Bind only on the Greenlit bridge gateway; `DOCKER-USER` rules allow the exact address:port pairs and drop all other RFC1918/link-local host-side traffic (established/related allowed). Rules are removed on teardown and documented in code comments.
- **DinD sidecar:** deliberately prepend a managed wrapper for the `docker` command even when a custom job image already contains the CLI. Before executing the real CLI, the wrapper attaches and waits for an isolated `docker:dind` sidecar on the job network, sets `DOCKER_HOST`, then `exec`s the original Docker argv. Do not discover a daemon failure by rerunning the user step.
- The host Docker socket is never mounted. Inspect every workflow, action, service, and DinD container in the invariant test.

## Out of scope

Parallel execution, prefetching, warm reuse, performance-budget enforcement, parity suite.

## Exit criteria and verification

1. `fixtures/full-ci/`: workflow with a postgres service, `actions/cache` (miss → save → hit across two runs), artifact upload + download across jobs, and a dynamically selected tool absent from the slim base — `litci run` green twice; second run shows cache hit and converged-image reuse.
2. Lazy-provision fidelity: a side effect before a dynamically chosen manifest-known missing command occurs exactly once; the tool installs at the pinned version and the step passes without script replay. A command absent from the manifest fails exactly as on GitHub.
3. Label fidelity: both 22.04 and 24.04 jobs use their own source-commit-pinned manifests and never borrow a recipe/version from the other label.
4. Network invariant: a step can reach the internet and authenticated Greenlit shims, and cannot reach any other host listener or LAN address.
5. DinD invariant: `docker build` inside a workflow succeeds on first script execution; every container inspection shows the host socket absent.
6. Cache and artifacts match current action endpoint/key behavior across jobs and runs; secrets and runtime tokens remain masked.
7. The complete TESTING.md pipeline passes, including the mandatory Greenlit dogfood CI step from this phase onward.
