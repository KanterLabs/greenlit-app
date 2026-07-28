# Phase 12 summary — Containment and test authority

Status: **complete**. All ten
`docs/PHASE-12-containment-test-authority.md` exit criteria pass. Exact
implementation commit `30bdb60384adaf3620e0f7f4befa74cc35ca1dbf`
passed the complete local matrix, the same-commit parity seed and comparison,
all capability-owning jobs, fixed performance budgets, and two independent
release-binary dogfood attempts locally and in GitHub Actions.

## What was built

- `docs/STABILIZATION-LEDGER.md` permanently records every known finding with
  severity, owning phase, user impact, authoritative reproduction, state, and
  resolving commit. Its checker validates one-repository commit authority,
  phase ownership and status, unique identifiers, exception approvals and
  scope, wildcard bans, and completed-phase closure. Rows are never deleted.
- One stabilization registry owns capability phase, certification state,
  forceability, and the exact action that clears a block. Default execution
  rejects every reachable uncertified requirement. `--allow-degraded` permits
  only explicitly forceable findings and records their exact identities with
  compatibility `degraded` and assurance `none`.
- Credentials, secrets, actions, services, Docker-in-Docker, source
  containment/write-back, evidence integrity, privileged infrastructure, and
  unknown findings are structurally non-forceable. Ambiguous reachability
  fails closed. The CLI and runtime independently reject these paths before
  credential access, network access, action resolution, or container creation.
- `litci export` and `litci confirm` are disabled until Phase 27. Historical
  implementations, plaintext token fallback, remote-variable acquisition,
  repository-local secret persistence, and other reachable side effects were
  removed or quarantined.
- Retained evidence uses descriptor-relative, no-follow creation. Run
  directories are born `0700`; artifacts and atomic temporary files are born
  `0600`. Unsafe existing objects fail closed. The bounded retained-secret
  scanner walks the complete run tree and detects direct, encoded,
  chunk-split, symlink-target, and runtime-discovered values across every
  terminal path.
- Result authority is composite: catalog completion is published last, private
  readers require the complete catalog-plus-files contract, active-process
  recovery is locked, dynamic masks share one bounded registry, and failed
  resource teardown cannot be rendered as success. Shared workflow storage,
  cache/artifact services, and source-CAS publication remain quarantined for
  their owning later phases.
- `ParityObservationV1`, `tools/compare-parity`, and the shared producer path
  validate observations before field-specific normalization. Unknown schema
  versions or fields, duplicate identities, missing observations, malformed
  lifecycle/provenance, undeclared differences, stale exceptions, and
  wildcards fail with exact JSON paths.
- The live parity path separates credential-only GitHub acquisition from the
  tokenless exact-source candidate. It builds the release binary outside the
  checkout, seals both evidence bundles, and compares the local oracle,
  Greenlit, and same-commit homelab observation through the canonical tool.
  Historical replay and injected GitHub-response certification were removed.
- Test authority now inventories Rust and non-Cargo sources, an exact
  28-target portable set, fixed capability owners, workflow routes, harnesses,
  Criterion identities, and release-transfer/provenance boundaries.
  Capability jobs provision their real prerequisite or fail; they cannot
  self-skip, select zero tests, or substitute a fake product runtime.
- Review-discovered certification defects were repaired and permanently
  recorded through `GL-STAB-103`: repository-root trust, fixed-budget Criterion
  sampling, serial capability ownership, process-liveness waits, closed-output
  synchronization, store-independent durable-helper staging, length-first
  bounded helper verification, and one effective path-specific collision
  action.

## Exit-criteria evidence

1. The ledger contains 103 unique findings: all 64 Phase-12-owned rows are
   resolved, 17 future-phase surfaces are explicitly contained, and 22
   future-phase rows remain open for their owners.
2. The checker passes with zero parity exceptions. Five valid baselines and 55
   malformed-ledger command-boundary canaries pass locally and in CI.
3. Compiled-CLI and real-Docker cases prove default quarantine, the explicit
   forceable shell-only override, exact forced-finding retention,
   compatibility `degraded`, and assurance `none`.
4. Credential, secret, action, Docker-in-Docker, source/write-back,
   evidence-integrity, and unknown findings are non-forceable and generate
   zero engine requests.
5. Export and confirmation return one Phase-27 action and perform no output,
   auth, network, or filesystem side effect.
6. Permission and retained-secret invariants cover success, failure,
   cancellation, preparation failure, output failure, atomic temporary modes,
   unsafe paths, and recursive transformed-secret scanning.
7. Test authority reviews 375 Rust files and 92 non-Cargo sources. The
   capability manifest binds 11 targets and 16 selected tests to five owners,
   12 workflow routes, and 13 harnesses; all 26 mutation canaries fail closed.
8. Parity-seed run `30401315389` and CI run `30401315402` agree at exact commit
   `30bdb60`. The tokenless comparison verified both sealed bundles and the
   local-oracle, GitHub, and Greenlit observations.
