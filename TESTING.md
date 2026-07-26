# TESTING.md — what v0 tests, and what it refuses to test

Tests exist to pin GitHub's behavior and Greenlit's invariants. They do not exist to prove code runs, to raise a coverage number, or to test our own scaffolding. Every test must answer: **which user-visible behavior or invariant breaks if this fails?** If the answer names another test, one of them gets deleted.

## The four test classes — only these exist

1. **Oracle tests** (`greenlit-expr`, `greenlit-workflow`) — table-driven tests transcribing GitHub's documented rules: expression grammar, functions, coercion, workflow semantics. One table row per documented rule plus its edge cases. This is the moat; high volume *here* is correct. When GitHub's docs are ambiguous, the row cites an observed-behavior run on GitHub instead.
2. **Integration tests** (`crates/greenlit-app/tests/` + `fixtures/`) — fixture workflows through `litci plan`/`litci run`, asserting step outcomes, outputs, and end state. Few rich fixtures over many tiny ones: extend an existing fixture before creating a new one. The phase files' named fixtures (`matrix-needs`, `shell-ci`, `actions-ci`, `full-ci`) are the backbone; each phase grows them. Behavior tests that inject a true external boundary such as the engine prober, GitHub API, or clock belong here; calling one a "unit test" does not create a fifth class.
3. **Invariant tests** — the fixed security/fidelity set, listed exhaustively: host tree unchanged after hostile step; frozen source unaffected by later edits; no cross-job writable state; no host Docker socket, privileged workflow container, host mount, host network, or device in any sandbox; LAN blocked / shim reachable / internet policy enforced; direct, encoded, chunk-split, structured-error, and service secret values absent from all output; `hashFiles` stays inside its supplied workspace, skips special nodes, streams wide-directory enumeration with depth-proportional state, enforces fixed entry and retained-byte ceilings on its symbolic-link alias registry, bounds alias traversal, returns at its fixed deadline, and bounds abandoned workers within a fixed live-worker cap; immutable CAS objects are verified and corruption is quarantined; one concurrent download per digest; active leases block deletion; no dirty sandbox survives or is reused after termination; cold/eager/lazy/daemon/in-process runs have identical semantic evidence; zero Greenlit network fetches on unchanged re-run; user steps never replay; unsupported behavior never passes; GitHub confirmation requires matching external evidence; stub checker clean. New invariant tests require a new invariant in AGENTS.md first.
4. **External oracles** — GitHub confirmation/parity cases and criterion/whole-run benchmarks. These judge the whole; they are not duplicated in miniature elsewhere.

## Banned — the anti-bloat rules

- **No tests for private functions or internal helpers.** Internals are covered through the behaviors they serve. If a refactor that preserves behavior breaks a test, that test was wrong.
- **No tests of test code.** Fixtures, builders, and helpers stay too simple to need tests — if a helper needs its own test, simplify the helper. Test utilities asserting on other test utilities is a defect.
- **No mocking our own crates.** Mock only true externals: GitHub's API, the engine prober, the clock. Everything else runs real.
- **No interaction testing** ("was this function called N times") — except where the interaction *is* the contract (for example, zero-refetch and no-script-replay invariants), and then it is asserted at the recording-boundary layer, not on internal calls.
- **No duplicate homes.** Each behavior is asserted in exactly one place. Before writing a test, search for the behavior; extend the existing table or fixture instead of adding a parallel one.
- **No coverage targets.** Coverage percentages manufacture tests. The bar is oracle completeness + the phase exit criteria, nothing else.
- **No snapshot tests** except the declared stable schemas: `litci plan --json`, metrics records, `RunLock`, `JobLock`, execution-result JSON, and the run-event journal. Human diagnostics, progress, log projections, and timing output are asserted separately.
- **Property tests** only where a GitHub rule is genuinely algebraic (coercion); one property per rule, bounded cases. Everything else is a table row.

## Enforcement

- A redundant, helper-testing, or internals-coupled test is a **defect** under the AGENTS.md quality bar — deleting it is a fix, not lost coverage.
- Phase summaries must list tests added *and tests deleted*, with one line justifying any net growth outside oracle tables and the phase's named fixtures.

## Internal CI

Every PR, in order, fail-fast:

1. `cargo fmt --check`
2. `cargo clippy --all-targets -- -D warnings`
3. `cargo deny check` — RustSec advisories, license allowlist, duplicate versions
4. `tools/check-stubs` — exact marker/registry validation and no active-or-earlier owned stubs
5. `cargo test --workspace` (oracle + integration + invariant)
6. `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`
7. Criterion and whole-run benches — record through Phase 9, budget-enforced
   in Phase 10

On main after Phase 10: GitHub-confirmation/parity suite and benchmark budget
gate. Dashboard publication is a separately authorized operation.

**Snapshotter-path coverage:** CI must force and exercise reflink and bounded
copy workspace materialization, eager Docker preparation, and the configured
containerd/stargz lazy provider. A capability-dependent self-skip is a defect;
the owning CI job must provide the capability or fail.

**Dogfood clause:** from Phase 2 onward this repo's own workflow must run green under Greenlit itself, and that run is a required CI step from Phase 4 onward. A CI tool whose repo can't run its own CI locally has failed its founding premise — this is the one test that can never be deleted.
