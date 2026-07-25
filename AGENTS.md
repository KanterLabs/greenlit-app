# AGENTS.md — Greenlit project manager

**This file is the handover root.** A head dev agent given this repo starts here and needs nothing outside these documents. You are implementing **Greenlit**: a local GitHub Actions runner. One line: run workflows locally, fast, with results you can trust — green locally means green on GitHub. This file governs how agents work; the spec governs product behavior; phase files govern what to build; TESTING.md governs what gets tested.

## Project identity

| Surface | Name |
|---|---|
| Product display name | Greenlit |
| Repository, GitHub App, and Cargo CLI package | `greenlit-app` |
| Executable | `litci` |
| Library crates | `greenlit-*` |
| Container-only helper | `greenlit-init` |
| Repository-local state | `.litci/` |
| User-local state | `~/.litci/` |
| Container image namespace | `greenlit/*` |
| Parity defect classification | `greenlit-defect` |

These names are final for v0. There is no legacy command or data-path compatibility requirement because no version has shipped.

## Document map (read order)

1. `AGENTS.md` — this file: working rules, stub discipline, quality bar, metrics rules, status tables.
2. `greenlit-v0-spec.md` — the product: principles, scope, security model, CLI, phase summaries.
3. `TESTING.md` — the four test classes, the banned list, the internal CI pipeline.
4. `docs/PHASE-1-engine-core.md` → `docs/PHASE-10-confirmation.md` — implementation briefs, alongside each completed phase's `docs/PHASE-N-SUMMARY.md`. Exactly one brief is active at a time. Phases 1–4 record the original executor foundation; Phases 5–10 replace its environment and execution architecture with the evidence-first design in the v0 spec.

`README.md` is the repository's public front door, not a governance document: it describes what ships, and the documents above remain authoritative when they disagree.

Precedence on conflict: phase file over spec for implementation detail; spec over phase file for product behavior; TESTING.md is absolute for anything test-related. Every conflict found gets flagged in the phase summary.

## Roles

- **Head agent:** reads all documents once for orientation, then owns sequencing and enforcement — activates one phase at a time, delegates it, verifies exit criteria by *running* the verification commands, maintains the Phase status and Stub registry tables, and writes each phase summary. Never implements ahead of the active phase.
- **Worker agents:** receive exactly four documents — this file, the spec, TESTING.md, and the active phase file — and implement only that phase.
- **Owner (human):** supplies the Owner-provided inputs below on their listed schedule. The owner approved the evidence-first v0 replacement and uninterrupted progression through Phases 5–10 on 2026-07-25. Public publication and launch remain separately authorized actions.

## Owner-provided inputs

Agents must request these from the owner at the listed point and stop rather than guess:

| Input | Needed by |
|---|---|
| Product/repository name — **provided:** `greenlit-app`; CLI — **provided:** `litci` | Supplied before repo creation |
| GitHub App public client ID — **provided:** `Iv23liyZuAdn5DSMxtyh` (app `greenlit-app`, owned by @ShaneKanterman04); owner permission update if Phase 3 verification finds read-only contents/variables access missing | Phase 3 auth |
| Dashboard deploy target: host, path, SSH access | Phase 6 |
| crates.io token + repo release permissions | Phase 6 |
| Explicit authorization and timing for public launch posts | End of Phase 6 |
| Explicit authorization for public runner-image/package publication | End of Phase 10 |

## First actions

1. Initialize Git; commit these nine documents (this file, the spec, `TESTING.md`, and the six phase briefs under `docs/`), a MIT `LICENSE`, and a `.gitignore` (Rust defaults + `.litci/`) at the repo root.
2. Mark Phase 1 "in progress". As its first setup work, create the root Cargo workspace and only the Phase 1 crates, pin the toolchain, commit `Cargo.lock`, and stand up the internal CI pipeline from TESTING.md. CI must be green before product behavior is implemented.
3. Begin the Phase 1 engine-core tasks.

The crate tree below is the target layout, not a requirement to create future-phase crates during bootstrap.

## How to work

1. Read this file, the spec, `TESTING.md`, and the **current phase file only**. The head agent's one-time orientation read of every phase is the sole exception. Do not read ahead or implement anything from a later phase.
2. Work strictly within the current phase's scope. If a task requires something from a later phase, do not build it early — and do not fake it silently. Follow the **Stub discipline** below; it is the only permitted form of deferral.
3. A phase is complete only when every item in its **Exit criteria** passes via the listed verification commands and the complete cumulative pipeline in TESTING.md passes. Then update the status table and write the phase summary. The next phase activates immediately through Phase 10; only external publication requires a separate owner authorization.
4. Never mark a criterion passed without running its verification.
5. If the spec and a phase file conflict, the phase file wins for implementation detail; the spec wins for product behavior. Flag the conflict in your summary.

