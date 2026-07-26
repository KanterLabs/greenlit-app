# ARCHITECTURE.md — Greenlit

This document records the architecture as implemented. It is updated with
each phase summary; crates and runtime paths appear as the phase that builds
them lands.

## Crate boundaries

The Cargo workspace uses resolver 2 and Rust edition 2024. Its five Phase 1
crates form an acyclic dependency graph:

```text
greenlit-app      -> greenlit-engine, greenlit-workflow, greenlit-expr, greenlit-metrics
greenlit-engine   -> greenlit-workflow, greenlit-expr
greenlit-workflow -> greenlit-expr
greenlit-expr     -> (no Greenlit crates)
greenlit-metrics  -> (no Greenlit crates)
```

`greenlit-metrics` has no dependency on another Greenlit crate.

| Crate | Boundary in Phase 1 |
|---|---|
| `greenlit-app` | The `litci` binary. It exposes only `plan` and `stats`, discovers a workflow, resolves local `vars` precedence, constructs command inputs, renders plans/diagnostics, and owns top-level `anyhow` context. It contains no workflow or expression semantics. |
| `greenlit-workflow` | The span-preserving YAML parser and typed workflow model. It recognizes the Phase 1 workflow surface, retains rejected-in-v0 constructs for planner diagnostics, and extracts literal `secrets`/`vars`, dynamic `vars` access, action references, and runner labels. Static extraction uses `greenlit-expr`'s real parser rather than a second expression grammar. |
| `greenlit-expr` | GitHub expression lexing, parsing, evaluation, coercion, contexts, status functions, JSON functions, and `hashFiles`. Filesystem access for `hashFiles` is injected behind `HashFilesFs`; `RealFs` is the local production implementation. |
| `greenlit-engine` | The pure planning boundary over a typed workflow, synthetic event, and resolved local contexts. It builds the `needs` graph, detects cycles, expands matrices, validates runner labels, partially evaluates conditions/templates/outputs, preserves runtime-deferred values, and emits the serializable `ExecutionPlan`. Local Git metadata collection and synthetic event construction also live here. |
| `greenlit-metrics` | Local invocation timing and persistence. It opens stage-labelled `tracing` spans, aggregates stage durations, appends schema-versioned NDJSON records, and reads those records for reporting. It has no network dependency or transmission path. |
| `greenlit-runtime` (Phase 2) | The `ContainerEngine` port and its bollard backend, three-state engine detection, base images, workspace isolation, `--write-back`, and the executor that drives a plan. From Phase 4 it also owns the per-job network, service containers, the network policy, Docker-in-Docker, and lazy provisioning. |
| `greenlit-actions` (Phase 3) | `uses:` parsing, ref→SHA resolution, the content-addressed action store, and `action.yml` parsing. |
| `greenlit-store` (Phase 4; verified-content extension in Phase 6) | The local `actions/cache` and artifact stores, the axum shim that serves their wire protocols to unmodified actions, and the machine-wide SHA-256 CAS. CAS objects publish atomically after verification; cross-process in-flight files single-flight materialization; corrupt objects move to quarantine; a SQLite-WAL catalog records objects, trees, aliases, references, downloads, leases, runs, and resources. It performs no I/O outside its own root and opens no socket of its own — `greenlit-runtime` binds the shim onto the job network. |
| `greenlit-init` (Phase 2) | The private container-only entrypoint helper that stacks the overlay and `exec`s the job command. Never a host command; embedded and extracted only into the base-image build context. |

No Phase 1 crate starts containers, accesses a container engine, fetches an
action, contacts GitHub, resolves remote variables, or prompts for secrets.

## Phase 1 dataflow

`litci plan` has one concrete path from authored workflow to output:

```text
CLI arguments + current repository
              |
              v
     workflow path discovery
              |
              v
  [parse] greenlit-workflow ------------------------+
              |                                     |
              v                                     v
   typed, source-spanned Workflow          static reference extraction
                                                    |
                                    --var > process env > .litci/vars
                                                    |
                    local Git metadata ------------+---- synthetic event
                                                    |
                                                    v
                                resolved PlanOptions + event
                                                    |
                                                    v
                                    [plan] greenlit-engine
                          DAG + matrix + partial expression evaluation
                                                    |
                                                    v
                                           ExecutionPlan
                                           /           \
                                          v             v
                               tree or stable JSON   lints/timings
                                    on stdout          on stderr
```

`greenlit-metrics::Invocation` times the parse, context/evaluation, and plan
stages around that path. Completion produces one local record appended to
`~/.litci/metrics/runs.ndjson`, including failed `plan` attempts with the
stages completed before failure. `litci stats` follows a separate read-only
path: it reads the NDJSON store and renders invocation history plus per-stage
average/minimum/maximum trends without creating another record.

## Phase 1 trust and resource boundaries

Repository contents are untrusted even though the local user selected the
repository. Phase 1 therefore bounds and contains every repository-controlled
path before it reaches a host resource:

- Workflow discovery and file loading reject links or paths outside the
  repository, cap source size using the runner's UTF-16 accounting, and keep
  YAML alias expansion, nesting, node count, and materialized-result size
  within the pinned runner limits. The event-stream loader detects duplicate
  keys before typed conversion.
- `.litci/vars` is opened relative to an already-open repository directory
  with no-follow/nonblocking semantics. It must be a regular file and is
  bounded by file size, assignment count, name size, and value size; authored
  values are literal and never recursively interpolated from the host
  environment.
- Local Git metadata uses fixed `git` arguments without a shell, disables lazy
  fetches, applies a five-second child deadline, and bounds both child output
  and retained changed-path data.
- Expression parsing/evaluation, template synthesis, JSON parsing, and plan
  retention have explicit depth and memory budgets. Matrix values and
  reference indexes share canonical storage, while DAG, lint, and event-filter
  passes use bounded/indexed traversal rather than pairwise amplification.
