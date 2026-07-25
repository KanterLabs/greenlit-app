# actions-ci fixture

A small repository used by Greenlit's Phase 3 action-execution tests
(`PHASE-3-actions.md` exit criterion 1). Its workflow exercises, end to end:

- `actions/checkout` (self-satisfied from the isolated workspace — no
  network for the local repo),
- a real marketplace `setup-*` action (`actions/setup-node`, pinned by full
  commit SHA, so resolution needs no ref lookup and the run needs no
  `actions/cache` involvement),
- a local composite action (`./.github/actions/composite`) — nested input
  scoping, nested expression evaluation, and output mapping,
- a local Docker action (`./.github/actions/docker-action`, a 3-line
  Dockerfile) — an isolated sibling container sharing the job's live
  workspace,
- a local variable reference (`vars.LOCAL_MODE`, gating the job, supplied
  via `--var`) and a remote variable reference (`vars.REMOTE_GREETING`,
  resolved through an authenticated GitHub configuration-variable lookup),
  and
- secret references (`secrets.CLI_SECRET` via `-s`, `secrets.ENV_SECRET`
  via the process environment).

`crates/greenlit-app/tests/actions_ci_smoke.rs` consumes this fixture: it
copies it to a temp directory, `git init`s it, stands up the mocked-GitHub
machinery (`support::fake_github`) for the remote variable lookup, and runs
the compiled `litci` binary end to end against the real Docker daemon and
the real network (the marketplace action and the pinned Node runtime
bundles are real network fetches — see that test's module doc comment for
why it is opt-in rather than part of the default `cargo test` path).

Docker actions now receive the full
`GITHUB_ENV`/`GITHUB_OUTPUT`/`GITHUB_PATH`/`GITHUB_STEP_SUMMARY`
command-file protocol (`ARCHITECTURE.md`'s known-issues log records this as
closed by the action-fidelity wave), so the Docker action here proves its
execution two ways at once: through the shared job workspace (a log file
under `$GITHUB_WORKSPACE`) *and* through a `$GITHUB_OUTPUT` value a later
step consumes, mirroring `crates/greenlit-runtime/tests/actions_docker.rs`.

`litci run` completes this workflow green.
