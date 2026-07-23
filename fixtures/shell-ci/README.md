# shell-ci fixture

A small shell-only repository used by Greenlit's Phase 2 execution tests
(`PHASE-2-execution.md` exit criterion 1). Its workflow exercises, end to end:

- env layering (workflow < job < step),
- a `::group::` block,
- `GITHUB_OUTPUT` and `GITHUB_ENV` command files,
- a declared job output propagated to a direct `needs` dependent,
- `continue-on-error` (a failing step the job tolerates),
- `if: failure()` (a step that stays skipped while the job is green), and
- a custom `jobs.<id>.container` image.

`litci run` completes this workflow green.
