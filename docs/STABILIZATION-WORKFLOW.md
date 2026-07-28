# Greenlit stabilization workflow

## Execution directive

This document is the execution plan for restoring Greenlit from workflow parsing
through trustworthy terminal completion.

- Run the head agent and delegated agents with `gpt-5.6-sol` at `ultra`
  reasoning effort.
- Work through exactly one numbered stabilization phase at a time. Do not
  implement, prepare, or partially land work owned by a later phase.
- Treat Phases 1–11 as historical, not as evidence that the current product is
  release-ready.
- Keep public package, image, dashboard, and launch publication blocked. Those
  actions still require the owner authorizations listed in `AGENTS.md`.
- Preserve all pre-existing user changes, including the current uncommitted
  `README.md` change.

### Head-agent responsibilities

The head agent owns sequencing, decomposition, integration, verification,
commits, phase summaries, status tables, and the final release-readiness
decision.

At workflow startup:

1. Read `AGENTS.md`, `greenlit-v0-spec.md`, `TESTING.md`, this workflow, and the
   active phase brief.
2. Inspect the worktree and preserve unrelated user changes.
3. Add the stabilization phase rows to the status table, create only the Phase
   12 brief in full, and mark only Phase 12 in progress.
4. Create the stabilization and parity-exception ledgers described below.
5. Publish progress through the installed `hark-ops` skill.

For each subsequent transition, create the next detailed phase brief from this
workflow only after the current phase is complete. Worker agents must receive
exactly the four documents required by `AGENTS.md`: `AGENTS.md`, the v0 spec,
`TESTING.md`, and the active phase brief. Do not ask workers to read future
phase briefs or this whole workflow.

### Subagent operating model

Use subagents proactively, normally four to eight per phase and never more than
the available concurrency limit.

- Spawn agents with `model: gpt-5.6-sol`, `reasoning_effort: ultra`, and a
  bounded context fork. Give each agent one concrete component boundary.
- Begin a phase with read-only behavior/oracle, security/invariant, and
  lifecycle/failure-mode reviews. Do not repeat the original broad audit.
- Consolidate findings into the stabilization ledger before implementation.
- Assign implementation agents exclusive files or non-overlapping modules.
  Because every agent shares one worktree, two agents must never edit the same
  file concurrently.
- The head agent owns cross-cutting schemas, shared interfaces, dependency
  changes, status documents, commits, and conflict resolution.
- After implementation, use independent read-only verification agents for
  parity, security, TESTING.md compliance, and phase exit-criteria review.
- Subagent completion is advisory. The head agent must inspect the diff and run
  every verification command itself.

### Per-phase execution loop

1. Mark exactly one phase in progress and inventory its open ledger entries.
2. Add or correct behavior-level reproductions in the single test class
   authorized by `TESTING.md`; verify that each new defect case fails for the
   intended reason before fixing it.
3. Implement only the active component. Keep downstream capabilities
   quarantined.
4. Run the component-specific local, compiled-Greenlit, GitHub Actions, and
   comparison gates described below where the component has a GitHub-observable
   equivalent.
5. Run the complete cumulative pipeline and all earlier component gates.
6. Perform an independent security/parity review and a head-agent line-by-line
   reread of the active phase brief.
7. Update `ARCHITECTURE.md`, the phase summary, the status table, dependency
   notes, tests added/deleted, and ledger rows. Record resolution commits; never
   delete historical ledger rows.
8. Make conventional commits, complete the phase PR under the repository
   policy, and activate the next phase only after every exit criterion passes.

Stop and request owner direction when work requires a new owner-provided input,
an in-scope parity mismatch would need a specification change, a parity
exception needs approval, or an external publication action is reached.

## Summary

The current green suite is not a release signal. The audit found false-green
paths, plaintext token persistence, fabricated JobLocks, unsafe content
handling, late network containment, action lifecycle failures, resource leaks,
and conflicting terminal evidence.

Phases 1–11 remain historical, but Greenlit is release-blocked. Add
stabilization Phases 12–28 and execute exactly one phase and one PR at a time.
Each phase must reproduce its owned defects, repair them, pass its component
gate plus every earlier gate, update architecture, summary, and status, and
leave zero open in-scope defects before the next phase activates.

Phase 12 precedes parsing because known security and evidence failures must be
contained immediately.

## Certification and comparison model

- Maintain a permanent stabilization ledger with defect ID, severity, owning
  phase, user-visible impact, authoritative test or oracle case, status, and
  resolving commit. Completed phases may have no open owned entries.
