# full-ci fixture

A repository exercising Phase 4's environment work together rather than piece
by piece (`docs/PHASE-4-environment.md` exit criterion 1). Its workflow
exercises, end to end:

- a `postgres` service container, health-gated on `--health-cmd` and reached
  at its service id;
- a tool the GitHub runner image carries but Greenlit's slim base does not,
  provisioned on first use and then served from the per-repo converged image;
- `actions/cache` across two runs of the same repository — a miss that saves,
  then a hit that restores;
- `actions/upload-artifact` in one job and `actions/download-artifact` in a
  dependent job, proving an artifact survives the job boundary intact;
- all of it on the job's own bridge network, behind the run's network policy.

`crates/greenlit-app/tests/full_ci_smoke.rs` drives it twice online and once
offline against one
sandbox `$HOME`, because the cache, the toolcache and the converged image
only prove anything if they survive between runs, and Phase 6's verified
content only proves offline replay when the exact cached lock succeeds.

`litci run` completes this workflow green.
