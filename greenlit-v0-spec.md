# Greenlit v0 — Evidence-first execution specification

Product and repository: `greenlit-app`. User command: `litci`.

**One line:** Run GitHub Actions locally in fast, disposable Linux
environments, and report exactly how closely each result corresponds to
GitHub.

## Priority order

1. Never produce a false green.
2. Preserve job isolation.
3. Make execution reproducible and explainable.
4. Match GitHub Actions semantics.
5. Reduce startup time and downloads.

Performance work may change latency but may not silently change semantics or
the meaning of a result.

## Core model

Each run freezes its source and every statically resolvable immutable identity
in a `RunLock` before any step executes. A needs-dependent job receives a
`JobLock` after its dependencies finish and before its sandbox starts. A
machine-wide SHA-256 CAS shares verified immutable runner, action, container,
toolchain, source, and download content. Each job receives a new root
writable layer, private workspace, command files, network, services, and
resource namespace.

Static workflow analysis may prioritize prefetches. It never defines which
tools exist: missing lazy content is fetched on demand, and an eager verified
runner is the fallback where lazy materialization is unavailable.

## Required behavior

### Workflow and resolution

- Discover `.github/workflows/*.yml`/`*.yaml`; select a workflow, job, or exact
  matrix case; validate required dispatch inputs before containers start.
- Parse jobs, matrices, `needs`, conditions, expressions, defaults,
  permissions, environments, job containers, services, concurrency, local and
  remote reusable workflows, and local/composite/JavaScript/container actions.
- Resolve reusable workflows and actions recursively, detect cycles and depth
  violations, and preserve GitHub-defined ordering while parallelizing ready
  jobs.
- Resolve action refs to full commits and OCI refs to index and selected
  `linux/amd64` manifest digests. A mutable alias is rechecked before lock
  finalization; if it moved, resolution restarts once and then fails.
- `RunLock` records source/workflow/event/input identities, runner profile,
  action commits/tree digests, static container digests, toolchain requests,
  opaque secret revisions, policy versions, and the support report.
- `JobLock` records the parent lock, matrix and dependency identities, dynamic
  runner/container/service/toolchain resolutions, environment fingerprint,
  resource policy, and sandbox configuration.
- Locks use versioned canonical JSON, SHA-256 self-identities, and are stored
  with the result. Secret values are forbidden in locks, traces, and errors.

### Compatibility and result truth

Every run produces a source-located support report with `supported`,
`degraded`, and `unsupported` findings. Unknown behavior fails closed.
Unsupported findings block affected reachable jobs by default. Only findings
explicitly marked forceable may run with `--allow-degraded`; security-breaking
constructs are never forceable.

Results have independent dimensions:

- Execution: `passed`, `failed`, `canceled`, `blocked`,
  `preparation-failed`, or `aborted`.
- Compatibility: `supported`, `degraded`, or `unsupported`.
- Assurance: `none`, `local`, `clean`, `hermetic`, or
  `github-confirmed`.

`clean` requires new writable state and all transparent Greenlit mutable
caches disabled. `hermetic` additionally requires exact Greenlit environment,
source, action, toolchain, container and architecture identities, no late
mutable input, no disqualifying kernel/runtime capability, and no external
step traffic. `github-confirmed` requires a matching successful GitHub run and
evidence; it is not inferred from a local pass. Cache hits never upgrade
assurance.

### Source and checkout

- Snapshot source at run start from tracked current bytes, tracked deletions,
  and untracked nonignored files. Hard-exclude `.litci/`, reject special
  nodes, preserve modes/symlinks/Git metadata, and retry concurrent changes.
- A clean source reports its commit; a dirty source reports its commit plus a
  stable snapshot digest and cannot be GitHub-confirmed.
- Jobs and local actions read only the snapshot. Edits after locking cannot
  create a mixed run.
- `actions/checkout` of the current repository restores the locked local
  snapshot. A different repository or ref performs a pinned checkout and
  records the resulting identity.
