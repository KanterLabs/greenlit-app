# Greenlit parity-exception ledger

Parity exceptions are not defect waivers. They may cover only an explicit
v0 non-goal or a specification-permitted degradation. An in-scope bug, defect,
regression, missing implementation, invalid observation, or provenance failure
is never waivable. Every exception is bound to one exact comparison case, one
exact source commit, and one exact scalar semantic leaf. Rows are closed, never
deleted.

| Exception ID | Case ID | Source commit | Exact field | Authoritative source | Reason and scope | Owner approval | Removal criterion | Status |
|---|---|---|---|---|---|---|---|---|
| — | — | — | — | — | — | — | — | — |

## Ledger authority

The first line, nine-column header, column order, and one contiguous visible
top-level Markdown table are canonical. HTML blocks or comments, code fences,
indented tables, blank-split rows, malformed delimiters, unknown or reordered
columns, duplicate exception IDs, duplicate active case/source/field keys,
partial placeholders, or extra placeholder rows are invalid. The all-em-dash
placeholder is permitted only while no real rows exist; remove it when the
first exception is added.

The complete permanent table is parsed. `closed` rows remain immutable history
and their IDs are never reused. Only `active` rows participate in comparison.
An active row is keyed by the exact tuple `(Case ID, Source commit, Exact
field)` and can affect only the `oracle` versus `greenlit-release` gate.
Exceptions are never applied to `oracle` versus `github-actions`.

For an active row to apply, both validated observations must contain the exact
path, both values must be scalar leaves of the same semantic type, and those
values must currently differ. The observation case and comparator
`--source-commit` must exactly match the row. An active row for the selected
case at another source commit, or a selected active row that no longer
suppresses a real mismatch, is stale authority and fails validation.

The following can never be excepted:

- `schema_version`, `case_id`, any `source` field, or any `producer` field;
- schema-defined record identity or structure fields such as record `id`,
  `job_id`, `step_id`, lifecycle `sequence` or `kind`, and filesystem
  `logical_path`; member names inside an unconstrained semantic `value` do not
  become identity fields merely because they use one of those names;
- run/job/step conclusions or step outcomes, because an exception cannot
  suppress failure truth;
- an entire document, object, array, collection entry, or record;
- a missing or extra record, missing path, null-versus-present value, type
  mismatch, or container mismatch;
- any lifecycle field or any field removed by declared normalization, including
  timestamps, durations, run identifiers, temporary directories, dynamic host
  ports, and role-specific producer evidence; or
- wildcard, recursive-descent, slice, range, prefix, pattern, or regular
  expression paths.

At most one active exception may target any one semantic record for a given
case and source commit. Splitting an object-wide waiver across several
nominally scalar leaf rows is invalid.

An exception cannot convert invalid input into valid input. Schema,
provenance, completeness, lifecycle, source, capture, repository, and
filesystem validation always run before exception lookup.

## Field contract

- **Exception ID:** `GL-PARITY-NNN`, where `NNN` is a nonzero three-digit
  number. It is globally unique and never reused.
- **Case ID:** one exact committed comparison-case identifier containing only
  its canonical identifier characters.
- **Source commit:** the full lowercase 40-character Git commit authorized by
  the comparison's `--source-commit`. This column binds the exception and its
  cited authority to immutable source content; a row never floats to a later
  commit.
- **Exact field:** one complete canonical `ParityObservationV1` JSON path to a
  scalar semantic leaf, with concrete zero-based array indexes and none of the
  prohibited targets above. Identifier-safe object members use dot notation;
  bracket notation is reserved for members that cannot use that spelling.
  Non-ASCII bracket members use canonical JSON `\u` escaping. Control, format,
  Markdown, and HTML syntax is invalid in a bracket member.
- **Authoritative source:** exactly
  `https://github.com/OWNER/REPO/actions/runs/RUN_ID; source-commit=<40hex>`,
  where the suffix commit exactly equals the row's **Source commit**. The URL
  must use the trusted canonical repository and positive run-ID syntax. For an
  active row selected by a comparison, it must also equal that comparison's
  validated `github-actions` `producer.run_url`; a documentation URL, retained
  capture path, another run, or another repository is not exception authority.
- **Reason and scope:** exactly
  `explicit non-goal (greenlit-v0-spec.md#explicit-non-goals): SUBSTANTIVE DETAIL`
  or `specification-permitted degradation
  (greenlit-v0-spec.md#compatibility-and-result-truth): SUBSTANTIVE DETAIL`.
  A permitted degradation may instead cite
  `#content-and-environment-preparation`; no other or nonexistent spec anchor
  can authorize a waiver.
  The detail must contain 24–1,024 characters, at least five alphanumeric
  tokens, no Unicode control/format characters or Markdown/HTML syntax, and
  explain the bounded semantic difference. Language describing an in-scope
  bug, defect, regression, wrong or incorrect result, repair, broken or
  unimplemented behavior, failure to match, or a pending fix is invalid.
- **Owner approval:** exactly `Shane YYYY-MM-DD`, using a real UTC calendar date
  no later than the validation date. Agent approval and future-dated approval
  are invalid.
- **Removal criterion:** exactly `remove when SUBSTANTIVE FINITE CONDITION`.
  The condition has the same substantive length, token, and character
  requirements and must state an observable event that makes the exception
  obsolete. `never`, `permanent`, `N/A`, `TBD`, `unknown`, owner discretion,
  `if desired`, `when convenient`, and `maybe` criteria are invalid.
- **Status:** exactly `active` or `closed`. Closing a row preserves every field
  as historical evidence.

## Example shape

This illustrative row shows the required syntax; it is not active authority and
must not be copied into the ledger table without real owner approval and
evidence:

```text
| GL-PARITY-001 | shell-only-seed | 0123456789abcdef0123456789abcdef01234567 | $.jobs[0].steps[0].name | https://github.com/KanterLabs/greenlit-app/actions/runs/123456; source-commit=0123456789abcdef0123456789abcdef01234567 | explicit non-goal (greenlit-v0-spec.md#explicit-non-goals): substantive bounded rationale for this one semantic leaf | Shane 2026-07-28 | remove when the cited non-goal leaves the declared v0 scope | closed |
```

The comparator and stabilization-ledger checker enforce the same parser and
metadata rules so authority cannot be weakened by choosing a different
entrypoint.
