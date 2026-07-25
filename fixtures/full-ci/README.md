# full-ci fixture

A repository exercising Phase 4's environment work together rather than piece
by piece (`docs/PHASE-4-environment.md` exit criterion 1). Its workflow
exercises, end to end:

- a `postgres` service container, health-gated on `--health-cmd` and reached
  at its service id;
- a tool the GitHub runner image carries but Greenlit's slim base does not,
  provisioned on first use and then served from the converged image;
- `actions/cache` across two runs of the same repository;
- the whole of the above in one job, on the job's own bridge network.

`crates/greenlit-app/tests/full_ci_smoke.rs` drives it twice against one
sandbox `$HOME`, because the cache, toolcache and converged image only prove
anything if they survive between runs.

**Known gaps, deliberately left visible rather than trimmed out of the
fixture's scope statement.** Two behaviors this fixture is meant to cover do
not yet work against the real marketplace actions, and both were found *by*
this fixture:

- `actions/upload-artifact@v4` fails inside the Azure Blob SDK after
  accepting the shim's `signedUploadUrl`, before any request reaches the
  shim. The twirp layer is correct — its field-naming asymmetry is fixed and
  covered in `crates/greenlit-store/tests/artifact_shim.rs` — so the
  remaining fault is in the blob-transfer half.
- `actions/cache@v4` saves correctly (the entry is committed, and a direct
  lookup with the stored key and version returns it) but a second run's
  restore misses, so the two runs compute different cache *versions*. The
  version is a hash of the resolved paths and compression method.

Until both are understood, the workflow above covers the service, the
provisioned tool, and the cache's save path. `litci run` completes it green.
