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

## Known issues log

Entries here describe upstream quirks that the implementation or dependency
policy actively contains. They are not deferred Greenlit behavior.

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
