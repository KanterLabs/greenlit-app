# Greenlit stabilization ledger

This permanent ledger records every defect discovered by the stabilization
workflow. Rows are appended or closed; they are never deleted. `open` means
the owning phase has not repaired the defect, `contained` means Phase 12 has
made the affected path impossible without claiming the underlying behavior is
correct, and `resolved` requires a verification case plus the resolving
commit.

| Defect ID | Severity | Owning phase | User-visible impact | Authoritative test or oracle | Status | Resolving commit |
|---|---|---:|---|---|---|---|
| GL-STAB-001 | critical | 12 | An empty or incomplete support report is treated as certified, so unknown behavior can run and appear trustworthy. | Compiled-CLI default quarantine, unknown finding, and forceability cases | open | — |
| GL-STAB-002 | critical | 12 | Certification rejection occurs after daemon, credential, network, action, engine, or image side effects. | Compiled-CLI zero-side-effect preflight cases with recording external boundaries | open | — |
| GL-STAB-003 | critical | 12 | A direct runtime caller can bypass CLI quarantine and start uncertified work. | Public runtime-boundary zero-operation quarantine case | open | — |
| GL-STAB-004 | critical | 12 | A forced degraded pass can receive local or clean assurance instead of none. | Forceable seed result, lock, journal, and human-output integration case | open | — |
| GL-STAB-005 | critical | 12 | Export and confirmation remain operational against uncertified evidence. | Compiled export/confirm refusal cases proving zero files, requests, and mutations | open | — |
| GL-STAB-006 | high | 12 | A support-policy block is persisted as preparation-failed and can conflict with the journal terminal. | Unsupported-preflight single-terminal blocked-result case | open | — |
| GL-STAB-007 | critical | 12 | A passing result is published before journal, trace, catalog, output, and metrics completion. | Closed-writer and terminal-publication integration fault case | open | — |
| GL-STAB-008 | high | 12 | Run directories, evidence files, and atomic temporaries inherit ambient umask. | Compiled run-tree mode gate under umask 000 across terminal paths | open | — |
| GL-STAB-009 | high | 12 | Retained frozen-source files and daemon templates use broad physical modes. | Direct and daemon source-capture run-tree mode integration case | open | — |
| GL-STAB-010 | critical | 12 | No complete retained-run secret scan exists and the runtime bearer token is not masked. | Recursive direct, encoded, chunk-split, runtime-token run-tree invariant | open | — |
| GL-STAB-011 | critical | 12 | The keyring fallback persists access and refresh tokens in plaintext on disk. | Compiled auth case proving keyring failure creates no credential file | open | — |
| GL-STAB-012 | high | 12 | OAuth and GitHub CLI failures can reflect credential-bearing response bodies into diagnostics. | Malformed OAuth, refresh, and gh diagnostic sentinel cases | open | — |
| GL-STAB-013 | medium | 12 | Metrics appends to pre-existing directories or files with unsafe modes. | Existing-mode stats integration cases with actionable refusal | open | — |
| GL-STAB-014 | critical | 12 | Capability-owning Docker, overlay, provider, policy, and benchmark tests return success when prerequisites are absent. | CI capability manifest plus prerequisite-failure gates | open | — |
| GL-STAB-015 | high | 12 | Shell scripts pretending to be Node and over-broad fake engines support real-runtime claims. | CI authority inventory and genuine-runtime ownership gate | open | — |
| GL-STAB-016 | high | 12 | Documented live-test commands use the ignored-test flag on non-ignored tests and execute zero cases. | Command-manifest gate that observes a nonzero selected test count | open | — |
| GL-STAB-017 | high | 12 | Overlay, reflink, bounded-copy, and beyond-platform-path-limit tests can pass without exercising the named path. | Forced-path heavy-runner capability gates with recorded strategy | open | — |
| GL-STAB-018 | high | 12 | Dogfood and release verification omit required gates, permit capability no-ops, and run only once. | Release-built two-run dogfood and release-check command manifest | open | — |
| GL-STAB-019 | high | 12 | No canonical parity schema, comparator, seed oracle, or intentional-mismatch gate exists. | Four-stage Parity Observation V1 seed comparison workflow | open | — |
| GL-STAB-020 | medium | 12 | Criterion silently drops parser or evaluator benchmarks after fixture setup failure. | Criterion benchmark-name manifest and nonzero-sample gate | open | — |
| GL-STAB-021 | critical | 13 | Workflow discovery and source capture can follow unsafe or changing filesystem content. | Frozen-source race, alias, containment, and sealed-tree fixture | open | — |
| GL-STAB-022 | critical | 14 | Lossy or partial YAML validation can accept unsupported workflow behavior. | Full-document schema oracle with mixed valid and invalid constructs | open | — |
| GL-STAB-023 | high | 15 | Expression parsing, evaluation, budgets, or hashFiles semantics can diverge from the pinned runner. | Expression oracle and pinned-runner comparison table | open | — |
| GL-STAB-024 | medium | 16 | The litci plan command promises no network but can retrieve stored credentials and query remote variables. | Zero-network plan case against request-counting GitHub boundary | open | — |
| GL-STAB-025 | critical | 16 | Remote variables and GitHub-token acquisition lack one trust-scoped transactional preflight. | Hostile-fork, missing-token, rotation, and zero-request quarantine cases | open | — |
| GL-STAB-026 | critical | 16 | Dynamic or whole secrets-context access can expose every persisted entry. | Whole-map and dynamic-key secret inventory integration cases | open | — |
| GL-STAB-027 | high | 16 | Credential-bearing dispatch inputs are retained in locks and can be exported verbatim. | Credential-shaped input preflight and evidence-absence case | open | — |
| GL-STAB-028 | critical | 17 | Job selection, static skips, and ambiguous reachability do not consistently bound support, prompt, action, or network work. | Selected-closure and dynamic-ambiguity planner comparison fixture | open | — |
| GL-STAB-029 | critical | 18 | JobLocks are fabricated before needs finish, including skipped, canceled, zero-leg, and never-started jobs. | Needs/dynamic/skipped lifecycle lock fixture with dependency digests | open | — |
| GL-STAB-030 | critical | 18 | Run evidence is not a digest-linked atomic bundle and can expose premature or mixed terminal state. | Bundle tamper, missing-link, duplicate-terminal, and final-marker gate | open | — |
| GL-STAB-031 | medium | 18 | Unsalted secret revision hashes permit dictionary recovery and cross-run correlation. | Keyed revision identity and tamper gate | open | — |
| GL-STAB-032 | critical | 19 | Mutable, hostile, corrupt, or partially published action, OCI, CAS, or source content can execute as verified. | Transitive content-graph, corruption, kill, race, and offline gate | open | — |
| GL-STAB-033 | high | 19 | DinD uses an unlocked mutable image and can pull it after lock finalization. | Exact DinD graph identity and offline-miss case | open | — |
| GL-STAB-034 | critical | 20 | Network containment and resource limits can be applied after untrusted code starts. | Reachable LAN/metadata canary and pre-start limit inspection gate | open | — |
| GL-STAB-035 | critical | 20 | DinD is privileged, unguarded on the job network, effectively unlimited, and can bypass policy. | Isolated authenticated DinD network, limit, and traffic invariant | open | — |
| GL-STAB-036 | high | 20 | Provisioning failures and skipped jobs can leak containers, volumes, networks, or sidecars. | Fault injection after every resource transition with zero survivors | open | — |
| GL-STAB-037 | high | 21 | Queued, fail-fast, and preparation cancellation paths omit lifecycle terminals. | Scheduler cancellation matrix with exactly one terminal per instance | open | — |
| GL-STAB-038 | critical | 21 | Cleanup errors are discarded, allowing success while untrusted resources survive. | Removal-failure lifecycle gate that prevents successful completion | open | — |
| GL-STAB-039 | high | 22 | Shell, environment, command-file, timeout, and step-result semantics can differ from GitHub. | shell-ci and matrix-needs compiled/GitHub comparison | open | — |
| GL-STAB-040 | critical | 23 | Cross-repository checkout implicitly writes the host GitHub credential into workflow-readable Git config. | Real checkout credential/config and exfiltration boundary probe | open | — |
| GL-STAB-041 | critical | 23 | Docker action builds and siblings can reach host networks, omit limits, or survive cancellation. | Dockerfile-build canary, actual-limit, and cancellation teardown cases | open | — |
| GL-STAB-042 | high | 23 | Action pre/main/post, real Node 20/24, state, checkout, and nested lifecycle behavior is uncertified. | Genuine-runtime actions-ci compiled/GitHub comparison | open | — |
| GL-STAB-043 | critical | 24 | Service PID 1 can start before network policy and service failures can leak state or secrets. | Pre-entrypoint canary, health-failure logs, and teardown invariant | open | — |
| GL-STAB-044 | high | 24 | Cache and artifact roots, staging files, blobs, and metadata inherit unsafe modes and incomplete protocol semantics. | full-ci, cache, artifact, and mode/concurrency integration gates | open | — |
| GL-STAB-045 | high | 25 | Daemon prefetch scans unrelated workflows and retrieves credentials before selected-workflow quarantine. | Daemon-enabled selected-workflow zero-request case | open | — |
| GL-STAB-046 | high | 25 | Recovery, daemon templates, credential stores, clean, and GC trust unsafe paths or modes. | Crash-boundary, unsafe-path, lease, and mode fault matrix | open | — |
| GL-STAB-047 | high | 26 | Recorder Drop fabricates an aborted terminal that conflicts with result evidence. | Preparation-failure exactly-one-terminal case | open | — |
| GL-STAB-048 | critical | 26 | Event and log sink failures are remembered but do not stop execution or pass publication. | Journal/output failure boundary with retained-log completeness | open | — |
| GL-STAB-049 | high | 26 | Cancellation is mapped to failed and post-step infrastructure failures are mislabeled as preparation failures. | SIGINT and post-step fault terminal-result cases | open | — |
| GL-STAB-050 | high | 26 | Metrics failure can make the CLI fail after retaining an assuring pass. | Invalid metrics path result/exit agreement case | open | — |
| GL-STAB-051 | high | 26 | Runtime tokens, lowercase percent encoding, JSON escaping, and error-routed secrets can evade masking. | Complete transformed-secret and structured-error invariant | open | — |
| GL-STAB-052 | high | 27 | Inspect and logs accept partial, unknown-version, wrong-run, duplicate, or contradictory evidence. | Verified-consumer version, identity, ordering, and terminal cases | open | — |
| GL-STAB-053 | critical | 27 | Confirmation can update result and confirmation evidence independently, and export lacks a verified immutable closure. | Partial-write, tamper, pagination, and two-pass export/confirm gate | open | — |
| GL-STAB-054 | high | 28 | Component greens may not compose into release-ready whole-product behavior or performance. | Release-built cumulative certification matrix and two dogfood runs | open | — |
| GL-STAB-055 | high | 12 | Private-helper tests, duplicate behavior homes, and own-crate doubles can make internal scaffolding count as user-visible capability evidence. | Source-tree test-authority checker plus the exact compiled integration, declared invariant, oracle, and capability-target manifest | open | — |
| GL-STAB-056 | high | 26 | Write-back is backed only by private or substituted-engine coverage and can apply an incorrect host mutation or leak sandbox content. | Compiled-CLI real-overlay diff, confirmation, apply, refusal, and cleanup matrix | open | — |
| GL-STAB-057 | medium | 28 | Public documentation can advertise capabilities that stabilization has quarantined or not yet recertified. | Whole-product release-readiness documentation and certification review | open | — |
| GL-STAB-058 | high | 12 | Near-miss long secret options such as --secre=VALUE or --secretx=VALUE can echo credential-like values in compiled-CLI parse diagnostics. | Compiled-CLI stderr redaction reproduction for near-miss long secret option spellings | open | — |
| GL-STAB-059 | high | 12 | CI and release parity export the GitHub token to the same process domain that builds, tests, and executes the release candidate. | Split-job workflow and credential-boundary canaries proving the token exists only in a fresh GitHub-observation job with no candidate present | open | — |
| GL-STAB-060 | high | 13 | Frozen source stores executable files with private physical modes but does not restore their logical executable modes in the job workspace. | Sealed-source fixture that executes a tracked 0755 script while retained source artifacts remain 0600 | open | — |
| GL-STAB-061 | high | 19 | OCI resolution misparses a valid tagged digest reference such as name:tag@sha256:… as a registry with no repository. | Compiled image-resolution fixture covering tagged and untagged immutable digest references | open | — |
| GL-STAB-062 | high | 24 | Root-running job containers create package-cache entries the host user cannot reclaim, leaving derived state after dogfood and clean operations. | Release-built dogfood and clean gate proving cache ownership permits complete host-side reclamation | open | — |
| GL-STAB-063 | high | 12 | Bounded parity producers kill a process group but do not adopt and reap grandchildren, leaving zombie descendants in a workflow container. | Producer overflow canaries and release-built dogfood proving the complete descendant tree is absent | open | — |
| GL-STAB-064 | medium | 12 | A host-tmpfs beyond-path-limit invariant is classified as portable even though container AppArmor rejects its required fixture. | Capability manifest owner that hard-fails unless the full beyond-path-limit fixture executes on host tmpfs | open | — |
| GL-STAB-065 | critical | 12 | CI credential-boundary checks accept spaced, wildcard, whole-context, alias, or merged-key expressions that can expose a GitHub token to candidate jobs. | Public workflow mutation gate with an exact allowlist for the isolated GitHub-observation job | open | — |
| GL-STAB-066 | high | 12 | Release provenance executes the candidate version probe with HOME and captured streams staged under ambient temporary paths. | Provenance command-boundary gate proving all candidate staging is descriptor-bound beneath one private validated root | open | — |
| GL-STAB-067 | critical | 12 | Split-job release bundles can be substituted, noncanonical, or mismatched across prepared, local, GitHub, and final evidence boundaries. | Four-kind transfer round trip plus digest, source, role, closure, archive, mode, path, link, race, and binary mismatch rejection gate | open | — |
| GL-STAB-068 | medium | 12 | Release finalizer cleanup references function-local paths after scope exit and can leave reconstructed candidate content behind on failure. | Real finalizer mismatch canary proving exact private-root cleanup on every terminal path | open | — |
| GL-STAB-069 | critical | 12 | A secret supplied on the command line is omitted from the sensitive-value registry and can be retained in run evidence while the recursive scan still publishes a result. | Compiled-CLI retained-tree invariant with a command-line secret across blocked and preparation-failure terminals | open | — |
| GL-STAB-070 | critical | 12 | A parity producer descendant can create a new process session, outlive the bounded parent, and mutate state after the producer reports success or failure. | Public producer canaries requiring escaped-session descendants to be killed and absent before every return | open | — |
| GL-STAB-071 | high | 12 | The stabilization checker accepts a resolved row whose claimed resolving commit does not exist in the repository. | Public malformed-ledger canary using a well-formed nonexistent commit identifier | open | — |
| GL-STAB-072 | high | 12 | Evidence-integrity drift is declared non-forceable but has no behavior gate proving the runtime boundary rejects it before engine operations. | Public runtime quarantine case that induces assessment drift and observes zero production-engine requests | open | — |
| GL-STAB-073 | high | 12 | Historical parity fixtures can be invalid under the current V1 schema or claim a source commit that never contained their seed workflow. | Canonical comparator replay gate binding a valid committed local oracle to its exact source and workflow bytes | open | — |
| GL-STAB-074 | high | 12 | Criterion targets can execute every declared sample while enforcing no latency ceiling, allowing a material parser or evaluator regression to remain green. | Manifest-authoritative Criterion upper-bound gate for every declared benchmark identity | open | — |
| GL-STAB-075 | high | 12 | Non-Cargo capability commands and prerequisite steps can be removed or substituted without either test-authority manifest noticing. | Public capability-manifest workflow-route mutation canaries for owners, runners, prerequisites, and execution commands | open | — |
| GL-STAB-076 | high | 12 | Warm execution repulls an already materialized exact-digest runner or workflow image and emits setup-download evidence on every unchanged run. | Native warm performance gate requiring zero retained setup-download events across repeated compiled-Greenlit execution | open | — |
| GL-STAB-077 | high | 12 | The credential-isolation self-test relies on ambient Git safe-directory configuration and fails before its canaries inside a GitHub job container. | Exact job-container credential self-test with a script-bound repository trust path | open | — |
| GL-STAB-078 | high | 12 | Credential-only parity pins the GitHub CLI to a host path absent from the canonical homelab image, so same-SHA certification fails before observation. | Exact homelab credential-only parity job plus workflow-route authority binding the trusted installed CLI path | open | — |
| GL-STAB-079 | high | 12 | Live parity output directories inherit the set-group-ID bit from the runner temporary volume, so exact private-mode binding rejects same-SHA local evidence. | Live local parity production under the canonical homelab filesystem-group parent with strict private-mode binding | open | — |
| GL-STAB-080 | high | 12 | The persistent-keyring capability job tries to install keyctl with sudo even though the canonical runner forbids privilege elevation, so ordinary-user acceptance never executes. | Digest-verified userspace keyctl provisioning plus the real unprivileged persistent-keyring acceptance gate | open | — |
| GL-STAB-081 | high | 12 | Committed historical parity replay ignores the script-bound checkout trust path and fails inside the portable job container before validating fixture bytes. | Exact pinned-container replay gate with config-independent repository trust | open | — |
| GL-STAB-082 | high | 12 | The copy-strategy capability job tries to install and execute privileged filesystem tooling with host sudo even though the canonical runner forbids privilege elevation. | Pinned private-DinD child-container acceptance proving real reflink and bounded-stream execution without host sudo | open | — |
| GL-STAB-083 | high | 12 | GitHub's job-log archive repeats each seed marker in aggregate and per-step entries, so concatenating every entry rejects valid same-SHA evidence as ambiguous. | Attempt-bound plain-text workflow-job log canaries plus exact live GitHub parity collection | open | — |
| GL-STAB-084 | high | 12 | The isolated copy gate pins a private runner image that the job's credential-free DinD daemon cannot pull, so filesystem capability assertions never execute. | Public digest-pinned child image plus exact native CI execution of the real reflink and bounded-stream gate | open | — |
| GL-STAB-085 | high | 12 | Same-SHA local parity passes Cargo's final hard-linked binary directly to a single-link identity boundary, so valid release builds are rejected before execution. | Separately installed single-link release binary plus exact live local parity production | open | — |
| GL-STAB-086 | high | 12 | A standalone parity binary installed directly beneath its private root makes the sibling isolated HOME appear nested within the binary target boundary, so local evidence fails before execution. | Canonical private target/release binary layout plus exact live local parity production | open | — |
| GL-STAB-087 | critical | 12 | Retained run directories inherit SGID from the runner while evidence creation ignores special bits, so the terminal secret scanner rejects and may remove otherwise valid evidence. | Compiled-CLI SGID-HOME creation and pre-existing-special-bit rejection case plus real retained-secret capability owner | open | — |

## Field contract

- **Defect ID:** `GL-STAB-NNN`, unique and never reused.
- **Severity:** `critical`, `high`, `medium`, or `low`.
- **Owning phase:** one stabilization phase from 12 through 28.
- **Authoritative test or oracle:** one behavior-level oracle, integration,
  invariant, external comparison, or fault-injection gate permitted by
  `TESTING.md`. Impact and oracle cells are plain text with no Unicode
  control/format characters or Markdown/HTML syntax.
- **Status:** `open`, `contained`, or `resolved`. A completed phase may own no
  `open` or `contained` row.
- **Resolving commit:** `—` until resolved, then the 7–40 character Git commit
  that contains the repair and authoritative verification.
