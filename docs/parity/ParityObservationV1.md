# ParityObservationV1

`ParityObservationV1` is the canonical comparison-only observation contract
for one Greenlit/GitHub parity case. It records contract outcomes and immutable
producer evidence. Raw logs, renderer text, and Greenlit-only security evidence
remain outside the observation.

The comparison authority is three independently produced observations, in this
fixed order:

1. `oracle` — a direct observation of the workflow's declared contract;
2. `github-actions` — evidence collected from the GitHub Actions run; and
3. `greenlit-release` — evidence collected from the release-built `litci`.

These strings are also the exact observation and raw-capture filename suffixes.
For example, the canonical seed files are
`seed-oracle.json`, `seed-github-actions.json`, and
`seed-greenlit-release.json`.

## Invocation and authority

Run the comparator with an explicit checkout, repository identity, and source
commit:

```bash
tools/compare-parity \
  --repository-root PATH \
  --repository-id OWNER/REPO \
  --source-commit <40hex> \
  --greenlit-binary PATH_TO_RELEASE_LITCI \
  --capture-root PRIVATE_LIVE_ROOT \
  [--exceptions LEDGER] \
  PRIVATE_LIVE_ROOT/seed-oracle.json \
  PRIVATE_LIVE_ROOT/seed-github-actions.json \
  PRIVATE_LIVE_ROOT/seed-greenlit-release.json

tools/compare-parity --self-test
```

`--repository-id` is trusted input obtained from the GitHub API's
`repository.full_name`; the comparator never infers product identity from the
checkout's `origin`, which may be a mirror. For the canonical Greenlit run, the
trusted value is `KanterLabs/greenlit-app`.

`--repository-root` is an exact real worktree root with a direct, real `.git`
directory. Git access ignores inherited `GIT_*` redirection, global/system
configuration, and replacement objects. The worktree must be pristine,
including untracked and ignored entries, throughout comparison. Tracked index
flags such as `skip-worktree` or `assume-unchanged`, which could conceal
working-tree drift, are prohibited. The comparator independently matches the
index to the exact HEAD tree, hashes each raw worktree file or symbolic-link
target with Git blob framing, checks executable modes without local Git
configuration, and rejects clean filters, stat-cache settings, and attributes
that would otherwise conceal raw drift.

`--source-commit` is the exact full lowercase commit under test and must equal
the checkout's pinned `HEAD`; every observation's `source.commit` must equal
it. At that commit, `source.workflow_path` must be a direct
`.github/workflows/*.yml` or `.github/workflows/*.yaml` committed regular,
non-symlink workflow blob, and the SHA-256 of its Git blob bytes must equal
`source.workflow_sha256`. Repository identity, `.git` identity, cleanliness,
and `HEAD` are checked again after comparison.

`--greenlit-binary` is the exact release-built `litci` executable used to
produce `greenlit-release`. It must be a regular, non-symlink executable. The
comparator pins one opened inode, hashes its exact bytes, executes that inode's
`--version` under a minimal environment with a ten-second deadline and a
4,096-byte combined-output ceiling, and requires exact
`litci VERSION (<source-commit>)` output. Its SHA-256 must equal both
`producer.binary_sha256` and the Greenlit capture's bound binary identity.
Missing, non-executable, replaced, unreadable, directory, symbolic-link,
wrong-build, or mutating inputs are provenance failures.

`--capture-root` is an absolute, current-UID-owned, mode-`0700` directory
outside and disjoint from the checkout. Its direct `captures/` directory is
also mode `0700`. The three positional observations must be the exact
`seed-<role>.json` files directly beneath that root; captures are fixed at
`captures/<case_id>-<role>.json`. Every evidence file is a one-link,
current-UID-owned, mode-`0600` regular file opened without following links,
bounded to 8 MiB, and revalidated around its exact read. Root identities remain
stable for the whole comparison, and neither directory may contain any file
outside this exact six-file closure.

Exit status `0` means the three observations agree after declared
normalization and any approved Greenlit exceptions. Status `1` means semantic
parity mismatches; every diagnostic names the exact JSON path and comparison
side. Status `2` means an observation, provenance, schema, ledger, repository,
or command-line validation error.

## Top-level contract

Every document is one JSON object with exactly these required fields:

