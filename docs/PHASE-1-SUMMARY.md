# Phase 1 summary — Engine core: parse and evaluate

Status: complete. All six exit criteria verified by running their commands; the complete
TESTING.md pipeline passes (fmt, clippy `-D warnings`, deny, check-stubs, 155 workspace
tests, doc `-D warnings`, criterion baselines recorded).

The phase closed with an owner-audit hardening wave followed by an independent
adversarial review (external model, max effort) that surfaced 8 defects in the
hashFiles containment — all confirmed, fixed, and re-verified: capability-style
fd-relative traversal (ancestor-rename immunity), `/proc/self/fd` same-inode
reopens, deadline-covered root/`$HOME` establishment, a bounded stranded-worker
cap, depth-proportional DFS path state, toolkit-parity `match || partialMatch`
pruning, and GitHub-parity failure on matched dangling symlinks.

## What was built

- **`greenlit-workflow`** — span-preserving YAML → typed workflow model driven directly off
  `saphyr-parser`'s event stream (custom `yaml::raw` tree; duplicate-key detection, GitHub
  scalar typing). Full trigger/env/defaults/permissions/jobs/steps surface incl.
  `strategy.matrix` with include/exclude, containers, services. Unsupported v0 constructs
  (`concurrency`, `workflow_call`, `environment`) parse but plan-fail with precise
  "not in v0" + span. Static extraction API (secrets/vars/uses/runs-on + dynamic-vars flag).
- **`greenlit-expr`** — full `${{ }}` lexer/parser/evaluator: documented operators,
  literals, property/index access, object filters; GitHub's exact coercion and
  loose-equality (Unicode ordinal case-insensitive compare via `unicode-general-category`,
  pinned to .NET 8 / Unicode 15 semantics); all built-ins including a real `hashFiles`
  (globset + SHA-256, workspace-rooted, injected-filesystem trait with `RealFs` shipping).
  Expression oracle transcribes GitHub's Expressions docs into table-driven tests.
- **`greenlit-engine`** — synthetic push/pull_request/workflow_dispatch events from git
  metadata; matrix expansion; `needs` DAG with named-cycle rejection; partial evaluation of
  `if`/outputs with static/deferred classification; supported-runner validation
  (ubuntu-latest→24.04, 22.04, 24.04 only); `ExecutionPlan` with env layering and
  deferred-condition markers.
- **`greenlit-app`** — `litci plan [--json] [-e] [-W] [--var]` (human tree on stdout,
  byte-stable JSON document; diagnostics/timings stderr-only) and `litci stats` (offline
  NDJSON history + per-stage trends, no recursive record). Errors render
  `file:line:col — message — fix`.
- **`greenlit-metrics`** — tracing span-timing over parse/eval/plan; versioned NDJSON
  appends with interrupted-tail recovery; criterion benches (parser, evaluator) recording
  baselines in CI.
- **Owner-audit hardening wave** (this branch): repository contents treated as untrusted —
  `hashFiles` root-pinned via `openat2`-style semantics with special-node skips, streaming
  `read_dir` with directory-identity checks, bounded canonical-directory alias registry and
  symlink alias graphs, fixed evaluation deadline; bounded workflow discovery; no-follow
  `.litci/vars` opening (rustix); expression/JSON/plan memory budgets; bounded metrics
  writes. New AGENTS.md "Host filesystem evaluation" invariant + TESTING.md invariant
  class 3 text + ARCHITECTURE.md "Phase 1 trust and resource boundaries" section with five
  known-issues entries; deny.toml license allowlist tightened to MIT/Apache-2.0/Unicode-3.0.

## Deviations from the phase file

- YAML: the brief's suggested successors (`serde_yml`, `saphyr`) were evaluated;
  `serde_yml` rejected (provenance concerns), high-level `saphyr` rejected (merge-key/
  span/dup-key hooks private) — the crate drives **`saphyr-parser`** events directly
  instead. Recorded in `crates/greenlit-workflow/Cargo.toml` comments.
- `dotenvy` (named in an earlier commit for `.litci/vars`) was replaced by **`rustix`**
  no-follow opening during the audit — a repo-authored symlink must not redirect
  local-variable reads into host files.
- `tokio`/`bollard` (listed in AGENTS.md's preferred-crates set) are not yet used — no
  network/containers exist in Phase 1; they arrive with Phase 2's runtime crate.

## New dependencies (one-line justifications)

- `saphyr-parser` — span-preserving YAML event stream (see deviation above).
- `globset` + `regex` — hashFiles glob semantics per GitHub's documented algorithm.
- `unicode-general-category` — GitHub/.NET ordinal-ignore-case comparison parity.
- `indexmap` (serde) — declaration-order-significant maps for byte-stable JSON.
- `rustix` (fs) — no-follow `.litci/vars` open relative to the repo directory.
- `serde_json` (promoted to runtime dep in engine) — streams the stable plan shape into a
  bounded byte counter during planning.
- Dev/bench only: `criterion`, `tempfile`, `harness`/`name` (test support).

## Tests added/deleted

155 workspace tests across expression-oracle, integration (plan/matrix/needs/rejections/
CLI/stats), and invariant (hashFiles deadline + resource bounds, deep-chain memory,
partial-match pruning, stranded-worker cap, dangling-symlink parity) classes. Audit wave
restructured flat test files into directories (`cli_behavior/`, `plan_contracts/`,
`matrix_contracts/`, `stats/`, `jobs_and_steps/`, `expression_validation/`); deleted
`spans.rs`, `static_extraction.rs`, `unsupported_constructs.rs` in favor of the
restructured suites (coverage retained via the span/extraction oracle restorations,
commits 82b5e03, 4358346).

## Stubs

None created, none realized. `tools/check-stubs`: 0 markers, 0 rows. Registry empty.

## Conflicts flagged

None between spec and phase file encountered in Phase 1 scope.
