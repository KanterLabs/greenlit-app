# Phase 6 — Parity and launch readiness

**Prerequisites:** Phase 5 complete (budgets enforced in CI).
**Deliverables:** parity harness (`tools/parity/`), live dashboard, release/install assets, owner-reviewed launch copy. This phase adds no new product scope; every in-scope defect it reveals must be fixed before completion.

## Objective

A public, continuously updated demonstration that green locally means green on GitHub: ≥95% raw step-level parity across ~50 real repositories, zero open `greenlit-defect` mismatches, and a release package ready for owner-authorized publication.

## Tasks

### Parity harness (tools/parity)

- Repo corpus: a config file listing ~50 popular OSS repos chosen for workflow diversity (languages, services, caching, matrices) and determinism, whose CI uses supported x64 `ubuntu-latest`, `ubuntu-24.04`, or `ubuntu-22.04` labels and remains within v0 scope. Document selection criteria; list excluded repos/constructs with reasons before computing the denominator.
- For each repo at a pinned SHA: fetch its latest completed GitHub run for the same SHA through the API (results only — never re-trigger remote CI), run the same workflow under Greenlit, and compare expanded top-level workflow-step conclusions (`success`, `failure`, `cancelled`, `skipped`). Exclude GitHub's internal setup/cleanup records that are not declared workflow steps.
- Canonical step identity is workflow path + expanded job/matrix identity + declared step ordinal. The raw denominator is the union of GitHub and Greenlit identities; a missing or extra step is a mismatch. The numerator is identities present on both sides with exactly matching conclusions.
- **Mismatch classification:** every mismatch is `greenlit-defect`, `environment-drift`, or `flaky`. A flake requires two consecutive local runs to disagree. Classification never removes a mismatch from the raw numerator/denominator; rationale is recorded for every classification so the score cannot be inflated.
- Report versioned JSON per repo (step identities, conclusions, diffs, classes, rationale) plus aggregate raw score and exact match/defect/drift/flake/excluded counts. Publish no adjusted score.
- Every `greenlit-defect` maps to a public GitHub issue containing the pinned case and diff. The report links issue numbers.
- During Phase 6 development, run on every merge to main and fail regressions below the recorded baseline. Phase completion and all later main builds additionally require raw parity ≥95% and zero open `greenlit-defect` mismatches.

### Defect remediation within Phase 6

- Keep Phase 6 active; do not reopen or roll back earlier phase statuses.
- Changes to earlier crates are allowed only for filed, in-v0 parity defects. Add the regression to its existing TESTING.md-approved oracle table, named fixture, or invariant boundary.
- Use one focused conventional commit per defect inside the Phase 6 PR. Run the owning phase's verification plus the complete cumulative CI/parity pipeline. Close the issue only after its pinned case matches.
- Out-of-v0 gaps remain published exclusions. Do not convert them into Phase 6 features.

### Dashboard

- Generate a static site from parity JSON + Phase 5 benchmark series: aggregate raw score, classification counts, per-repo step diffs/rationale, score-over-commits trend, and GitHub-vs-cold-vs-warm chart. No client-side third-party data fetching, analytics, or telemetry.
- Deploy by rsync/scp to the owner's server behind the project subdomain; read host/path credentials from the environment and never commit them.

### Release and launch assets

- README: one-line pitch, terminal demo of a failing→fixed CI loop, raw parity table/classification counts, benchmark chart, security summary, install instructions, factual act comparison with cited limitations, and honest out-of-v0 list.
- Install script served from the project domain: require Linux x86_64, fetch the release binary, verify checksum, install `litci` to `~/.local/bin`, and give PATH guidance.
- Release automation: tagged releases build the static `litci` binary, generate checksums, publish required `greenlit-*` libraries in dependency order, publish `greenlit-app`, and create GitHub Release and Homebrew artifacts. `greenlit-init` remains unpublished and embedded.
- Re-verify `greenlit-app`, `litci`, all published crate names, Homebrew formula, GitHub repository/App, and domain availability immediately before release. Availability checks do not rename the already approved identity; a collision is an owner blocker.
- Retain GitHub App public client ID `Iv23liyZuAdn5DSMxtyh`; verify the renamed `greenlit-app` registration, device flow, callback configuration, and read-only contents/variables permissions before release.
- Draft Show HN and r/programming copy: problem, demo, raw score/dashboard, security model, and honest limitations. Complete link and rendering review with the owner. Do not publish or message externally without explicit owner authorization and timing.

## Out of scope

New runner features, macOS/Windows, non-x86_64 hosts/runners, any hosted service beyond the static dashboard, or automatic launch-post publication.

## Exit criteria and verification

1. Parity CI green: ≥95% raw parity over the published in-scope denominator and zero open `greenlit-defect` mismatches. Drift and flake mismatches remain in the raw score and have published counts/rationale.
2. Dashboard live at the project subdomain and auto-updated by CI; verify a merge changes the deployed revision.
3. Install script tested on a clean Linux x86_64 VM/container: no Docker present → `litci setup` → `litci run` on a sample repo succeeds.
4. README renders with live raw parity and benchmark data; a release tag produces checksum-verified GitHub, Cargo, and Homebrew install artifacts, each invoking `litci --version` successfully.
5. Every Phase 6 defect fix has a pinned regression and completed owning-phase plus cumulative verification.
6. Launch copy and assets are link-checked and owner-reviewed; publication remains a separate explicitly authorized action.
