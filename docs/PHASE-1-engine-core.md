# Phase 1 — Engine core: parse and evaluate

**Prerequisites:** none. This phase creates the root workspace after the pre-phase Git/document bootstrap.
**Crates:** `greenlit-workflow`, `greenlit-expr`, `greenlit-engine`, `greenlit-metrics`, `greenlit-app` (skeleton).
**No containers, Docker access, or network calls in this phase.**

## Objective

Given a workflow file, a synthetic event, and any required local variable overrides, `litci plan` prints the fully resolved execution plan. This phase is the fidelity moat: the expression evaluator must match GitHub exactly.

## Tasks

### Workspace and CI foundation

- Create only the five Phase 1 crates under the root workspace; pin Rust stable, edition 2024, and commit `Cargo.lock`.
- Stand up the complete ordered CI pipeline in TESTING.md, `cargo-deny` policy, weekly Dependabot configuration, and `tools/check-stubs` before product behavior.
- `tools/check-stubs` validates exact stub syntax, one open registry row per marker, no marker for closed rows, and no marker owned by an in-progress or completed phase. Registered markers owned by not-started future phases are allowed.

### greenlit-workflow

- Parse `.github/workflows/*.yml` into a typed model: `on` (all trigger forms), `env`, `defaults`, `permissions`, `jobs` with `runs-on`, `needs`, `if`, `outputs`, `env`, `defaults`, `strategy.matrix` (including `include`/`exclude`, `fail-fast`, `max-parallel`), `services`, `container` (image, credentials, env, ports, volumes, options), and `steps` (`id`, `if`, `name`, `uses`, `run`, `shell`, `with`, `env`, `working-directory`, `continue-on-error`, `timeout-minutes`).
- Model unsupported-in-v0 constructs (`concurrency`, `workflow_call`, `environment`) as recognized-but-rejected: parsing succeeds, planning fails with a precise "not in v0" message naming the construct and its location.
- Preserve source spans (file, line, column) on every node for error messages.
- Static extraction API: all referenced `secrets.*` names, literal `vars.*` names, whether a dynamic `vars[...]` lookup exists, all `uses:` references, and all `runs-on` values.

### greenlit-expr

- Lexer + parser for the full GitHub expression grammar inside `${{ }}`: literals (`null`, booleans, numbers, single-quoted strings), operators `== != < <= > >= && || !`, grouping, property access (`a.b`), index access (`a['b']`, `a[0]`), object filters (`a.*.b`).
- Built-in functions: `contains`, `startsWith`, `endsWith`, `format`, `join`, `toJSON`, `fromJSON`, `hashFiles` implemented for real in this phase — glob matching + SHA-256 over file contents per GitHub's documented algorithm, rooted at a provided workspace path; the filesystem sits behind an injected trait only so tests can use a fake, the real implementation ships now. Implement status functions `success()`, `failure()`, `cancelled()`, `always()` against injected job/step status.
- Implement GitHub's exact type-coercion and loose-equality rules. Document each rule with a comment citing the GitHub docs section it implements.
- Context model: `github`, `env`, `vars`, `secrets`, `needs`, `matrix`, `steps`, `runner`, `job`, `inputs` as typed lookups; unknown keys resolve to empty string where GitHub does the same.
- **Test oracle:** transcribe every example and rule in GitHub's "Expressions" documentation page into table-driven oracle tests. Every documented function and operator gets at least one row per documented behavior plus coercion edge cases.

### greenlit-engine

- Synthetic event builder: default `push` event populated from local git metadata (repo name, branch, SHA, actor from git config); `pull_request` and `workflow_dispatch` variants with sensible synthetic payloads.
- Matrix expansion → concrete job instances; `needs` → DAG with cycle detection (reject with a named-cycle error); evaluate job- and step-level `if` where inputs are statically known, mark others as runtime-deferred.
- Parse and retain `jobs.<id>.outputs` as runtime-deferred expressions for Phase 2 finalization. Model `needs` context slots for each direct dependency; do not invent output values during planning.
- Supported runner validation: accept x64 `ubuntu-latest`, `ubuntu-24.04`, and `ubuntu-22.04` from GitHub's current [hosted-runner label table](https://docs.github.com/en/actions/how-tos/write-workflows/choose-where-workflows-run/choose-the-runner-for-a-job), plus the KanterLabs `homelab` Ubuntu alias; resolve `ubuntu-latest` and `homelab` to 24.04 for v0. Reject every other OS, architecture, preview, slim, self-hosted, group, or larger-runner label during planning with the supported list and source span.
- `ExecutionPlan` type: ordered jobs with resolved names and runner image identifiers, optional job-container configuration, env layering (workflow < job < step), declared/deferred job outputs, step list with kind (`run` | `uses`), and deferred-condition markers.

### greenlit-app (CLI skeleton)

- `litci plan [--json] [-e EVENT] [-W path] [--var KEY=VALUE]`: human-readable tree output by default, stable JSON with `--json`; `--var` is repeatable.
- Local variable resolution for this no-network phase: CLI override → same-named process environment variable → `.litci/vars` dotenv. If a statically referenced variable is unavailable, fail with an action to supply `--var` or `.litci/vars`; authenticated fallback replaces this interim final step in Phase 3. Dynamic `vars[...]` requires a complete locally supplied map in this phase.
- `litci stats`: read the local versioned NDJSON file and render recent invocation history plus per-stage duration trends; no network access.
- `plan --json` writes exactly one stable plan document to stdout. Human diagnostics, warnings, and timing tables go to stderr; timing/run metadata is excluded from the plan schema.
- Errors render with source spans: `file:line:col — message — fix`.

### greenlit-metrics

- Span-timing layer over `tracing`: wrap parse, expression evaluation, and plan assembly in named spans; aggregate durations per invocation.
- NDJSON writer appending one record per invocation to `~/.litci/metrics/runs.ndjson` (schema versioned from day one). Local-only per AGENTS.md — no network code in this crate, ever.
- `litci plan` emits its stage timings on stderr and appends a record; `litci stats` reads records without appending a recursive stats record.
- Criterion micro-benchmarks for the workflow parser and expression evaluator, wired into CI, recording baselines (no budgets yet — history first).

## Out of scope (do not build)

Container execution, image handling, action fetching, remote variable lookup, secrets prompting, auth, caching, parallelism.

## Exit criteria and verification

1. The complete TESTING.md pipeline passes, including `tools/check-stubs`, warning-free docs, deny, and criterion baseline recording.
2. `cargo run -p greenlit-app -- plan -W fixtures/matrix-needs.yml` (create it: 3 jobs, matrix with include/exclude, needs chain, declared job outputs, if conditions) prints a correct resolved plan.
3. `--json` stdout is valid and byte-stable across runs; timings appear only on stderr and in the metrics record.
4. Planning a fixture using `workflow_call` fails with the precise "not in v0" message and location; unsupported runner labels fail with the accepted stable-x64 list.
5. Local variable precedence, missing-variable guidance, dynamic-variable handling, and unknown-key expression semantics are covered through the expression oracle and `matrix-needs` integration fixture.
6. `litci stats` renders fixture metrics history and stage trends without network access or a new metrics record.