- The production `hashFiles` filesystem lexically normalizes the supplied
  physical workspace path, rejects root-path symlink components, and pins the
  root directory in one `openat2` lookup, lazily (only on first actual use
  inside the supervised worker, never at construction). Every later access —
  entering a literal search root, descending into a child directory, or
  hashing a child file — resolves fresh from that one pinned root descriptor,
  one path component and one `openat2` call per hop, rather than relative to
  a held intermediate descriptor cached from an earlier access: an ancestor
  renamed away since it was last inspected breaks the very next hop through
  it (`ENOENT`, treated the same as any other vanished entry) instead of the
  stale descriptor silently continuing to serve content that has moved
  outside the workspace. Splitting each access into single-component hops
  (rather than one `openat2` call over a joined multi-component path) keeps
  every syscall argument bounded by a single path component's own length
  regardless of how deep the access is, which is what lets this stay
  compatible with the depth-proportional-state guarantee below even past
  Linux's `PATH_MAX`. A directory's `(device, inode)` identity comes from the
  object that same walk reaches. A final regular-file or directory open is
  validated once via `O_PATH` and then reopened through that exact
  descriptor's own `/proc/self/fd` entry — never a second lookup by name —
  so no replacement (a stable special node included) can be raced into a
  readable handle. Regular files stream in 64 KiB chunks; native-order
  directory enumeration retains only the active DFS stack, and that stack's
  lexical path is one shared buffer pushed on descent and popped on ascent,
  so retained bytes stay proportional to depth rather than depth squared.
  Traversal also prunes: a directory is opened, and an entry whose native
  enumeration type went unreported (`DT_UNKNOWN`) is resolved, only if its
  path so far could still lead to a pattern match (toolkit's own
  `match || partialMatch`), so an unrelated or unreadable sibling — or an
  irrelevant `DT_UNKNOWN` entry — is never touched. A non-directory result
  (an entry-point file, a plain child file, or a symlink probe result) is
  ever yielded only on a *full* pattern match with negative patterns folded,
  never merely because it sits at a glob's literal search root or partially
  matches a prefix; a bare literal exclusion (`hashFiles('a.txt', '!a.txt')`)
  therefore folds to nothing rather than bypassing the negation. The exact
  first-alias registry is separately capped at 262,144 entries and 32 MiB of
  retained identity bytes; reaching either cap fails with the single action
  to narrow patterns. A matched symbolic link whose target cannot be found
  fails the whole expression, matching GitHub's own hash helper, rather than
  silently contributing an empty match. One worker owns the complete
  computation and cooperative checks, while the caller enforces the same
  runner-compatible 120-second deadline independently; evaluation therefore
  returns on time even if an uninterruptible host filesystem call leaves
  that worker detached until the call completes. Starting that worker itself
  reserves one slot in a fixed live-worker cap — a single atomic
  compare-and-swap before the thread spawns, released only once the worker
  actually exits, however late — so the cap bounds concurrently live workers
  outright rather than only the ones later detected as stuck (see the
  known-issues log).
- Human stdout/stderr escapes repository-authored terminal controls and has a
  bounded render buffer. The default metrics path is traversed from a held
  `HOME` directory descriptor with no-follow/nonblocking opens; readers and
  repair/append writers take cross-process shared/exclusive locks, and each
  NDJSON record is bounded to 8 MiB. No Phase 1 dependency provides network or
  telemetry transport.

## Job environment dataflow

Everything around one job hangs off its own bridge network, and the ordering
below is load-bearing rather than incidental. Phase 6 removed Phase 4's
runtime apt convergence and command shims: a runner now starts only from an
official GitHub ARC image pinned to its Linux amd64 platform-manifest digest.
The support report explicitly calls this a self-hosted profile rather than a
complete GitHub-hosted runner image.

```text
                    create job network
                            |
              inspect it for the bridge gateway
                            |
          bind the shim on that gateway (cache + artifacts)
                            |
     start services on the bridge, gated on their health probes
                            |
       boot the exact locked runner profile or job container
                            |
       apply the network policy *before* any workflow code runs
                            |
             seed the image's actual PATH once
                            |
                        run the steps
                            |
   tear down container, then DinD, then services, then the network
```

A container reaches the host only at the bridge gateway — `127.0.0.1` inside
a container is the container — which is why the shim binds there and why the
gateway address has to be discovered rather than assumed. The policy is
applied before readiness because a container that has executed even one step
unrestricted has already had its chance to reach the LAN. Teardown runs in
reverse because a network holding any attachment cannot be removed.

Runner profiles are fixed in `executor::runner_profile`: Ubuntu 24.04 uses
the official ARC runner 2.336.0 image and Ubuntu 22.04 uses 2.321.0, each by
its exact amd64 manifest digest. Registry manifests/configs are verified in
the machine-wide CAS, Docker materializes only the digest reference, and
offline replay requires both the CAS metadata and exact daemon image.
Greenlit's private init helper is injected read-only; the profile is never
rebuilt or mutated. Legacy `greenlit/converged-*` images are not consulted.

## Phase 5 immutable-resolution dataflow

Execution no longer reads mutable project or registry state after its lock is
finalized:

```text
live repository
      |
      v
canonical source snapshot -----> machine-wide SHA-256 CAS
      |
      v
workflow parse + compatibility inventory
      |
      v
resolve and recheck mutable action/container aliases
      |
      v
RunLock + per-job JobLocks
      |
      +----> ~/.litci/runs/<run-id>/run-lock.json
      +----> ~/.litci/runs/<run-id>/support-report.json
      |
      v
fresh job execution from frozen source + locked identities
      |
      v
append-only trace.ndjson + terminal result.json
```

The RunLock names the frozen source tree, workflow, runner provider and image,
architecture, runner version, action commits, container digests, toolchain
artifacts, secret revision digests, and compatibility findings. The engine
boundary receives immutable container identities; a mutable tag cannot be
passed to job, service, Docker-action, or internal sidecar creation. Result
classification keeps execution outcome, compatibility, and assurance
independent, so a locally successful run with an unsupported construct is
still blocked from every green classification.

The CAS root is `~/.litci/store/`. Objects are verified before atomic
publication, corrupt entries are quarantined, and an SQLite-WAL catalog tracks
objects, trees, aliases, references, downloads, leases, runs, and runtime
resources. Phase 5 ingests frozen source objects and establishes the package
download-cache fast path. Phase 6 moves action, Node runtime, runner, and OCI
content from their legacy stores into this verified boundary.

## Phase 7 fresh-execution dataflow

The executor schedules dependency-ready jobs asynchronously. A run-level
semaphore bounds total workers, each matrix strategy adds its own
`max-parallel` semaphore, and case-insensitive concurrency groups serialize
owners while `cancel-in-progress` first cancels and cleans the prior owner.
Reports remain in deterministic plan order even though dependency outputs are
merged in actual completion order, matching GitHub's matrix-output behavior.

Runtime-deferred matrices are materialized only after every `needs` result is
available. Their axes, include/exclude entries, controls, runner labels,
conditions, names, steps, and outputs are evaluated against the completed
dependency context. Exact CLI matrix selection is applied to the concrete
legs and persists only the selected JobLock.

Every concrete job leg receives a unique resource namespace:

```text
immutable runner image + frozen source
                  |
                  v
       reflink-first private workspace
          (bounded copy fallback)
                  |
                  v
  unique writable layer + command-file volume
                  |
                  +--> unique bridge + services
                  +--> Docker-action siblings
                  +--> optional DinD sidecar
                  |
                  v
         one sequential step stream
                  |
                  v
 cancel/finish --> reverse-order cleanup
```

Cancellation is a shared token observed around queued permits, immutable
action/runtime/image preparation, service startup and health waits, container
boot, and every active step. Cleanup remains uncancelled so a canceled run
cannot leave a reusable dirty sandbox. CPU, memory, process, and writable-layer
limits are applied before both job and service containers start. Explicit
service ports bind to the job bridge gateway, so parallel jobs can request the
same port without colliding on the host.