| Field | Contract |
|---|---|
| `schema_version` | Exactly `ParityObservationV1`. |
| `case_id` | Stable case identity shared by all three observations. |
| `source` | Exact repository, source commit, workflow path, and workflow-content SHA-256. |
| `producer` | Immutable role, repository, runner, run, binary, and raw-capture provenance. |
| `run` | Run identity, timing, conclusion, and temporary directory. |
| `contexts` | Identity-sorted observed context values. |
| `outputs` | Identity-sorted workflow outputs. |
| `jobs` | Nonempty, identity-sorted jobs; each has nonempty ordered steps. |
| `lifecycle` | Nonempty, contiguous sequence starting at one; array order is semantic. |
| `filesystem_probes` | Identity-sorted logical-path observations. |
| `resource_security_findings` | Identity-sorted common external observations. Greenlit-only invariants do not belong here. |
| `dynamic_ports` | Identity-sorted container/host port mappings. |

The record shapes are fixed:

```text
source: {repository, commit, workflow_path, workflow_sha256}
producer: {role, repository, runner, run_id, run_attempt, run_url,
           binary_sha256, capture_method, capture_sha256}
run: {id, started_at, completed_at, duration_ms, conclusion, temporary_directory}
contexts[] / outputs[] / step.outputs[] / job.outputs[]: {id, value}
jobs[]: {id, name, conclusion, duration_ms, outputs, steps}
steps[]: {id, name, outcome, conclusion, duration_ms, outputs}
lifecycle[]: {id, sequence, kind, timestamp, job_id, step_id}
filesystem_probes[]: {id, logical_path, kind, exists, mode, sha256}
resource_security_findings[]: {id, category, outcome, detail}
dynamic_ports[]: {id, container_port, host_port, protocol}
```

Unknown or missing fields at any depth, unknown schema versions, duplicate
JSON object keys, malformed scalar values, duplicate identities, unsorted
identity collections, empty job or step collections, broken lifecycle
references, and missing required observations are validation errors. Context
and output `value` fields may contain any valid JSON value and are compared
recursively without rewriting their text.

JSON numbers are parsed and compared as exact arbitrary-precision decimal
values, never IEEE-754 binary floats. Large exponents remain distinct, and
`NaN`, positive or negative infinity, and other non-JSON numeric constants are
rejected. Fields declared as integers require JSON integer syntax; a
mathematically integral decimal or exponent spelling is not interchangeable
with an integer token.

All timestamps use canonical, uppercase, offset-bearing RFC 3339:
`YYYY-MM-DDTHH:MM:SS[.fraction](Z|+HH:MM|-HH:MM)`, with one through six
fractional digits when a fraction is present. Week dates, unsupported
precision, compact or lowercase forms, invalid calendar values, missing
offsets, and invalid offsets are rejected. Completion must not precede start,
lifecycle timestamps must be nondecreasing and no more than one second outside
the run endpoints, and every exact millisecond duration and enclosing
lifecycle span must agree within the declared 1,000 ms observation tolerance.

## Producer evidence

`producer` has exactly the fields shown above. All three producer repositories
must equal `source.repository` and `--repository-id`; all three runner
identities must agree exactly; and `producer.run_id` must equal `run.id`.
Roles and role-specific fields are:

| Role | Required provenance |
|---|---|
| `oracle` | `capture_method` is `direct-oracle`; `run_attempt` is `1`; `run_url` and `binary_sha256` are null. |
| `github-actions` | `capture_method` is `github-api-logs`; `run_attempt` is the positive GitHub run attempt; `run_url` is exactly `https://github.com/OWNER/REPO/actions/runs/RUN_ID`; `binary_sha256` is null. |
| `greenlit-release` | `capture_method` is `retained-evidence`; `run_attempt` is `1`; `run_url` is null; `binary_sha256` is the exact `--greenlit-binary` bytes' lowercase SHA-256. |

Each live observation is bound to one fixed raw capture beneath
`--capture-root`:

```text
captures/<case_id>-<role>.json
```

The derived path is not a schema field and cannot be redirected by an
observation. Its securely read exact bytes must hash to
`producer.capture_sha256`. A symlink, hard link, wrong mode/owner, directory,
missing file, mutation, or digest mismatch cannot supply that evidence.
There is no committed-capture replay route: certification accepts only
captures produced live from the exact source `HEAD`, so historical evidence
cannot introduce an evidence-commit lag.