- Parallel jobs cannot mutate each other's workspaces. `--write-back` supports
  one selected job and applies only after listing its diff and confirming the
  host source has not changed.

### Content and environment preparation

- Immutable filesystem objects live in a machine-wide SHA-256 CAS. OCI layers
  may remain in an engine-native digest store but are cataloged and leased by
  the same metadata system.
- Downloads are resumable, single-flight per digest across processes,
  cancelable, digest verified, and atomically published. Corruption is
  quarantined and refetched.
- Runner labels map to immutable Greenlit profile manifests. Profiles record
  OS, architecture, shell defaults, executor version, toolchain inventory,
  logical profile digest, and eager/lazy OCI identities.
- Greenlit never claims that an inferred minimal environment equals a complete
  GitHub runner. GitHub runner image/version/kernel differences are evidence.
- Static analysis prefetches likely actions, services, toolchains and runner
  paths; an underprediction may affect performance only.
- If every locked object is cached, offline execution succeeds. Missing
  offline content reports the exact identity and performs no substitution.
- Registry auth/rate-limit/network failures are preparation failures, not
  workflow failures.

### Sandboxes, actions, services, and networks

- Every job uses a fresh root writable layer, job-private CoW workspace,
  process namespace, command-file directory, network, service set, and
  namespaced volumes. No writable state is reused after user code.
- Immutable runner/action/toolchain/source material is read-only. Job-private
  workspace data may be shared only with that job's Docker-action siblings.
- Implement JavaScript, container, and composite actions; pre/main/post phases;
  checkout; `GITHUB_OUTPUT`, `GITHUB_ENV`, `GITHUB_PATH`, `GITHUB_STATE`,
  summaries; conditions; outputs through `needs`; timeouts; cancellation;
  `continue-on-error`; shell/default/working-directory behavior.
- Actions execute only from commits in a lock. Unknown `runs.using` values are
  unsupported.
- Services use resolved image digests and a job-scoped network, honor env,
  credentials, ports, options, and health checks, retain timeout logs, and
  always tear down without cross-run name/port collisions.
- Internet is available by default; host loopback, host LAN, link-local
  addresses, the host Docker socket, devices, host networking, privileged
  workflow containers, and arbitrary host mounts are unavailable.
- Greenlit-owned privileged infrastructure such as isolated DinD is recorded,
  strictly scoped, and disqualifies hermetic assurance.
- Fork/untrusted source receives no protected secrets. Secret values and
  bounded common encodings are masked across output chunks, structured logs,
  errors, traces, summaries, and service output.
- Configured CPU, memory, process, and disk limits apply before job start.

### Cache policy

Keep identities and policy distinct:

- Runner, actions, OCI images, source, verified downloads and toolchains are
  immutable and allowed in clean verification.
- Package-download content may be reused by checksum/version/architecture, but
  installation steps still run.
- Workflow-authored `actions/cache` remains enabled and reported in every
  mode.
- Transparent compiled-output caches are disabled for clean/hermetic runs and
  never mounted by default.
- Job memoization is out of v0.
- A miss changes performance only. Corruption causes eviction/refetch.

### Scheduling and progress

- Expand matrices deterministically. Start a job only after direct `needs`
  results and outputs are final; evaluate GitHub-compatible skip/status rules.
- Honor matrix `max-parallel` and `fail-fast`, workflow/job concurrency, a
  global worker limit, and a per-project fairness limit.
- Steps remain sequential and execute exactly once unless GitHub defines
  otherwise. Cancellation reaches queued jobs, steps, actions, services,
  containers and preparation tasks.
- Show resolving, compatibility, runner, content, actions, toolchains,
  services, sandbox, steps, and cleanup separately.
- Every fetch identifies cache hit, prefetch, or current download; bytes;
  shared storage reused; and the item causing the wait. Step-time traffic is
  labeled workflow traffic.