Workspace materialization first attempts Linux `FICLONE` per regular file and
falls back to a streaming copy while enforcing fixed entry and byte ceilings.
The source is already frozen before this point, so concurrent host edits cannot
produce a mixed checkout. A completed writable filesystem is never reused;
warmth comes only from immutable images/CAS content and workflow-declared
caches.

## Phase 8 daemon and recovery dataflow

The optional daemon is the same `litci` binary speaking a bounded,
schema-versioned JSON protocol over a mode-0600 Unix socket. Linux peer
credentials must match the daemon UID. A missing, stale, or incompatible
daemon is replaced automatically; `--no-daemon` and every daemon failure use
the same authoritative in-process resolution and execution path.

```text
repository changes ----> low-priority watcher
                              |
                     cancel stale preparation
                              |
                 +------------+-------------+
                 |                          |
       immutable action prefetch    one-use frozen source
                 |                     template
                 v                          |
        verified shared stores       atomic client claim
                                            |
                                  re-hash current source
                                            |
                                 match ------+------ mismatch
                                   |                   |
                               adopt once       discard + capture
```

Run transitions and immutable-object references are durable in the CAS
catalog. Leases heartbeat while a run owns content. Startup recovery first
marks expired, unterminated runs aborted, then reconciles only engine resources
whose `greenlit.run` label or exact namespace names a terminal, unleased run.
Unlabelled resources, active runs, and unrelated managed containers are never
eligible. Containers are removed before networks and named volumes.

`litci doctor` reports catalog integrity, interrupted runs/downloads, leases,
and reclaimable bytes without deleting data. `litci clean` uses the same
reference graph: partial downloads are reclaimed before immutable objects,
active leases and RunLock pins block deletion, and any catalog inconsistency
blocks destructive collection.

Repository-local persisted secrets are AES-256-GCM ciphertext in
`.litci/secrets.vault`; the random 256-bit key exists only at mode 0600 under
`~/.litci/vault.key`. Legacy plaintext dotenv secrets migrate atomically and
are removed only after the encrypted vault is durable. Direct, multiline,
standard/base64url, and percent-encoded variants are registered with the
streaming masker before output reaches terminal logs, annotations, structured
results, retained service logs, or errors.

## Phase 9 provider and policy dataflow

Runner preparation is split across backend-neutral `RunnerProvider` and
`Snapshotter` interfaces. The OCI provider resolves and verifies the exact
linux/amd64 manifest, config, and layer identities in the machine-wide CAS.
Every host can pass that identity to the eager Docker snapshotter. Configured
containerd hosts can instead use the direct tonic gRPC stargz snapshotter; no
containerd or `ctr` subprocess participates in the product path.

```text
locked runner digest
        |
        v
verified OCI manifest + layers in CAS
        |
        +----------> eager Docker materialization
        |
        `----------> direct containerd transfer
                          |
                 require eStargz TOC annotations
                          |
                 stargz remote snapshot prepare
                          |
             first step before all layer bytes arrive
                          |
             verified demand read of later content
```

The lazy path fails closed unless the stargz plugin reports linux/amd64 and
remote snapshot annotation export. Access prioritization is an image-build
property: likely files must be ordered into the immutable eStargz artifact
rather than guessed from a workflow at runtime. Images without eStargz TOC
annotations use the verified eager path.

`--clean` disables Greenlit's mutable build cache, cache shim, artifact shim,
and toolcache while retaining digest-verified immutable CAS and
dependency-download reuse. `--hermetic` implies clean, rejects checkout of
late mutable content during preflight, and installs a default-reject egress
policy after the job's loopback, established traffic, internal shim, and
private service network exceptions. External traffic and privileged
infrastructure are recorded as evidence and cap assurance.

RunLocks include the runner provider, snapshotter, architecture, kernel,
container-runtime implementation/version, and privileged-infrastructure
fingerprint. The executor passes the finalized runner image identity across a
reserved internal lock boundary, so boot cannot silently fall back to a
hardcoded runner alias.

Worker concurrency is machine-wide, not merely process-local. Each foreground
run acquires kernel-backed file-lock slots beneath
`~/.litci/scheduler/v1/slots`; the kernel releases a crashed process's slots.
A run may occupy at most one fewer than the machine worker count, preserving a
slot for competing projects while per-run and matrix limits remain nested
inside that global bound.

## Phase 10 external-evidence and release boundary

`litci export` reads only a completed clean run and produces a separate
workflow. It never modifies `.github`, commits, pushes, dispatches, or sends a
message. The exported workflow pins action commits and container digests,
assigns deterministic names to unnamed steps, and adds one pinned
`upload-artifact` evidence job. The evidence template carries a source
placeholder which the GitHub job replaces with its own `GITHUB_SHA`; this
avoids a self-referential commit identity while preserving an exact
two-pass workflow-digest check.

```text
completed local run
  RunLock + frozen workflow + ExecutionPlan
                    |
                    v
       separate fully pinned workflow
                    |
             user-controlled Git operation
                    |
                    v
     successful GitHub run + evidence ZIP
                    |
          read-only GitHub REST import
                    |
                    v
 source/event/workflow/jobs/steps/artifact all match?
          | no                         | yes
          v                            v
 preserve local result       classify from stored evidence
                                      |
                         github-confirmed only if the local
                         run was already hermetic/supported
