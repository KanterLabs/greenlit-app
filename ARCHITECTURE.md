# ARCHITECTURE.md — Greenlit

Created in Phase 1 (workspace and CI foundation task); updated in every phase
summary commit per `AGENTS.md` repo hygiene rules.

## Crate boundaries

Cargo workspace, `resolver = "2"`, edition 2024. See `AGENTS.md`'s "Target
workspace layout" for the authoritative crate list and phase-by-phase
introduction order; this section restates the crates that exist as of Phase
1 and their responsibilities.

| Crate | Kind | Responsibility |
|---|---|---|
| `greenlit-app` | bin (`litci`) | Thin CLI: argument parsing (`clap`), wiring the other crates together, rendering output, `anyhow`-wrapped top-level error handling. No parsing/evaluation/planning logic of its own. |
| `greenlit-workflow` | lib | YAML → typed workflow model: triggers, jobs, matrix, containers, services, steps; the job `needs` DAG; static extraction (`secrets.*`, `vars.*`, `uses:`, `runs-on`). |
| `greenlit-expr` | lib | The `${{ }}` expression language: lexer, parser, evaluator, built-in functions, GitHub's coercion/equality rules, and the typed context model. |
| `greenlit-engine` | lib | The planner: `(workflow, event, contexts) -> ExecutionPlan` — matrix expansion, DAG evaluation, supported-runner validation, env layering, deferred-condition marking. |
| `greenlit-metrics` | lib | `tracing`-based span timing, the local NDJSON run-record writer, and `litci stats` rendering support. Strictly local; no network code ever. |

Dependency direction is one-way and acyclic: `greenlit-app` depends on the
other four; `greenlit-engine` depends on `greenlit-workflow` and
`greenlit-expr`; `greenlit-workflow` and `greenlit-expr` do not depend on
each other or on `greenlit-engine`/`greenlit-metrics`. `greenlit-metrics` has
no dependency on the other product crates (it is instrumentation, not
product logic) beyond what `tracing` itself requires.

Future phases add `greenlit-runtime` (Phase 2), `greenlit-actions` (Phase
3), `greenlit-store` (Phase 4), and the container-only `greenlit-init`
helper (Phase 2) — not created yet; see `AGENTS.md`.

## Dataflow

Placeholder — filled in as each phase builds real behavior. The end-to-end
shape once Phase 1 lands product logic (not yet implemented by this
scaffolding task) is:

```
.github/workflows/*.yml
        │  (greenlit-workflow: parse)
        ▼
  typed Workflow model ──────────────┐
        │                            │ static extraction
        │ (greenlit-expr: evaluate)  │ (secrets/vars/uses/runs-on)
        ▼                            ▼
  resolved contexts           (feeds CLI variable/secret
        │                      resolution prompts)
        ▼
  (greenlit-engine: plan)
        │
        ▼
  ExecutionPlan ──► litci plan [--json]   (greenlit-app renders; greenlit-metrics records stage timings)
```

Phase 2 onward extends this with the `ExecutionPlan` flowing into
`greenlit-runtime`'s `ContainerEngine` trait for real execution.

## Known issues log

Every upstream bug or dependency/tooling quirk worked around gets an entry
here — one per issue, newest last — so the reasoning lives in the repo, not
in an agent's context window.

- **`deny.toml` `bans.multiple-versions = "warn"` (not `"deny"`).** Deliberate,
  pragmatic choice, not a loosened security or license gate: transitive
  duplicate crate versions (e.g. two versions of `syn`/`bitflags`/`itertools`
  pulled in by unrelated dependency trees) are extremely common in the Rust
  ecosystem and are usually resolved by upstream crates on their own
  schedule, not by us. Denying on every duplicate would make CI red for
  reasons outside this repo's control and train contributors to reach for
  `deny.toml` skip entries reflexively. RustSec advisories (`advisories`
  vulnerability checks) and the license allowlist (`licenses.allow`) remain
  hard `deny`s — those are the actual security/legal gates and are not
  loosened by this choice. Revisit if duplicate versions start hiding a real
  problem (e.g. two incompatible major versions of a security-sensitive
  crate like a TLS stack).
- **`cargo-deny` 0.20.2 has no config-file lint-level knob for `unmaintained`
  advisories.** The installed `cargo-deny` version restructured
  `[advisories]` so `unmaintained`/`unsound` take a `Scope` value (`all` /
  `workspace` / `transitive` / `none` — which crates the check applies to),
  not a `LintLevel` (`warn`/`deny`/`allow`) as in older `cargo-deny`
  releases; every advisory hit (vulnerability, unmaintained, unsound) is
  hard-denied by the tool itself unless explicitly ignored by ID. The only
  way to downgrade `unmaintained` findings to a warning (visible, non-blocking
  — the intended policy) is the CLI lint-level override, so both the
  documented local command and `.github/workflows/ci.yml` invoke
  `cargo deny check -W unmaintained` rather than encoding it in `deny.toml`.
  `yanked` is still a real config-level `LintLevel` field and is set to
  `"deny"` directly. Source: `cargo-deny` 0.20.2's
  `src/advisories/cfg.rs` (vendored copy under
  `~/.cargo/registry/src/.../cargo-deny-0.20.2/`), which deprecates the old
  `vulnerability`/`notice` lint-level fields entirely in favor of always-deny
  behavior plus the `-W`/`-A`/`-D` diagnostic-code CLI flags.
- **`hashFiles()` leading-slash patterns need an observed-run verdict.** The
  published expressions documentation describes paths as relative to
  `GITHUB_WORKSPACE` and illustrates `/src/*.js` as matching the repository
  root, while the current `@actions/glob` implementation appears to interpret
  a leading `/` as filesystem-absolute before the runner later filters
  results back to the workspace. Greenlit currently keeps the documented,
  workspace-root-relative behavior because `AGENTS.md` gives documentation
  precedence when source and behavior are unclear and no live observation is
  available. A later parity phase must run both a repository-root file and a
  same-shaped filesystem-absolute file on GitHub Actions, record the result,
  and either confirm this choice or correct it. Sources:
  [expressions documentation](https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#hashfiles),
  [`@actions/glob` pattern construction](https://github.com/actions/toolkit/blob/main/packages/glob/src/internal-pattern.ts),
  and [the runner hash script](https://github.com/actions/runner/blob/main/src/Misc/expressionFunc/hashFiles/src/hashFiles.ts).
