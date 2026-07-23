# ARCHITECTURE.md — Greenlit

This document records the architecture implemented in Phase 1. It is updated
with each phase summary; later-phase crates and runtime paths are intentionally
absent until they exist.

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

## Known issues log

Entries here describe upstream quirks that the implementation or dependency
policy actively contains. They are not deferred Greenlit behavior.

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