### Events, terminal output, and retained logs

- The executor emits typed lifecycle events and job-scoped redacted log
  chunks. Renderers never infer job or step state from workflow text.
- Every run retains a schema-versioned, ordered `events.ndjson` beside its
  lock, trace, and result evidence. `litci run --format jsonl` projects those
  exact records to stdout; `plain` is the default human projection.
- Compact mode retains every redacted body line but hides successful step
  bodies. A failure displays only the final 200 lines or 256 KiB, whichever
  bound is reached first, plus the exact `litci logs` replay command.
  `--log-mode full` streams bodies without changing execution or evidence.
- `litci logs` replays a run journal and filters by job, concrete job
  instance, step id/ordinal/event id, tail count, or follow mode. JSONL replay
  returns the original matching records.
- Color follows `--color auto|always|never`; `NO_COLOR` disables automatic
  styling. Machine output never contains presentation-only records.

### Daemon, recovery, and garbage collection

The same `litci` binary may run a per-user optional daemon. `litci run`
auto-starts/upgrades it, falls back to the identical in-process path, and
supports `--no-daemon`. The daemon watches workflow, local-action, container,
package-lock, toolchain and Git state; prepares immutable content and one-use
clean templates; and yields resources to foreground runs. It never persists
plaintext secrets or changes an active run's lock.

Persist resource intent and state transitions before reporting completion.
On process crash, reboot, interrupted download, runtime failure, disk
exhaustion, or partial cleanup:

- reconcile recorded resources with actual containers, networks, volumes,
  mounts and snapshots;
- mark interrupted jobs aborted unless a future explicit resumption protocol
  applies;
- preserve locks, logs, service logs, trace and result evidence;
- resume verified download progress;
- remove abandoned writable resources;
- never reuse a dirty sandbox.

Leases block deletion. GC removes abandoned writable state first, then
unreferenced snapshots, then least-recently-used unpinned immutable content.
Recent/pinned runs retain their references. Inconsistent metadata blocks
destructive GC. `doctor` reports without deleting by default; `clean` previews
reclaimable bytes and requires confirmation.

### GitHub confirmation

Greenlit can export a separate workflow whose action/container references are
fully pinned and which uploads `greenlit-evidence-v1.json`. Export does not
edit, commit, push, dispatch, or message externally. Confirmation performs
read-only GitHub API access and requires matching source commit, workflow
semantics, event/inputs, action commits, container/toolchain requests, expanded
job/step identities, successful conclusions, and evidence artifact digest.
Without matching evidence, report only that a GitHub pass was observed.

## Performance targets

- Warm native-Linux sandbox creation p95 under two seconds.
- Warm typical workflow under 30 seconds.
- Unchanged warm workflow: zero Greenlit-controlled external downloads.
- First useful step begins before the complete runner downloads when the
  selected provider supports lazy materialization.
- Concurrent requests for one digest perform exactly one external download.
- Typical private workspace materialization adds under 500 MB excluding
  intentional build output.
- Cancellation acknowledgment under one second.

Docker Desktop, emulated architectures, large browser/Android toolchains, and
workflow-controlled downloads may exceed these targets and must be identified
in diagnostics.

## Explicit non-goals

- macOS/Windows/ARM hosted-runner emulation;
- local GitHub OIDC issuance;
- unsafe privileged/host/device/socket modes;
- perfectly hermetic arbitrary internet responses;
- remote CAS, compiled-job memoization, or guessed job skipping;
- public package/image publication, dashboard deployment, or launch posts
  without separate owner authorization.

## Definition of done

Every run has immutable locks and stored evidence; every job starts fresh;
shared content is digest verified and concurrency safe; the support report
explains known differences; classifications derive only from evidence;
repeated runs are fast through immutable reuse rather than dirty containers;
offline and crash behavior are exact; and no unsupported or uncertain behavior
can silently produce a GitHub-equivalent claim.