- Quarantine every uncertified capability. Default execution is `blocked`;
  `--allow-degraded` may run only explicitly forceable behavior and can never
  override a security finding or produce clean, hermetic, or GitHub-confirmed
  assurance.
- Disable export and confirmation until their owning phase passes. Ensure
  tokens and secrets cannot enter serialized plans, locks, events, or
  artifacts.
- Every observable component uses this fail-fast sequence:
  1. Local oracle, integration, invariant, and fault-injection CI.
  2. The compiled `litci` binary runs the committed component workflow.
  3. The same workflow and commit run through real GitHub Actions on the
     homelab runners.
  4. A canonical semantic comparator evaluates both observations.
- Use `homelab` for short metadata and oracle jobs and `homelab-heavy` for Rust
  builds, Docker, runtime, and long integration suites.
- Introduce `ParityObservationV1`, containing only contract-relevant outcomes,
  contexts, outputs, lifecycle ordering, filesystem probes, and
  resource/security findings. Normalize only schema-declared nondeterminism
  such as timestamps, durations, run IDs, temporary paths, and allocated
  ports.
- Maintain a separate parity-exception ledger. Each entry requires an exact
  case and field, authoritative source, reason, scope, owner approval and date,
  and removal criterion. Wildcards are forbidden. In-scope bugs cannot be
  waived; exceptions are limited to intentional, specification-permitted
  degradation or non-goals.
- Do not manufacture GitHub comparisons where no equivalent exists:
  - Invalid workflows that GitHub rejects before runner assignment use retained
    GitHub control-plane observations.
  - CAS, daemon, isolation, recovery, evidence integrity, and other
    Greenlit-specific internals use invariant, tamper, race, and fault-injection
    tests.
- Capability-owning CI jobs must provide their dependency or fail. Remove all
  capability self-skips and fake substitutes such as shell scripts pretending
  to be Node.

## Component order

### Phase 12 — Containment and test authority

- Add the stabilization and certification ledgers, checker, capability
  quarantine, `--allow-degraded`, canonical observation and comparison tooling,
  and one end-to-end seed oracle.
- Immediately prevent serialized credentials, enforce 0700 run directories
  and 0600 artifacts, hard-block unsafe credential, action, and DinD paths, and
  disable confirmation.
- Exit when known audit findings are assigned, no owning CI job self-skips,
  secret scanning covers entire run directories, and the four-stage comparison
  catches an intentional mismatch.

### Phase 13 — Workflow intake and frozen source

- Build one no-follow, regular-file, repository-contained loader shared by
  discovery, picker, plan, run, and daemon.
- Capture an internally consistent source and original Git identity, preserve
  modes and symlinks, derive dirty state from captured bytes, and execute only
  a digest-verified sealed tree.
- Gate symlink and race escapes, concurrent create, delete, rename, and mode
  changes, ignored workflows, post-lock tampering, custom upstreams, repo-local
  identity, and no-origin repositories.

### Phase 14 — YAML schema and typed workflow model

- Validate the entire document regardless of selected event. Unknown or
  malformed constructs fail closed.
- Replace lossy markers with spanned, lossless types for triggers, reusable
  workflows, environments, job controls, concurrency, containers, services,
  permissions, and every current-spec field.
- Use one exhaustive visitor for validation and expression-site inventory.
  Gate unknown triggers, cron and activity schemas, reusable calls, mapping
  keys, exact spans, and mixed valid and invalid constructs.

### Phase 15 — Expression language

- Match the pinned runner's lexer, parser, n-ary logical AST, depth and memory
  accounting, UTF-16 positions, radix values, coercion, ordinal comparison,
  JSON escaping, format validation, function laziness, and `jobs` context.
- Centralize evaluation modes so static and deferred workflow and action
  templates use identical budgets.
- Repair `hashFiles` pattern and containment semantics against the pinned
  toolkit while retaining Greenlit's stronger traversal bounds. Live-workspace
  binding is completed in Phase 22.

### Phase 16 — Events, contexts, trust, and input preflight

- Create one canonical event and GitHub identity shared by expressions,
  environment variables, checkout, filters, and evidence; never fabricate
  unavailable pull-request or event values.
- Compute selected-job reachability before support enforcement, prompts,
  secrets, variables, actions, or network work.
- Add source-located, reachability-aware support findings; one transactional
  auth session; trusted and fork classification; host-token separation;
  applicable repository and organization variables; and complete dynamic
  secret and token inventory.
- Gate hostile forks, missing and scoped tokens, pull-request identity, dynamic
  maps, variable visibility, auth corruption and rotation faults, non-GitHub
  origins, and offline zero-network behavior.