```

Confirmation verifies the remote run conclusion, exact source commit, event,
workflow path and semantic digest, distinct successful job instances, ordered
successful steps, a unique unexpired artifact, the API-provided archive
digest, and exact canonical evidence bytes. Duplicate names cannot reuse one
remote result, and a user job cannot impersonate the reserved evidence job.
A matching GitHub pass cannot upgrade an unsupported, degraded, non-clean, or
non-hermetic local result.

The native Linux x86_64 CI path executes 20 unchanged warm runs and enforces
sandbox p95 below two seconds, workflow p95 below 30 seconds, and zero
Greenlit-controlled downloads. `tools/release-check` validates the optimized
binary and packages all publishable workspace crates together so their
unpublished path dependencies can be checked without publishing them. The
private `greenlit-init` crate remains excluded. The release workflow requires
an explicit `publish` input and the protected `release` environment; Phase 10
does not itself publish any artifact.

## Known issues log

Entries here describe upstream quirks that the implementation or dependency
policy actively contains. They are not deferred Greenlit behavior.

- **The pinned official ARC runner images are ordinary gzip OCI images, not
  eStargz artifacts.** The configured lazy provider therefore correctly uses
  the eager fallback for those default images. The live provider suite builds
  a pinned eStargz fixture, proves partial materialization and verified
  on-demand reads, and compares it with eager execution. Publishing
  Greenlit-maintained eStargz runner artifacts requires a separately
  authorized release channel; Greenlit never labels an ordinary gzip image as
  lazy or infers a smaller substitute.

- **`hashFiles('/…')` documentation differs from hosted-runner behavior.**
  The expressions reference describes `/src/*.js` as a repository-root
  pattern, but an observed run on hosted runner 2.336.0 returned an empty
  digest for `/src/*.js` and the expected digest for `src/*.js` against the
  same workspace file. Greenlit follows the observed toolkit behavior and
  treats the leading slash as filesystem-absolute, while its canonical
  workspace boundary prevents traversal or hashing outside the supplied
  workspace. Evidence: [parity run 29880046283](https://github.com/ShaneKanterman04/greenlit-app/actions/runs/29880046283),
  [GitHub expressions reference](https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#hashfiles),
  and [toolkit pattern rooting](https://github.com/actions/toolkit/blob/main/packages/glob/src/internal-pattern.ts).

- **Runner-compatible symbolic-link traversal can amplify a compact alias
  graph.** The pinned toolkit remembers canonical directories only in the
  active DFS chain, so two aliases per level can generate exponentially many
  visits before the runner's fixed deadline. Greenlit instead visits each
  canonical directory once per invocation: the first-discovered lexical alias
  preserves ordering and later aliases are omitted. The exact registry stops
  with corrective guidance at 262,144 identities or 32 MiB of identity
  storage, so ordinary wide-directory enumeration remains depth-streamed and
  the alias defense cannot itself grow without bound. This intentional
  security-over-parity rule keeps untrusted repository topology from
  exhausting host memory. Upstream context: [runner hashFiles timeout issue
  #1840](https://github.com/actions/runner/issues/1840) and the [pinned toolkit
  traversal](https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Misc/layoutbin/hashFiles/index.js).

- **A `hashFiles` worker stuck in an uninterruptible host syscall cannot be
  killed, so simultaneously live workers are bounded and die only with the
  process.** The caller-side supervisor returns on time by detaching a
  worker once its fixed deadline elapses, but detaching does not free
  anything: the thread, its held descriptors, and its filesystem handle all
  persist until the blocking call eventually returns, if it ever does.
  Killing that thread outright is not possible from within this library —
  only a process supervisor could do that, out of scope for the Phase 1
  engine-core crate. Greenlit instead bounds the damage with a real
  reservation: starting a worker atomically increments a fixed live-worker
  counter (a single `fetch_update` compare-and-swap) *before* the thread is
  spawned, and only a worker that has actually exited — whether it finished
  normally or was detached and is still finishing very late — decrements it.
  This bounds concurrently live workers outright, not merely the ones later
  discovered to be stuck (an earlier revision incremented the counter only
  once the caller detected a stranding, which left an unbounded window at
  call start where arbitrarily many concurrent calls could all start before
  any one of them was individually flagged). Once the fixed cap of
  simultaneously live workers is reserved, a new `hashFiles()` call fails
  fast with the corrective action (wait for concurrent evaluations to finish,
  or investigate the stalled workspace or `$HOME` filesystem) instead of
  starting another thread. The cap is kept small — concurrent `hashFiles()`
  evaluations are rare during planning — so a burst of them cannot spawn
  unbounded threads even when every one of them would otherwise complete
  quickly. This is a security/robustness containment, not parity — the
  runner's own model kills the whole child helper process on timeout, which
  this library cannot do to itself. Upstream pattern: [runner hashFiles
  timeout issue #1840](https://github.com/actions/runner/issues/1840).

- **Contained `hashFiles` traversal does not cross mount points.** Each
  descriptor-relative lookup uses Linux `RESOLVE_BENEATH`,
  `RESOLVE_NO_MAGICLINKS`, and `RESOLVE_NO_XDEV`. The last flag deliberately
  rejects a bind mount or nested pseudo-filesystem inside an untrusted checkout
  even though its lexical path is below the workspace. This security-over-
  parity restriction prevents repository topology from exposing host devices
  or procfs through `hashFiles`; the single fix on kernels lacking `openat2` is
  to upgrade to Linux 5.6 or newer. Ordinary relative links and absolute links
  whose spelling is beneath the canonical workspace remain supported.

- **A matched entry that vanishes between enumeration and access is treated by
  its kind, not uniformly.** A committed *symbolic link* whose target cannot be
  resolved fails the whole expression (`DanglingSymlink`), matching GitHub's
  pinned hash helper, whose `statSync`/`readFile` of the followed target errors
  out. A *non-symlink* object (a plain matched file, or an ancestor directory)
  that is enumerated but then returns `ENOENT` on the next access is instead
  skipped silently, exactly like the search root's own vanished-before-traversal
  case. The asymmetry is deliberate: the rename-escape containment re-resolves
  every access from the pinned root, so an ancestor renamed away — by an
  attacker probing the boundary or by ordinary concurrent build churn — surfaces
  as `ENOENT`; hard-failing on that would turn the containment mechanism itself
  into a reliable "abort this workflow" oracle. The one case GitHub's contract
  meaningfully covers, a stably-committed dangling symlink, is already the hard
  failure; a transient vanish is racy on GitHub's own hosted runner too and is
  not a fidelity target. Reference: [pinned hashFiles helper](https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Misc/expressionFunc/hashFiles/src/hashFiles.ts).

- **The `hashFiles` workspace root must be a physical path.** Root
  establishment deliberately uses one `openat2` lookup with
  `RESOLVE_NO_SYMLINKS` and `RESOLVE_NO_MAGICLINKS`; any symbolic-link
  component is rejected with the action to supply the physical directory
  path. This security-over-parity rule closes the namespace race inherent in
  resolving an alias and opening its target in separate operations. The root
  lookup does not use `RESOLVE_NO_XDEV`, so physical workspaces on ordinary
  mounted filesystems remain supported. Once pinned, in-workspace symbolic
  links retain the separately documented contained behavior.

- **Newtonsoft's identifier categories are pinned to .NET 8's Unicode data.**
  Legacy `fromJSON` accepts unquoted object keys by applying .NET
  `Char.IsLetterOrDigit` to UTF-16 code units. `unicode-general-category`
  0.6.0 supplies the matching Unicode 15 category table; newer crate releases
  track newer Unicode versions and would change accepted input. The dependency
  remains intentionally pinned until the runner moves its .NET baseline.
  Upstream: [`unicode-general-category` repository](https://github.com/yeslogic/unicode-general-category)
  and [.NET 8 breaking-change catalog](https://learn.microsoft.com/en-us/dotnet/core/compatibility/8.0).

- **Saphyr's high-level loader accepts duplicate mapping keys.** GitHub
  workflow input must reject duplicates with their source locations, while
  `saphyr::YamlLoader` overwrites an earlier key. `greenlit-workflow` therefore
  consumes `saphyr-parser`'s spanned event stream directly and builds its own
  small raw tree, where duplicate detection occurs before typed conversion.
  The parser-specific code is confined to `yaml::raw`. Upstream tracking:
  [saphyr-rs/saphyr#116](https://github.com/saphyr-rs/saphyr/issues/116).

- **The lockfile spans the `syn` 2-to-3 proc-macro transition.** `serde_derive`
  and `thiserror-impl` use `syn` 3, while current transitive proc-macro paths
  through `clap_derive`, `tracing-attributes`, and Criterion's support graph
  still use `syn` 2. A workspace-level dependency change cannot unify those
  incompatible transitive requirements. `cargo-deny` therefore denies every
  duplicate version except the exact locked `syn@2.0.119` entry; pinning the
  exception forces review whenever that old line changes. Upstream migration
  tracking includes [tokio-rs/tracing#3582](https://github.com/tokio-rs/tracing/issues/3582)
  and [clap-rs/clap#4824](https://github.com/clap-rs/clap/issues/4824).

- **Phase 3's first outbound HTTPS clients pull in the Rust TLS ecosystem's
  own license/version footprint.** `greenlit-actions` (`resolve::GitHubApiResolver`,
  `store::TarballFetcher`) was the first crate in this workspace to make a
  real HTTPS request (to `api.github.com`), via `ureq`'s `rustls` backend —
  chosen over `native-tls` specifically because `native-tls` links system
  OpenSSL, which would turn the shipped `litci` binary from a single static
  executable into one with a host OpenSSL runtime dependency, contradicting
  `greenlit-v0-spec.md` "Tech": "One distributable static host binary."
  `greenlit-app` reuses the identical `ureq`/`rustls` choice for the same
  reason: `litci auth`'s device-flow/refresh client
  (`auth::device_flow::DeviceFlowClient`, `auth::refresh`) talks to
  `github.com`, and the authenticated configuration-variables lookup
  (`vars::remote::VariablesClient`) talks to `api.github.com`. Both crates
  resolve to the one pinned `ureq` version, so this remains a single
  dependency-tree footprint, not a duplicate. `rustls`'s dependency chain
  (`ring`, `rustls-webpki`, `ring`'s own `untrusted`, and `ring`'s `subtle`)
  is ISC- or BSD-3-Clause-licensed, and its bundled Mozilla root CA list
  (`webpki-roots`) ships under CDLA-Permissive-2.0 — all still standard
  OSI-approved/permissive terms, just outside this workspace's original
  MIT/Apache-2.0/Unicode-3.0 allowlist, which predates any crate needing
  outbound TLS. `deny.toml`'s `[licenses] allow` list was extended to admit
  exactly these three, with the reasoning recorded inline there. There is no
  rustls crypto provider or comparably minimal pure-Rust HTTPS client
  available that avoids this footprint.

- **`litci auth`'s token store chose the Linux kernel keyring over the
  cross-platform `keyring` crate, trading persistence guarantees for a
  smaller, daemon-free dependency.** `auth::token_store` uses
  [`linux-keyutils`](https://docs.rs/linux-keyutils) directly against the
  kernel's per-UID *persistent* keyring (`keyrings(7)`,
  `KEYCTL_GET_PERSISTENT`) rather than the `keyring` crate, whose only
  daemon-free Linux backend is that same `linux-keyutils` crate behind a
  non-default feature — its *default* Linux backend is a D-Bus Secret
  Service client (`zbus`), pulling a full async D-Bus stack and requiring a
  running `gnome-keyring`/`kwalletd` session, commonly absent on headless
  dev boxes, containers, and CI runners `litci auth` must not depend on. The
  trade-off this creates: the kernel keyring is in-memory, not disk-resident
  (a host reboot clears it, same as any session credential cache) and the
  kernel expires it automatically after
  `/proc/sys/kernel/keys/persistent_keyring_expiry` seconds of disuse. Both
  cases surface as ordinary "not authenticated" (`auth::current_token`
  returning `None`), which every caller already handles by pointing at
  `litci auth` again — no special-case recovery path exists or is needed.
  The documented `0600` file fallback under `~/.litci/auth.json` (with a
  printed warning) covers the same "kernel keyring unavailable" case a
  cross-platform crate's D-Bus backend failure would otherwise leave
  unhandled. Compiled-binary integration tests force this same file-only
  path via the internal `LITCI_TEST_NO_KEYRING` variable (`tests/support`'s
  harness sets it for every sandboxed invocation) rather than exercising the
  real kernel keyring, which is scoped to the test-runner process's UID and
  so is not sandboxable the way a temporary `$HOME` is; the keyring code
  path itself is instead covered by a unit test scoped to the thread-private
  keyring identifier, which never touches persistent kernel state.

- **`ring`'s own transitive pins duplicate two already-present crates at
  older versions.** `ring` (via `rustls`, via `ureq`) depends on
  `getrandom@0.2.17` and (through `rustls-webpki`) `windows-sys@0.52.0`,
  while the rest of the workspace already resolves to `getrandom@0.4.3` and
  `windows-sys@0.61.2` respectively (the latter is a Windows-only
  conditional dependency that never compiles into Greenlit's Linux x86_64
  binary at all). This workspace does not control `ring`'s own `Cargo.toml`
  version requirements, so the two old-version entries are named exactly in
  `deny.toml`'s `bans.skip`, mirroring the existing `syn@2.0.119` exception's
  granularity — a version bump on either side requires deliberately updating
  the pinned skip entry, not a silent pass.

- **`uses:` execution is pinned against one `actions/runner` release,
  independent of Phase 4's runner-image manifest.** Node-action execution
  (node20/node24), composite-action nesting, Docker actions, and the pre/post
  step protocol are all implemented by directly reading the pinned
  `actions/runner` **v2.336.0** source
  (<https://github.com/actions/runner/releases/tag/v2.336.0>) rather than
  reverse-engineering behavior from observed hosted-runner output, per
  `PHASE-3-actions.md`'s explicit "do not depend on Phase 4's runner-image
  manifest for these" instruction. The four pinned Node runtime bundles
  (`crate::executor::actions::node_runtime`, `greenlit-runtime`) come from
  that release's own `src/Misc/externals.sh` packaging script, which declares
  `NODE20_VERSION=20.20.2`/`NODE24_VERSION=24.18.0` and fetches a standard
  (glibc) `linux-x64` tarball from `nodejs.org/dist` plus an Alpine (musl)
  tarball from the separate `actions/alpine_nodejs` release for each version.
  The standard tarballs' checksums come from nodejs.org's own published
  `SHASUMS256.txt` for each version; the Alpine tarballs are release assets
  nodejs.org does not publish a checksum for at all, so their `sha256` was
  computed directly from the downloaded release asset during implementation
  (the asset was fetched, hashed, and discarded — not taken from any
  third-party republication). All four are re-verified against their live
  sources as part of this work:

  | version | variant | url | sha256 |
  |---|---|---|---|
  | node20 | standard | `https://nodejs.org/dist/v20.20.2/node-v20.20.2-linux-x64.tar.gz` | `19e56f0825510207dd904f087fe52faa0a4eb6b2aab5f0ea7a33830d04888b8b` |
  | node20 | alpine | `https://github.com/actions/alpine_nodejs/releases/download/v20.20.2/node-v20.20.2-alpine-x64.tar.gz` | `f21a2253025a5d1a14332a0b1ed48871689c5ca9aa37a6141428944b75de7d91` |
  | node24 | standard | `https://nodejs.org/dist/v24.18.0/node-v24.18.0-linux-x64.tar.gz` | `783130984963db7ba9cbd01089eaf2c2efb055c7c1693c943174b967b3050cb8` |
  | node24 | alpine | `https://github.com/actions/alpine_nodejs/releases/download/v24.18.0/node-v24.18.0-alpine-x64.tar.gz` | `0103dd81376d57dcc2bcb39a13cfd6db19ab82f6c2c83a166e44d775f736d0d9` |

  A job container's libc is not known until it is booted, so `RuntimeStore`
  fetches and caches *both* variants a job needs (content-addressed under
  `~/.litci/node-runtimes/<node20\|node24>/<standard\|alpine>/<sha256>/`) and
  a per-job in-container probe (`cat /etc/*release | grep ^ID`, checked for
  `alpine`) selects which bind-mount to expose as the executable path at exec
  time — never both mounted live at once, and never a guess made from the
  requested image name, which is not a reliable libc signal. Pre/post-step
  ordering (pre in file order before a job's first step, post in *reverse*
  push order at job end regardless of failure) and the `pre-if`/`post-if`
  default-to-`always()` rule are taken from the same pinned release's
  `JobExtension.cs`/`ExecutionContext.cs`/`StepsRunner.cs` (`PostJobSteps` is
  literally a `Stack<IStep>`, which is why this crate's own `PostChain` is
  push/reverse-drain) and `ActionManifestManager.cs`. Libc-family detection
  as the variant-selection signal follows the same release's `StepHost.cs`.
  Composite-action nesting depth is capped at 10, matching
  `docs/adrs/1144-composite-actions.md`.

- **A Docker action's sibling container shares the job's *live* workspace
  through a run-scoped named volume, not a bind mount of the job container's
  own view.** The job container's workspace is not necessarily a host
  directory at all — under overlay isolation it is a private overlay upper
  layer inside that one container, invisible to any sibling. Docker actions
  must nonetheless run as a sibling container (never sharing the host Docker
  socket with the job container, and never running inside it), per
  `PHASE-3-actions.md`. The resolution: a job plan that contains any Docker
  `uses:` step forces that job onto `IsolationStrategy::CopyIn` (rather than
  overlay) and binds one namespaced, run-scoped named Docker volume
  (`crate::executor::container::namespaced_volume_name`, the same
  namespacing already used for other job volumes) at the workspace path in
  *both* the job container and every Docker-action sibling for that job. A
  copy-in job's workspace is, by construction, whatever is bind-mounted at
  that path — so pointing that mount at a shared named volume instead of a
  bind mount costs nothing beyond what copy-in isolation already does, and
  requires no change to `greenlit-init`. Writes made by a `run:` step and by
  a Docker-action sibling are therefore mutually visible for the remainder of
  the job, proven end-to-end by
  `crates/greenlit-runtime/tests/actions_docker.rs` against the real Docker
  daemon (a `run:` step writes a file, a Docker action reads and appends to
  it, and a later `run:` step reads back both halves). The tradeoff: a job
  with a Docker action cannot use overlay isolation even if it would
  otherwise qualify. **Phase 4 closed the cleanup half of this entry**: the
  port gained `remove_volume`, and the run-scoped volume is now removed after
  the job container's own teardown (it has to follow, because a volume still
  bound by a running container cannot be removed). Before that it leaked one
  volume per run until an operator pruned by hand.

- **`actions/checkout` of the workflow's own repository is satisfied from the
  already-materialized isolated workspace, with no network access; checking
  out a *different* repository always performs a real, authenticated
  clone.** `crate::executor::actions::checkout` special-cases exactly the
  self-checkout shape (`with.repository` absent, or equal to the synthetic
  event's own `owner/repo`) by treating the workspace already prepared by
  `greenlit-init` as the checkout result outright and reading `ref`/`commit`
  outputs from the already-known `RunnerEnv`, rather than re-cloning content
  litci already isolated onto disk for the job. Checking out any other
  repository is a real `git clone` executed inside the job container, using
  a short-lived `http.https://github.com/.extraheader` basic-auth header
  (the token is never embedded in the remote URL, so it cannot leak through
  a logged `git remote -v` or similar) built from `litci auth`'s stored
  token; with no token available, this fails immediately with the same
  `litci auth` fix-it message used elsewhere, rather than silently attempting
  an anonymous clone. `checkout`'s post step (credential cleanup, removing
  the injected `extraheader` config) still appears in the post chain for
  *both* shapes — a documented no-op for the self-checkout case, since there
  is no injected credential to remove there, kept honest rather than
  omitted, so the post-step *ordering* contract (exit criterion: checkout's
  post step still runs even when a later step fails) holds identically
  regardless of which shape triggered it.

- **A composite action's nested `pre` steps run immediately before that
  action's own nested main steps, not hoisted to the job's front alongside
  ordinary top-level `pre` steps.** The pinned runner hoists every *top-level*
  step's `pre` action to the front of the job and its `post` action to a
  job-end stack, but a composite action's own nested steps are, at that
  level, a single unit (the composite's containing `uses:` step is itself
  what gets hoisted/stacked as one participant). This implementation
  deliberately simplifies one layer further: a nested step's `pre` runs in
  place, immediately before that same nested step's main action, rather than
  additionally hoisting it to the outer job's front. This is a documented
  fidelity gap, not an oversight — real-world composite actions rarely rely
  on nested pre-step hoisting timing, and getting it exactly right would
  require threading hoisted nested pre steps back out through the same
  `JobActionPlan` pre-pass that already handles top-level steps, which was
  not attempted this wave. Nested `post` steps are *not* similarly
  simplified: they still push onto the job's shared, reverse-drained
  `PostChain` and so run at job end in the correct overall LIFO position
  alongside every other step's post action. Nested `uses:` steps of *every*
  kind now execute — originally `actions/checkout` and Docker actions nested
  inside a composite were rejected outright; the action-fidelity wave closed
  that. Each nested kind dispatches through the same execution module its
  top-level counterpart does rather than a parallel implementation: a nested
  checkout runs `crate::executor::actions::checkout` and pushes its
  credential-cleanup post entry onto the same job-wide `PostChain`, and a
  nested Docker action runs `crate::executor::actions::docker_action`
  against the job's shared sibling volumes, which
  `crate::executor::actions::resolve`'s pre-pass provisions whenever a
  Docker action exists at *any* nesting depth — so the sibling apparatus is
  already in place by the time composite recursion reaches it. Both are
  pinned by `crates/greenlit-runtime/tests/actions_composite.rs`.

- **A manifest's declared input `default:` value is used as a literal
  string, not evaluated as a `${{ }}` expression.** `actions/checkout`'s own
  manifest and the marketplace actions this implementation was validated
  against only use literal default values (`default: true`, `default: '1'`),
  so this is not a fidelity gap for any action exercised here — but the
  pinned runner's `ActionManifestManager.cs` does generically evaluate a
  default value as an expression before substitution, which a marketplace
  action relying on a non-literal, expression-valued `default:` would
  observe differently under this implementation (the raw text would be
  passed through, rather than evaluated). Recorded here as a known,
  narrow fidelity gap rather than left silent. **Closed by the
  action-fidelity wave**, for both action kinds that declare inputs at all:
  `nodejs::input_env` already evaluated a JS action's default (Phase 3);
  `composite::composite_inputs_value` now does the same for a composite's
  own declared inputs, evaluating each `default:` as a `${{ }}` template
  against the *enclosing* scope that resolved the invoking step's `with:` —
  matching `ActionManifestManager.EvaluateDefaultInput`, which runs against
  that same invoking step's `IExecutionContext`. A nested composite's
  defaults get that enclosing scope with full fidelity (the parent
  composite's own already-built context, threaded straight through); a
  *top-level* composite step's defaults fall back to a reconstructed
  context assembled from what this crate's executor actually keeps on hand
  for it (`composite::fallback_caller_context`) — real `github`/`vars`/
  `secrets`/`env`/`needs`/`status`, but an empty `steps` context, since
  nothing threads the job's `StepRecord` history into composite execution
  today. A top-level composite input default referencing `steps.*` is
  therefore still a narrower, separately-recorded gap rather than a revived
  version of this one. Pinned by
  `crates/greenlit-runtime/tests/actions_composite.rs`.

- **`GITHUB_TOKEN`'s reserved-name rule is reused, not special-cased, to keep
  it out of the ordinary secrets chain.** GitHub secret and configuration
  variable names share one documented rule: "must not start with the
  `GITHUB_` prefix"
  (<https://docs.github.com/en/actions/reference/security/secrets>,
  <https://docs.github.com/en/actions/reference/workflows-and-actions/variables#naming-conventions-for-configuration-variables>).
  A real repository can therefore never hold a user-created secret literally
  named `GITHUB_TOKEN` — it is always the platform-populated token instead.
  `crate::secrets::validate_name` (`crate::gh_names::validate_configuration_name`)
  already rejects that exact name for the same reason it rejects any other
  `GITHUB_`-prefixed one, so `crate::secrets`'s ordinary chain excludes
  `GITHUB_TOKEN` from its candidate set outright rather than letting it reach
  (and fail) that validator; `crate::auth::resolve_github_token` is its own,
  separate resolution path (local override, if any, else the stored auth
  token, else the empty string — never a hard failure, since GitHub itself
  always provides a working token and a workflow that merely references it
  should not be blocked from an unauthenticated local run). `github.token`
  gets the identical treatment via a small, additive
  `greenlit_workflow::extract::StaticExtraction::references_github_token`
  flag (`extract::walk`), since — unlike `secrets.GITHUB_TOKEN` — no existing
  Phase 1 extraction tracked a bare `github.*` field access at all.

- **Every step's layered environment needs an explicit, tracked `PATH` from
  job start — ordinary Docker `exec` inheritance alone stops being enough
  the moment any step writes to `GITHUB_PATH`.** Before this wave, no
  environment layer (`greenlit_engine::execution::env::RunnerEnv::into_map`,
  the job/workflow `env:` layers, `GITHUB_ENV` accumulation) ever carried an
  explicit `PATH` key at all — the container's own image-configured default
  reached every step only through ordinary Docker `exec` environment
  inheritance, invisible to Greenlit's own `IndexMap`-based layering. That
  works for a job's *first* step, but `apply_path_additions`
  (`greenlit_engine::execution::env`) documents its contract as *prepending*
  onto whatever `PATH` the layered map already carries — and the instant one step
  calls `core.addPath()` (`actions/setup-node` and most `setup-*` actions
  do), every *later* step's `exec` starts carrying an explicit
  `PATH=<additions>` entry, which Docker's exec-env merge applies key-for-key
  over the container's live environment, silently dropping `/usr/bin` and
  friends for the rest of the job — a real defect this wave found (and
  fixed) building `fixtures/actions-ci`, invisible until then because no
  earlier test ran a real `setup-*` action followed by another real step
  against a live daemon. GitHub's own runner never has this gap: its
  `ExecutionContext` seeds one explicit, always-current `PATH` value from the
  runner process's own environment at job start
  (`src/Runner.Worker/FileCommandManager.cs`'s `AddPathFileCommand` only ever
  prepends onto it — `actions/runner` v2.336.0, pinned release, see the
  Node-runtime entry above). `crate::executor::job::seed_container_path`
  reproduces that: one `sh -c 'printf %s "$PATH"'` exec against the freshly
  booted (and ready) job container, seeded into `base_env` before the step
  loop starts — container-agnostic, so it works identically for the
  locked runner profile and an arbitrary user-specified
  `jobs.<id>.container`. `crates/greenlit-runtime/tests/actions_composite.rs`
  pins the regression (a `run:` step's own `GITHUB_PATH` addition must not
  break a later composite step's access to system binaries).

- **A composite step's own environment must be the same job-wide
  base/workflow/job layers an ordinary top-level step sees, not just its own
  `GITHUB_ENV` accumulation.** `crate::executor::actions::composite`'s own
  module docs already state the intended rule ("env: accumulation is
  job-wide, not composite-scoped"), but before this wave the implementation
  only threaded the job's *accumulated* `GITHUB_ENV` writes into a nested
  step's environment (and its `${{ env.* }}` context) — literal
  workflow-level and job-level `env:` blocks, and the `base_env` fix above,
  were never visible inside a composite's nested steps at all. Fixed by
  threading `base_env`/`workflow_env`/`job_env` through `CompositeEnv`
  (mirroring `crate::executor::step::layered_env` exactly) rather than
  seeding nested execution from an empty map. A second, unrelated bug in the
  same code path — a nested step's shell was resolved against
  `{cmdfiles_dir}/script`, one directory short of where
  `CommandFilePaths::new` actually places it
  (`{cmdfiles_dir}/step-<n>/script`) — meant *every* composite `run:` step
  failed outright ("no such file") before this wave, regardless of the env
  gap; no prior test had ever executed a composite action's nested `run:`
  step against a real daemon. Both are pinned directly by
  `crates/greenlit-runtime/tests/actions_composite.rs`.

- **A Docker action's sibling container does not receive the
  `GITHUB_ENV`/`GITHUB_OUTPUT`/`GITHUB_PATH`/`GITHUB_STEP_SUMMARY`
  command-file protocol every other step kind gets.** GitHub's real runner
  exposes these uniformly to *any* step, including a Docker container
  action: `Runner.Worker/FileCommandManager.cs`'s `InitializeFiles` runs
  once per step regardless of handler type, writing each file's
  (container-translated) path into the step's `github` context before
  dispatch (`actions/runner` v2.336.0, pinned release). This
  implementation's `crate::executor::actions::docker_action::execute` passes
  `INPUT_<NAME>` env vars (via the same `nodejs::input_env` every action kind
  uses) but never creates or mounts the four command files, so a
  marketplace Docker action that writes to `$GITHUB_OUTPUT`/`$GITHUB_ENV`
  either silently loses that write or (if it does not tolerate an unset
  variable) fails outright. **Closed by the action-fidelity wave.** The
  Phase-3 entry here predicted the fix would put the command files inside
  the job's shared workspace volume; the wave deliberately did not — command
  files under `GITHUB_WORKSPACE` would be visible to `git status`, matched
  by a `hashFiles('**')` pattern, and swept into an `upload-artifact` or
  `actions/cache` archive, and the workspace has to stay exactly what the
  workflow checked out. Instead a *second* run-scoped named volume is
  mounted at the same `CMDFILES_BASE` path in the job container and in
  every sibling, so `crate::executor::cmdfiles` is reused unchanged (the
  paths it materializes through the job container resolve to the same bytes
  in the sibling) — the same shape as GitHub's own runner, which mounts its
  `_runner_file_commands` directory into every container action.
  `cmdfiles::open_to_sibling` then widens the step's files (`0777` dir,
  `0666` files) so an action image running as a non-root `USER` can append;
  the widening is scoped to a run-private volume only this run's containers
  mount. Effects are collected even when the action exits non-zero, matching
  GitHub's handling of a failed step's files, and every step kind now folds
  them through one function, `cmdfiles::apply_effects` — closing, in
  passing, a latent drop where a *nested* JS action's `GITHUB_ENV`/
  `GITHUB_PATH` writes were discarded instead of accumulated. Pinned by
  `crates/greenlit-runtime/tests/actions_docker.rs` with a `USER
  65534:65534` Dockerfile action writing all four files.

- **The network policy is enforced inside each container's own namespace, not
  by host `DOCKER-USER` rules.** `PHASE-4-environment.md` names the host
  chain, but Docker exposes no API for it, so honoring that literally would
  mean shelling out to `iptables` under `sudo` on every `litci run` — against
  the spec's "zero prerequisites" and the never-shell-out rule. Greenlit
  instead starts a short-lived sidecar in the workflow container's own network
  namespace with `CAP_NET_ADMIN`, installs the rules there, and lets it exit;
  the rules persist for the namespace's life and the capability does not. This
  is stronger than the host chain rather than merely equivalent — the workflow
  container never holds `NET_ADMIN`, so code inside it cannot remove the rules
  binding it, verified by a root-in-container `iptables -F` failing with
  "Permission denied" while the drop stayed in force.

- **A Docker action's sibling container joins the job container's network
  namespace rather than getting a network of its own.** The sibling
  originally ran on Docker's default bridge, which could not resolve a
  `services:` container by hostname and, worse, sat entirely outside the
  network policy above — that policy is installed into a specific namespace
  and a default-bridge sibling was never the namespace it was pointed at.
  The fix reuses the netguard sidecar's own mechanism: the sibling's spec now
  sets `network` to `container:<job-container-id>`, the same
  `--network container:<id>` form netguard uses to bind its rules into a
  namespace. Joining that namespace, rather than attaching to the job's
  bridge network as a second member, buys two things from one line: the
  sibling is guarded from its first instruction (the rules were installed
  into that namespace before the sibling ever started, so there is no
  per-step sidecar run and no race), and it resolves a service's id for free,
  since Docker's embedded DNS is owned by the network namespace, not by the
  container. The tradeoff is stated in the module docs and confined to
  something no workflow can observe: on GitHub's own runner a Docker
  `uses:` action gets its own IP on the job's network, with its own
  loopback; here the sibling shares the job container's namespace outright,
  so `127.0.0.1` inside the action is the job container's loopback rather
  than one of its own. Every fidelity-relevant behavior — service-id
  reachability, internet access, and the LAN/metadata block — is identical
  either way. Pinned by
  `crates/greenlit-runtime/tests/actions_docker.rs`'s services-and-guard
  test.

- **Blob URLs authorize themselves; the bearer token does not reach them.**
  `@azure/storage-blob` treats a signed URL as self-authorizing and sends no
  `Authorization` header, and `actions/cache` fetches its `archiveLocation`
  with a bare `HttpClient`. A shim that requires a bearer header on those
  routes returns 401, which the Azure SDK surfaces as a `RestError` with an
  **empty** message and which `actions/cache` reports as an ordinary cache
  miss — two failure modes that both hide the cause. Greenlit therefore does
  what the real service does and puts a per-run signature in the URL, kept
  distinct from the bearer token so the token never lands in a URL a client
  might log (`actions/cache` calls `setSecret` on the download URL for exactly
  that reason). Evidence: reproducing the upload host-side against
  `crates/greenlit-store/examples/shim_probe.rs` and printing `error.stack`
  rather than `error.message`.

- **The artifact twirp client sends snake_case and parses camelCase.** Requests
  are serialized with `useProtoFieldName: true`; responses are parsed without
  it, so protobuf-JSON's default lowerCamelCase applies. Because the same call
  passes `ignoreUnknownFields`, a snake_case *response* is not rejected but
  silently ignored, leaving the field at its default — an empty
  `signedUploadUrl` that fails much later and elsewhere. Pinned by
  `crates/greenlit-store/tests/artifact_shim.rs`.

- **Provisioned apt packages carry no pinned version.** The runner-images
  toolset manifest lists a package *set* with no versions, so a provisioned
  apt package lands at whatever the Ubuntu archive currently offers. That is
  what the hosted runner gets from the same archive, but it is weaker than
  `PHASE-4-environment.md`'s "at the exact versions listed", which genuinely
  holds only for tools the manifest versions explicitly. The distinction is
  preserved in `Recipe::pinned_version` and surfaced in the install log as
  `distribution default` rather than papered over with an invented number.

- **Docker bind sources must outlive container creation and startup.** A CI
  run observed a helper written under the runner-managed temporary directory
  disappear after Docker accepted the container specification but before
  `runc` started it. Docker's bind syntax then materialized the absent source
  as a directory, so `/greenlit/bin/greenlit-init` failed with `is a
  directory: permission denied`. Greenlit now publishes the embedded helper
  atomically under `~/.litci/runtime`, keyed and verified by SHA-256. A corrupt
  regular file is atomically replaced; a non-file at that immutable identity
  fails closed with an actionable error.

## Phase 11 event boundary

`greenlit-runtime` now exposes two presentation-neutral ports beside
preparation progress: `ExecutionEventSink` receives typed job/step
transitions, while `RunLogSink` receives already-masked bytes with a concrete
job scope. The compatibility executor adapts a flat writer; the CLI uses the
typed entrypoint.

`greenlit-app::run_events` serializes both ports into one sequenced
`events.ndjson` journal and then projects each record as compact text or exact
JSONL. Workflow bytes can therefore create only `log` events: they cannot
forge a job header, step conclusion, cache observation, or result badge.
Journal records are made durable before successful result evidence is
published and again after the terminal event.

The compact renderer holds only a per-step failure tail (200 lines and 256
KiB caps). Successful bodies remain solely in the durable journal unless
`--log-mode full` is selected. `litci logs` is a read-only projector over that
journal and never reconstructs lifecycle state from text.
