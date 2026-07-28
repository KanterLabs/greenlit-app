# Phase 12 — Containment and test authority

## Objective

Restore the meaning of a Greenlit result before repairing component semantics.
Every uncertified capability must fail closed or run only through an explicit,
non-assuring degraded override. Security findings are never forceable. Build
the permanent defect and parity-comparison machinery that every later
stabilization phase must pass.

## Scope

### Governance and certification state

- Track `docs/STABILIZATION-WORKFLOW.md`, this brief, the stabilization ledger,
  and the parity-exception ledger. Add Phases 12–28 to `AGENTS.md`, with only
  Phase 12 in progress.
- Add `tools/check-stabilization-ledger`. It validates ledger schemas, unique
  identifiers, valid owning phases and statuses, resolving commits for closed
  defects, approval metadata for exceptions, the wildcard ban, and the rule
  that a completed phase has no open owned defect.
- Run the checker in the fail-fast local/CI pipeline immediately after
  `tools/check-stubs`. The checker must use only the Python standard library.
- Seed the stabilization ledger from every known audit class. Review agents
  add exact findings before implementation; rows are closed with commits and
  never deleted.

### Capability quarantine

- Introduce one source of truth for stabilization certification. It records
  the owning phase, current certification state, whether an uncertified
  finding is forceable, and the user action that resolves a block.
- Default `litci run` is blocked while any reachable required capability is
  uncertified. Add `litci run --allow-degraded` for explicitly forceable
  findings only.
- A degraded override must be conspicuous in human and machine output, record
  compatibility `degraded`, cap assurance at `none`, and persist the exact
  findings that were forced. It cannot convert a failed or blocked execution
  into a pass.
- Security, credential, secret, action-execution, privileged-infrastructure,
  source-containment, and evidence-integrity findings are non-forceable.
  Unknown findings are non-forceable.
- Quarantine is reachability-aware only where the current planner already
  proves reachability without ambiguity. Ambiguous or dynamic reachability
  fails closed until Phase 16 or 17 certifies it.

### Immediate containment

- Reject execution before network, action resolution, credential retrieval, or
  container creation when the reachable workflow needs secrets, GitHub tokens,
  repository/organization variables, any `uses:` action, or Docker-in-Docker.
  These hard blocks remain until their owning stabilization phases certify the
  paths.
- Disable `litci export` and `litci confirm` with one actionable stabilization
  diagnostic. They cannot be re-enabled before Phase 27.
- Never serialize a secret, access token, refresh token, runtime bearer token,
  or bounded common encoding of one into a plan, lock, result, event journal,
  trace, metrics record, diagnostic, export, or artifact.
- Create each run directory with mode `0700` and every retained run artifact
  with mode `0600`, including temporary files before atomic publication.
  Existing unsafe modes are reported, not silently trusted.
- Extend the secret invariant to scan the complete retained run directory
  recursively after success, failure, cancellation, and preparation failure.

### Canonical comparison authority

- Add stable `ParityObservationV1` JSON containing only
  contract-relevant outcomes, contexts, outputs, lifecycle ordering,
  filesystem probes, and resource/security findings.
- Normalize only schema-declared nondeterminism: timestamps, durations, run
  identifiers, temporary paths, and dynamically allocated ports. Normalizers
  are field-specific; arbitrary key deletion, wildcard paths, and free-form
  text replacement are forbidden.
- Add one canonical comparator. It validates both observations before
  normalization, reports exact JSON paths for mismatches, applies only
  approved case-and-field exceptions, and fails on unknown schema versions,
  duplicate identities, missing observations, or undeclared fields.
- Add one committed seed workflow with a local oracle observation. Run it
  through the release-built `litci` binary in degraded mode, run the same
  commit through GitHub Actions on the correct homelab runner, and compare both
  through the canonical tool.
- Keep invalid-workflow control-plane observations and Greenlit-only security
  invariants outside fabricated GitHub comparisons.

### Test authority and CI

- Inventory every capability-dependent test and CI step. The job that owns a
  capability must provision it or fail; it may not self-skip or replace a real
  runtime with a fake executable.
- Portable jobs may select an explicit non-capability test set, but cannot
  report a capability as tested when it was not provisioned.
- Add a negative comparison case that mutates one contract field and proves
  the four-stage gate fails for the exact field.
- Preserve the `TESTING.md` four-class taxonomy and duplicate-home ban. Extend
  existing fixtures where possible.

## Public interfaces

- Add `litci run --allow-degraded`.
- Add schema-versioned `ParityObservationV1` and a canonical comparison CLI
  under `tools/`; these are certification interfaces and must reject unknown
  fields and versions.
- Add stabilization-ledger and parity-exception Markdown table schemas enforced
  by `tools/check-stabilization-ledger`.
- Do not alter the current evidence schema beyond the minimum containment
  metadata needed to prevent false assurance; Phase 18 owns replacement with
  V2 evidence.

## Invariants

- Default execution cannot pass through an uncertified required capability.
- `--allow-degraded` cannot override a security finding or yield `local`,
  `clean`, `hermetic`, or `github-confirmed` assurance.
- No credential-bearing value reaches retained or rendered bytes.
- Run-directory and artifact modes are restrictive from first creation, not
  repaired after publication.
- Export and confirmation remain impossible.
- Comparison differences fail unless one exact case-and-field exception is
  owner-approved and still within its removal criterion.
- No later stabilization phase is implemented or prepared in this phase.

## Tests

- Extend compiled-binary CLI behavior coverage for default quarantine,
  forceable degraded execution, non-forceable findings, degraded evidence,
  and disabled export/confirmation.
- Extend the existing secret invariant to walk complete retained run trees for
  direct, encoded, and chunk-split values across all terminal paths.
- Extend run-evidence integration coverage for `0700` directories, `0600`
  artifacts, atomic temporary-file modes, and unsafe pre-existing paths.
- Add schema/comparator external-oracle cases for exact equality, each allowed
  normalization class, unknown fields/versions, duplicates, missing records,
  unapproved differences, one exact approved exception, and wildcard
  rejection.
- Extend CI behavior rather than testing private checker/comparator helpers.
  The intentional mismatch is asserted through the same command CI runs.

## Exit criteria

1. All known audit findings have unique stabilization-ledger rows with
   severity, owning phase, user impact, and an authoritative reproduction or
   gate.
2. `tools/check-stabilization-ledger` passes locally and in CI, and rejects a
   malformed temporary ledger in its behavior-level negative gate.
3. Default `litci run` blocks uncertified capability use; an explicitly
   forceable seed can run only with `--allow-degraded` and records degraded
   compatibility with assurance `none`.
4. Credential, secret, action, DinD, source-containment, and evidence-integrity
   findings cannot be forced.
5. Export and confirmation are disabled, and no code path performs their
   network or filesystem side effects.
6. Secret scanning of complete run directories passes for every terminal path,
   and run directories/artifacts are born with `0700`/`0600` modes.
7. No capability-owning CI job self-skips or substitutes a fake runtime.
8. Local oracle, release-built Greenlit, GitHub homelab, and canonical
   comparison observations agree for the seed case.
9. The comparison gate rejects an intentional mismatch and reports its exact
   schema path.
10. The complete cumulative `TESTING.md` pipeline, stargz/provider gates,
    benchmarks, and two release-binary dogfood runs pass with no open Phase 12
    ledger entry.