### Phase 17 — Planner and workflow control graph

- Implement recursive local and remote reusable-workflow composition through
  an immutable-source interface, including mappings, outputs, cycle and depth
  detection, and environments required by the current specification.
- Correct matrices, pre-matrix job conditions, `needs`, `strategy` and `job`
  contexts, concurrency, job-level `continue-on-error`, runner profiles,
  deferred runner and container identities, zero-leg behavior, and output
  contracts.
- Gate diamonds and cycles, selected closures, dynamic matrices and runners,
  skipped jobs with invalid deferred data, array and object partial evaluation,
  and stable `plan --json`.

### Phase 18 — Evidence identities and lifecycle contracts

- Replace the current schemas with canonical `RunLockV2`, `JobLockV2`,
  `RunResultV2`, `SupportReportV1`, and `RunBundleManifestV1`.
- Bind repository and event identity, selection, source, variables, plan,
  policy, runner and content identities, and keyed secret revisions. Provider
  revisions take precedence; otherwise use a protected machine-key HMAC.
- Digest-link all artifacts and publish the bundle manifest atomically last.
  Reject unsupported versions, mixed runs, duplicate terminal states, missing
  links, tampering, and premature JobLocks.
- Treat existing evidence as legacy-unverified; it can never be confirmed.

### Phase 19 — Immutable content, CAS, actions, and OCI resolution

- Build one transitive content graph covering action commit, tree, and subpath;
  reusable workflows; Node archive and tree; OCI index, manifest, config,
  layers, and TOCs; runner profiles; toolchains; source; and Docker build
  inputs.
- Enforce path and symlink containment, provenance, cross-process cancelable
  single-flight acquisition, transactional publication, restart-once
  mutable-ref handling, corruption recovery, exact offline replay, private
  authentication, and preparation-egress restrictions.
- Gate hostile action trees, large real API responses, arbitrary checkout
  commits, races and kills, moving refs, private actions and registries,
  individual missing or corrupt graph nodes, and cold to warm to offline reuse.

### Phase 20 — Engine, runner, and sandbox provisioning

- Extend the container-engine boundary with structured capabilities and errors,
  bounded cancellation, exact image identity, and idempotent resource
  operations.
- Make resource creation transactional; establish containment before untrusted
  entrypoints; use verified IPv4-only Greenlit networks or block; apply limits
  before start; use nonce-based readiness; support non-root images; and prove
  overlay, copy, eager, and lazy execution paths.
- Digest-lock DinD and isolate it on a separate authenticated network
  unavailable to services, with limits, netguard, and complete evidence.
- Gate forbidden IPv4 and IPv6 traffic, host socket and mount access, forged
  readiness, non-root boot, provider fallback, hung engine calls, and fault
  injection after every resource transition.

### Phase 21 — Scheduler and job lifecycle

- Replace global waves with a deterministic direct-needs readiness queue.
- Evaluate job conditions before matrix, concurrency, content, or resources;
  materialize dynamic identities after needs; create and consume exactly one
  concrete JobLock immediately before sandbox creation.
- Implement matrix active-sibling cancellation, global and project limits,
  machine-wide workflow and job concurrency with one running plus newest
  pending, crash-safe leases, and cancellation-preserving job and step results.
- Gate immediate readiness, cross-process concurrency, zero-resource skips,
  active and queued fail-fast, dynamic locks, output-finalization failures, and
  exact canceled and blocked outcomes.

### Phase 22 — Run-step semantics

- Repair shell, defaults, and working-directory behavior; environment
  precedence and Linux case sensitivity; protected variables; step conditions;
  timeouts; cancellation; `continue-on-error`; output limits; summaries; PATH
  ordering; log commands; and bounded command-file parsing.
- Bind `hashFiles` to the current job-private workspace so prior step edits and
  checkouts are visible without crossing job boundaries.
- Extend `shell-ci` and `matrix-needs` through the compiled binary and GitHub
  oracle, including exactly-once execution and background-process termination.

### Phase 23 — Action execution

- Implement genuine Node 20 and 24, composite, Docker, checkout, and
  `docker://` behavior with exact inputs, caller environment, action contexts,
  filesystem topology, pre, main, and post timing, LIFO cleanup, isolated state
  identity, command-file propagation, timeout, and cancellation semantics.
- Content-key Docker builds and honor Docker metadata, lifecycle, and
  environment precedence.
- Extend `actions-ci` and remove fake-Node coverage. Gate nested actions,
  failures, skips, cancellation, state separation, checkout ref and path
  restoration, local-action edits, corrupt Node runtimes, and cleanup failures.