## Stub discipline

Stubs rot when they are invisible. Every cross-phase placeholder must be loud, tracked, and owned:

- A stub's body is exactly `unimplemented!("STUB(phase-N): <missing behavior>")` — never a silent no-op, fake return value, or hardcoded plausible result. If a stub is reached at runtime it must crash with that message.
- Register every stub in the **Stub registry** below at creation time: location, missing behavior, owning phase.
- A stub may be owned only by a later phase whose status is `not started`; it must be unreachable from all behavior promised by the active phase.
- **A phase is not complete while any stub it or an earlier phase owns exists.** Phase 1 creates `tools/check-stubs`, which validates exact marker syntax, one-to-one correspondence with open registry rows, and ownership against the Phase status table. It fails for markers owned by an in-progress or completed phase while permitting registered future-phase markers.
- A registry row is closed only by filling its "Realized in" column with the commit that implemented the real behavior — rows are never deleted, and closed rows must have no corresponding marker.

## Stub registry

| Location | Missing behavior | Owning phase | Realized in |
|---|---|---|---|
| — | — | — | — |

## Phase status

| Phase | File | Status |
|---|---|---|
| 1 — Engine core | `docs/PHASE-1-engine-core.md` | completed |
| 2 — Execution | `docs/PHASE-2-execution.md` | completed |
| 3 — Actions | `docs/PHASE-3-actions.md` | completed |
| 4 — Environment | `docs/PHASE-4-environment.md` | completed |
| 5 — Resolution evidence | `docs/PHASE-5-speed.md` | completed |
| 6 — Verified content | `docs/PHASE-6-parity-launch.md` | in progress |
| 7 — Fresh execution | `docs/PHASE-7-fresh-execution.md` | not started |
| 8 — Daemon & recovery | `docs/PHASE-8-daemon-recovery.md` | not started |
| 9 — Lazy & hermetic | `docs/PHASE-9-lazy-hermetic.md` | not started |
| 10 — Confirmation | `docs/PHASE-10-confirmation.md` | not started |

## Target workspace layout

Cargo workspace. Crates are fixed — do not add or rename crates without flagging it:

```
crates/
  greenlit-app        Cargo package for the thin `litci` CLI binary (clap).
  greenlit-workflow   YAML → typed workflow model; job DAG; static extraction.
  greenlit-expr       ${{ }} lexer, parser, evaluator; context types.
  greenlit-engine     Planner: (workflow, event, contexts) → ExecutionPlan.
  greenlit-runtime    ContainerEngine trait; bollard/Docker backend; images; overlay.
  greenlit-actions    uses: resolution and JS/composite/Docker action execution.
  greenlit-store      Toolcache, actions/cache shim, artifacts store.
  greenlit-metrics    Timed spans, counters, local NDJSON run records, stats rendering.
  greenlit-init       Private container helper for overlay mounts; never a host command.
```

Phase 1 creates `greenlit-app`, `greenlit-workflow`, `greenlit-expr`, `greenlit-engine`, and `greenlit-metrics`. Phase 2 adds `greenlit-runtime` and `greenlit-init`; Phase 3 adds `greenlit-actions`; Phase 4 adds `greenlit-store`. The release publishes the CLI package and required library crates in dependency order. `greenlit-init` has `publish = false`, is embedded in `litci`, and is extracted only into the base-image build context so the product remains one distributable host binary.

## Global invariants (apply in every phase)

- **Fidelity:** never take a semantic shortcut. When GitHub's behavior is unclear, match GitHub's documented behavior, then its observed behavior; document the source in a code comment. Steps within a job run sequentially and never skip. Each user step executes exactly once unless GitHub itself defines a retry; runtime provisioning never replays it.
- **Security:** repo is mounted read-only with a throwaway overlay upper layer; the host Docker socket is never mounted into any workflow container; workflow containers cannot reach the host LAN; secret values are masked in all log output. These are not configurable off.
- **Host filesystem evaluation:** `hashFiles` never reads outside its supplied workspace root or opens special filesystem nodes; directory enumeration state is proportional to traversal depth, the exact canonical-directory alias registry has fixed entry and retained-path-byte ceilings, symbolic-link alias graphs stay bounded, and evaluation returns at the runner-compatible fixed deadline, with abandoned workers held within a fixed live-worker bound (see ARCHITECTURE.md known issues).
- **UX:** every error maps to a state plus the one action that fixes it. No raw stack traces, no "cannot connect to Docker daemon".
- **Performance budgets** (enforced at Phase 10): < 2s from `litci run` to first step executing on the warm native-Linux profile; < 30s warm re-run of a typical test workflow. Earlier replacement phases record regressions without taking semantic shortcuts to meet them.
- v0 hosts are Linux x86_64. All engine access goes through the `ContainerEngine` trait in `greenlit-runtime` so other platforms and architectures remain ports.