The capture itself is an exact `ParityCaptureV1` object:

```text
{schema_version, case_id, role, capture_method, authority, observation}
```

Unknown fields or versions are rejected. Its embedded `observation` is the
published observation with only the self-referential
`producer.capture_sha256` omitted. Live publication injects the exact
capture-byte digest and must reproduce the supplied observation exactly.
`authority` has exactly
`{common, markers, semantic_sha256, <role>}`. `common` binds
`{repository, commit, workflow_sha256, run_id}`; `markers` binds
`{contexts, seed_value, temporary_directory, filesystem_probes}`; and
`semantic_sha256` binds the exact canonical observation excluding `producer`.
The marker `seed_value` is derived from the `emit` step output, not a job
output.
The role block field sets are:

- `oracle`: `source_commit`, `workflow_blob_sha256`,
  `run_block_sha256`, `rendered_verify_sha256`, `bash_path`, `process_umask`,
  `command_output_sha256`, `step_exit_codes`, and
  `log_marker_identities`;
- `github-actions`: `event`, `head_sha`, `workflow_sha256`, `run_attempt`,
  `run_url`, `job_name`, `job_conclusion`, `step_records`,
  `lifecycle_records`, and `log_marker_identities`; and
- `greenlit-release`: `event`, `source_commit`, `build_source_commit`,
  `binary_sha256`, `frozen_workflow_sha256`, `result_conclusion`, `result_compatibility`,
  `result_assurance`, `journal_lifecycle`, `requested_runner`,
  `resolved_runner`, and `reported_durations`.

The GitHub and Greenlit event is exactly `push`. The GitHub `head_sha` and
workflow digest, the oracle source and workflow blob, and both Greenlit
source/build commits must equal the trusted source identity. Oracle run-block,
rendered-script, command-output, Bash, umask, exit-code, and marker claims are
independently derived from the committed workflow and fixed oracle contract,
not trusted because the capture repeats them.
Retained run-lock, result, journal digests, and provider prose are validated
inside the producer but deliberately omitted from the public capture:
the comparator has no independent source for those values, so publishing them
would overstate its authority.
The Greenlit requested runner is `homelab`, the resolved image is
`ubuntu-24.04`, and `reported_durations` is exactly
`{run_elapsed_ms, job_duration_ms, step_duration_ms}` with nonnegative JSON
integer values consistent with the retained result.

Producer evidence is validated before comparison. Only the inherently
role-specific fields `role`, `run_id`, `run_attempt`, `run_url`,
`binary_sha256`, `capture_method`, and `capture_sha256` are then normalized.
`producer.repository` and `producer.runner` remain exact across all three
observations. No producer or source field may be excepted.

## Semantic completeness

Identity collections are sorted by `id`, contain no duplicate IDs, and are
compared record-for-record. A missing record cannot be treated as a value
difference. Jobs contain at least one step, step array order is semantic, and
job/step conclusions and outputs must be internally consistent. A successful
run cannot contain a failed, cancelled, timed-out, or otherwise unsuccessful
job; a successful job cannot contain such a step. Conversely, a failed run or
job cannot claim only successful/neutral/skipped children, and a skipped run
cannot contain a started successful job. A skipped declaration uses conclusion
`skipped` and the matching lifecycle skip event.

Run, job, and step conclusion tokens—and step outcome tokens—are exactly
`success`, `failure`, `cancelled`, `skipped`, `timed_out`, `neutral`,
`action_required`, `stale`, `blocked`, `preparation-failed`, or `aborted`.
A step's outcome equals its conclusion except for the declared
continue-on-error relation `outcome: failure` with `conclusion: success`.

The lifecycle kind set is exactly:

```text
run_started
run_completed
job_started
job_completed
job_skipped
step_started
step_completed
step_skipped
```

The lifecycle begins with one `run_started`, ends with one `run_completed`, and
has contiguous `sequence` values matching array order. Run events have null
`job_id` and `step_id`; job events reference one declared job and have null
`step_id`; step events reference a declared job and one of that job's declared
steps. Every declared job and step has either exactly one ordered
started/completed pair or exactly one skip event, never both. Starts precede
their completions, started/completed job events enclose their step events,
skipped declarations remain in authored order, steps do not overlap, and
lifecycle results, durations, and enclosing run/job conclusions agree with the
declared records.