### Phase 24 — Services, cache, artifacts, and workflow storage

- Complete service environment, credentials, ports, options, health checks,
  logs, secrets, collision-free naming, and teardown.
- Validate cache, artifact, and toolcache protocols; token failures; clean and
  hermetic cache policy; limits; concurrency; and cross-job isolation.
- Extend `full-ci` with private services, failed health checks, masked logs,
  cache cold, hit, and corruption cases, artifacts, DinD workloads, and
  teardown checks.

### Phase 25 — Daemon, recovery, and storage operations

- Make daemon and in-process preparation semantically identical; propagate
  offline, trust, and selection policies and keep prefetch performance-only.
- Finish durable resource reconciliation, CAS graph retention and unpinning,
  atomic GC, `clean`, and `doctor`, all respecting active runs and leases.
- Gate daemon and in-process equivalence, kill and restart at every durable
  boundary, download versus GC, clean versus active run, stale resources,
  corrupt catalogs, and zero network on unchanged warm runs.

### Phase 26 — Terminal events, results, logs, metrics, and write-back

- Drive every path from one terminal state machine: passed, failed, canceled,
  blocked, preparation-failed, or aborted. Emit exactly one durable terminal
  event and keep journal, result, trace, projection, exit status, and resources
  consistent.
- Bound and mask logs, summaries, service output, structured errors, and
  broken-writer paths. Preserve completed steps during cancellation.
- Complete local-only metrics and spans, read-only stats, terminal and PTY
  projections, and write-back lifecycle without telemetry or stale resources.

### Phase 27 — Verified consumers, export, and GitHub confirmation

- Make inspect, logs, doctor, export, and confirm accept only verified bundle
  states and report active, partial, corrupt, completed, and confirmed states
  explicitly.
- Replace textual export rewriting with parsed immutable-closure generation
  that pins every reusable workflow, nested action, Docker action, job
  container, and service.
- Match exact repository, ref, event, selection, locks, profiles, jobs, steps,
  and outcomes; paginate remote results; append confirmation evidence without
  rewriting the original result.
- Gate tampering, dirty and degraded rejection, remote mismatches, pagination,
  two-pass committed exports, and a real production `github-confirmed` run.

### Phase 28 — Whole-product certification

- Run the complete cumulative pipeline using a release-built binary: local CI,
  Greenlit dogfood, the same committed workflow on GitHub Actions homelab
  runners, and canonical comparison.
- Exercise cold, warm, offline, daemon, in-process, reflink, copy, eager, lazy,
  clean, hermetic, cancellation, crash recovery, and two consecutive dogfood
  runs.
- Enforce the warm-start and warm-rerun budgets, close every stabilization
  ledger row, validate every parity exception, and perform a final
  specification, phase, and architecture self-review.
- Establish release readiness only. Package, image, dashboard, and launch
  publication still require separate owner authorization.

## Interfaces and compatibility

- Do not add or rename crates.
- Add `litci run --allow-degraded` with explicit non-forceable security
  findings.
- Replace faulty evidence schemas instead of preserving compatibility; no
  version has shipped.
- Keep `ContainerEngine` as the sole runtime boundary, extending it only for
  required capabilities, cancellation, identities, and transactional resource
  handling.
- Keep stable JSON snapshots only for the schemas permitted by `TESTING.md`.
- Split oversized modules within their owning component phase. Do not perform
  an unrelated whole-repository rewrite.

## Mandatory phase verification

For every phase, the head agent must run:

1. `cargo fmt --all -- --check`
2. `cargo clippy --all-targets -- -D warnings`
3. `cargo deny check`
4. `python3 tools/check-stubs`
5. The stabilization-ledger checker introduced in Phase 12
6. `tools/tests/check-portable-test-manifest --run`
7. `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`
8. The active component's capability-owning integration and invariant jobs
9. The compiled-Greenlit and GitHub Actions comparison gate where applicable
10. Criterion and whole-run benchmarks under the enforcement rules in
    `TESTING.md`

Before completing a phase, verify every listed exit criterion directly, rerun
all cumulative earlier gates, and confirm that no capability-dependent test
self-skipped.

## Assumptions

- The current v0 specification is authoritative, including reusable workflows
  and environments. Older phase text or README language does not silently
  narrow scope.
- The GitHub oracle proves Actions semantics on the homelab runner environment,
  not GitHub-hosted Ubuntu image parity.
- No known in-scope mismatch may remain in the parity-exception ledger.
- Existing user changes to `README.md` remain untouched.
- Performance optimization resumes only after the relevant semantic and
  security component is certified.