## Quality bar

v0 is held to zero known defects within its declared scope. Operationally:

- No `unwrap()`/`expect()`/`panic!` outside tests; every fallible path is handled or propagated with context. No `#[allow(...)]`, no `TODO`/`FIXME` comments, no `#[ignore]`d tests, no dead code. Registered future-phase `unimplemented!` markers are permitted only under the Stub discipline above.
- `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, and warning-free `cargo doc` on every phase completion.
- `unsafe` only where a kernel interface requires it (`greenlit-init` mount calls), isolated in one module, commented with the invariant it upholds.
- Every GitHub-matching behavior carries a comment citing the docs section or an observed-behavior test that pins it. No guessed semantics — if GitHub's behavior is unknown, test it on GitHub first.
- Every user-visible behavior has an integration test; every public API has a doc comment.
- Before marking a phase complete: a self-review pass re-reading the phase file top to bottom, verifying each task line against the code it produced.

Perfection is scoped: constructs on the spec's Out list are *correctly rejected*, not implemented. Scope expansion is never the answer to a quality instinct.

## Metrics (instrumentation-first, from Phase 1)

Measurement is built before optimization, not after:

- Every pipeline stage runs inside a timed span (`tracing`): parse, plan, engine detection, image ensure, container boot, overlay setup, each step exec, action resolve/fetch, cache lookups. A new stage gets its span the day it exists.
- Every `plan` or `run` invocation appends one NDJSON record to `~/.litci/metrics/runs.ndjson`: stage durations, step durations, hit/miss counters, totals. Read-only reporting commands such as `stats` do not append records. **Strictly local — metrics are never transmitted anywhere. No telemetry, ever.** Any code path that would send metrics off-machine is a defect.
- The end-of-run table always shows the stage breakdown alongside step timings.
- `litci stats` renders recent invocation history and per-stage trends from the local records.
- Micro-benchmarks (criterion) for the parser and expression evaluator live in CI from Phase 1 recording baselines; Phase 5's budget enforcement extends this harness rather than starting fresh.

## Conventions

- Rust stable, edition 2024. `cargo fmt` and `cargo clippy --all-targets -- -D warnings` clean before any phase is complete.
- Errors: `thiserror` in library crates, `anyhow` only in `greenlit-app`. Every user-facing error message states what happened and what to do.
- Async: `tokio`. No blocking calls on the async runtime.
- Tests: governed by `TESTING.md` — read it before writing any test. Its banned list is enforced under the Quality bar.
- Tests colocated per crate; integration tests in `crates/greenlit-app/tests/`. Fixtures (workflow YAML files) in `fixtures/`.
- Dependencies: prefer std and the already-chosen crates (`serde`, `clap`, `tokio`, `bollard`, `tracing`, `thiserror`, `anyhow`). YAML: `serde_yaml` is archived/unmaintained — evaluate maintained successors (e.g. `serde_yml`, `saphyr`) at implementation time and record the choice. Flag any other new dependency in your summary with a one-line justification.
- Commits: conventional commits, one logical change each.
- MIT license header policy: none (LICENSE file only).

## Repo hygiene

Cleanliness is enforced mechanically, not aspirationally:

- `rust-toolchain.toml` pins the toolchain; `Cargo.lock` is committed. Reproducible builds for every contributor and agent.
- `#![forbid(unsafe_code)]` in every crate except `greenlit-init` (whose `unsafe` follows the Quality bar's confinement rule).
- `pub` is exceptional: `pub(crate)` by default; anything public follows the Rust API Guidelines checklist and carries docs.
- Modules stay single-purpose; split a file before it reaches ~400 lines. No grab-bag `util` modules.
- `ARCHITECTURE.md` is created in Phase 1 and updated in every phase summary commit: crate boundaries, dataflow, and a **known-issues log** — every upstream bug or dependency quirk worked around gets an entry with the issue link, so tribal knowledge lives in the repo, not in an agent's context window.
- Dependency hygiene: `cargo deny` gates CI — RustSec advisory check, license allowlist, duplicate-version check. Dependabot enabled weekly. An unused dependency is a defect.
- One PR per phase, squash-merged; `main` stays linear and always green. No committed binaries or generated files — fixtures are small text.

## Definition of done (per phase)

Code + tests + the complete TESTING.md pipeline passing + status table updated + `tools/check-stubs` clean for the active and all earlier phases + a short summary listing: what was built, deviations from the phase file, new dependencies, tests added/deleted, stubs created (with registry rows), and stubs realized (with commits). Phases transition immediately through Phase 10; publication remains outside the phase-completion gate.