9. The comparator's 114 command checks include intentional contract-field
   mismatches that fail at their exact schema paths.
10. The cumulative local and remote matrices pass with no open Phase-12 row:
    Rust quality, dependency and governance policy, 28 portable targets and
    219 tests, five Criterion identities, both copy strategies, deep path,
    production keyring, stargz, six Docker-runtime targets and nine cases,
    whole-run policy/performance, live parity, and two release-binary dogfood
    attempts.

## Dependencies

No new third-party package entered `Cargo.lock`.

- Existing `rustix` was reused in engine and store boundaries and extended
  with `fs`/`process` features for descriptor-relative private creation and
  validation.
- Direct application dependencies `ring`, `zip`, and `sha2` were removed with
  the quarantined persistence/confirmation paths. `zip` and `typed-path` left
  the lockfile.

## Tests added and deleted

Added behavior and invariant targets include:

- compiled-CLI credential capability and isolation;
- live policy/quarantine behavior;
- complete retained-secret and run-evidence permission invariants;
- selected-matrix real-runtime behavior;
- public runtime quarantine assessment;
- native beyond-`PATH_MAX` `hashFiles`;
- metrics invocation-record schema;
- release-profile performance attribution; and
- command-boundary ledger, test-authority, portable/capability manifest,
  parity producer/comparator, provenance/transfer, copy-strategy, credential,
  and Criterion gates.

Deleted duplicate, private-helper, or substitute-backed homes include:

- action manifest/private resolver tests;
- application dotenv and historical policy-mode targets;
- engine execution-value and source-snapshot helper suites;
- fake-engine, shell-smoke, write-back, and substituted action-runtime suites;
  and
- store cache/artifact shim and cache-store suites.

These deletions enforce `TESTING.md`. Authoritative behavior moved to a
compiled CLI, a public runtime boundary, a declared invariant, or an external
oracle. Uncertified action, storage, write-back, daemon, variable, and
credential semantics remain visibly quarantined for their owning phases.

## Verification record

The exact local implementation passed:

- `cargo fmt --all -- --check`
- `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`
- `cargo deny check`
- `python3 tools/check-stubs`
- `tools/check-stabilization-ledger --self-test`
- the complete test-authority, portable/capability-manifest, transfer,
  provenance, comparator, and exact-HEAD producer command boundaries
- `tools/tests/check-portable-test-manifest --run`
- `RUSTDOCFLAGS='-D warnings' cargo doc --locked --workspace --no-deps`
- `tools/tests/check-criterion-manifest`
- both init-helper copy strategies
- the deep-path, persistent-keyring, stargz, Docker-runtime, and
  Docker-policy capability owners
- release-built tokenless local parity from a pristine no-hardlink checkout
- `tools/check-release-dogfood target/release/litci`

The local performance gate measured 1,330 ms invocation-to-first-user-step
p95, 338.46 ms runtime-bootstrap p95, and 1,808.33 ms whole-workflow p95, with
zero retained-setup downloads.

Fresh push-triggered GitHub evidence at the same commit passed:

- Parity seed `30401315389`: one homelab job, all four steps.
- CI `30401315402`: all ten jobs in 40m12s — portable, tokenless local
  evidence, credential-only GitHub observation, native deep path, real Docker,
  persistent keyring, configured stargz, performance/policy, canonical parity
  comparison, and two-run release dogfood.

The remote dogfood attempts retained:

- `000000000000000018c692e0352fc4c1-00000668-0000` in 583,651.21 ms; and
- `000000000000000018c693685c3a20c3-00000766-0000` in 569,393.08 ms.

Both closed as `Passed/Degraded/None`; job cleanup passed. The only workflow
annotation was GitHub's non-blocking checkout Node 20 deprecation notice.

## Stubs and ledgers

No stubs were created or realized. `tools/check-stubs` reports zero markers
and zero registry rows.

All Phase-12 ledger rows are resolved by real implementation commits.
`docs/PARITY-EXCEPTIONS.md` contains no exception, and no owner exception
approval was used.

## Deviations, conflicts, and external operations

- Phase 12 deliberately disables historical Phase 8 daemon behavior and Phase
  10 export/confirmation behavior until their stabilization owners re-certify
  them. This is the active phase's containment requirement, not a silent scope
  reduction or an unresolved product-spec conflict.
- Credentialed GitHub observation is isolated from tokenless candidate
  building and execution, strengthening the brief's four-stage requirement.
- Audit execution expanded the permanent ledger through `GL-STAB-103`.
  Repairs remained containment, evidence integrity, test authority, or
  certification reliability; no Phase 13 or later product semantics were
  implemented.
- The Phase-12 branch and its exact-SHA CI/parity evidence were pushed to the
  repository. No crate, package, runner image, dashboard, release, launch
  post, or other public publication was performed.
- The owner's pre-existing `README.md` edit remained uncommitted and
  byte-for-byte unchanged throughout the phase.