Filesystem probe `logical_path` values are canonical relative paths, never host
temporary paths, absolute paths, parent traversal, or aliases. An absent probe
uses `kind: "absent"`, `exists: false`, and null `mode` and `sha256`. An
existing file or symbolic-link observation requires a four-digit octal mode and
lowercase SHA-256; an existing directory requires a mode and a null SHA-256.
Any contradictory or incomplete filesystem evidence is invalid.
An observed absent or non-directory ancestor cannot have an observed existing
descendant.

The canonical `shell-only-seed` case is intentionally complete and fixed:

- all producers report runner `homelab`, and the source workflow is
  `.github/workflows/parity-seed.yml`;
- the workflow has the exact `push` branches `main` and `stabilization/**`
  (`workflow_dispatch` may also be present), one `shell` job, and exactly the
  authored `emit` and `verify` Bash run blocks;
- the authored `verify` block sets `umask 0022` immediately before creating
  `parity-seed.txt`, independent of the runner's inherited process umask;
- contexts are `github.job=shell`, `github.workflow=Parity seed`,
  `runner.arch=X64`, and `runner.os=Linux`;
- workflow outputs are empty;
- job `shell` succeeds, has an empty job-output collection, and contains the
  ordered successful steps `emit` and `verify`; `emit` exposes
  `seed_value=greenlit`;
- lifecycle includes the run, job, and both step start/completion pairs in that
  order;
- filesystem probe `parity-seed-file` observes the regular file
  `workspace/parity-seed.txt`, mode `0644`, and its exact content digest; and
- resource/security findings and dynamic ports are empty.

Removing one of these required seed observations, adding an undeclared seed
record, changing its identity, or reporting an incomplete lifecycle or
filesystem record is a validation error rather than an exceptable mismatch.
Normal comparator invocation accepts only this exact case identity. The
synthetic rich-schema case used by `--self-test` runs through a separate
non-certifying child entrypoint whose success label is explicitly self-test
only; it is not selectable through the production comparator.

## Declared normalization and comparison order

Every original document is fully validated before normalization. The comparator
then normalizes only these schema-declared nondeterministic fields:

- `$.run.id`;
- `$.run.started_at`, `$.run.completed_at`, and every lifecycle `timestamp`;
- run, job, and step `duration_ms`;
- `$.run.temporary_directory`;
- each `$.dynamic_ports[n].host_port`; and
- the validated role-specific producer evidence fields listed above.

Container ports, lifecycle order and kinds, conclusions, context and output
values, filesystem paths and evidence, diagnostic text, source identity,
producer repository and runner, and values that merely resemble timestamps,
identifiers, ports, or temporary paths remain exact. There is no arbitrary key
deletion, path-pattern normalization, free-form replacement, or inference from
field names.

Comparison proceeds in two gates:

1. `oracle` must equal `github-actions` after declared normalization. No parity
   exception is consulted for this gate.
2. Only after that gate passes, `oracle` is compared with `greenlit-release`.
   A narrowly approved active exception may suppress one qualifying scalar
   leaf mismatch in this gate.

An exception can never make the direct oracle and GitHub evidence agree, repair
invalid evidence, invent a missing record, change a record's type, or hide
identity or provenance. See `docs/PARITY-EXCEPTIONS.md` for the complete ledger
contract.

## Behavior self-test

The standalone command:

```bash
tools/compare-parity --self-test
```

constructs command-boundary positive and negative cases. It covers exact-number
handling, arbitrary-precision integers, and integer-token syntax; strict RFC
3339 and chronology; every schema section and declared
normalization; producer role, repository, source, commit, workflow, binary, and
capture provenance; unknown fields and versions; duplicate and missing
records; lifecycle pairs, skip variants, ordering, references, and
conclusions; complete filesystem evidence; oracle/GitHub exception isolation;
and exception laundering through protected, normalized, missing, container,
type, whole-record, stale, or wrong-commit paths. It also executes hostile
release binaries that overproduce version output or leave a descendant holding
their output pipes. The intentional semantic negative must fail at the exact
mutated leaf.
