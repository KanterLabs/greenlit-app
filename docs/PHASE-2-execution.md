# Phase 2 — Execution: containers and `run:` steps

**Prerequisites:** Phase 1 complete (`litci plan` works).
**Crates:** `greenlit-runtime` and `greenlit-init` (new), `greenlit-engine` (extend), `greenlit-app` (add `run` and `setup`).

## Objective

`litci run` executes a shell-only workflow (no `uses:` steps) green, end to end, in base or job-level containers, with GitHub-faithful step, output, dependency, isolation, and log semantics.

## Tasks

### ContainerEngine trait + Docker backend (greenlit-runtime)

- Trait: pull/build/commit image; create/start/stop/remove container; exec with streamed stdout/stderr and exit code; create/remove network. All async.
- Bollard-backed Docker implementation. No shelling out to `docker`.
- **Three-state engine detection:** (a) reachable via `DOCKER_HOST` → Unix socket → Podman socket → use it; (b) binary installed but daemon down → prompt, then `sudo systemctl start docker` (handle socket activation and rootless `systemctl --user` cases); (c) absent → `litci setup` runs Docker's official install script after one confirmation. Every failure state produces the message + fix action; never a raw connection error.
- Validate the Linux x86_64 host before engine work. Map `ubuntu-latest`, `ubuntu-24.04`, `ubuntu-22.04`, and the KanterLabs `homelab` alias; `ubuntu-latest` and `homelab` use 24.04 in v0.

### Base image and private init helper

- `images/base/Dockerfile`: Ubuntu LTS matching the resolved runner label + `bash`, `sh`, `git`, `curl`, `wget`, `jq`, `tar`, `unzip`, `build-essential`, `ca-certificates`, plus `greenlit-init`. Build through the engine API on first use; tag with the label and a content hash.
- Create the private `greenlit-init` crate with `publish = false`. Embed its built bytes in `litci`, extract it only into the image build context, and never install it as a host command.
- `#![forbid(unsafe_code)]` remains in every other crate. Confine required overlay mount syscalls to one documented `greenlit-init` module with explicit safety invariants.

### Overlay isolation

- At container start, `greenlit-init` mounts overlayfs with the Docker-level read-only repo bind as lower layer and a container-local tmpdir as upper; expose the merged view at the workspace path. If unprivileged overlayfs is unavailable, fall back to copy-in and log the fallback.
- The host repo bind mount is read-only at the Docker level, independent of the overlay — defense in depth.
- `--write-back`: after the run, export the upper-layer diff, list changed paths, and require confirmation before the Greenlit host process applies it. The workflow container never receives host write access. `--no-input` rejects `--write-back` unless a separate future non-interactive approval mechanism is explicitly specified.

### Job and step execution semantics (greenlit-engine + greenlit-runtime)

- One container per job; each step is an exec in that container.
- For ordinary runner jobs, use the resolved Greenlit base image. For `jobs.<id>.container`, run steps in the requested image and implement image, credentials placeholder, env, ports, named/anonymous volumes, and safe `options` semantics. Reject host bind mounts, privileged mode, host networking, host PID/IPC namespaces, and other containment-breaking options with a source-spanned fix. Private-registry credentials are completed with auth in Phase 3.
- Shell resolution matching GitHub: default `bash -e {0}` for ordinary Ubuntu jobs when bash exists; default `sh -e {0}` in job containers; honor `shell:` and `defaults.run.shell`. Honor `working-directory`.
- Env layering resolved per step (workflow < job < step), with `runner`/`github` context env vars set (`GITHUB_WORKSPACE`, `GITHUB_REPOSITORY`, `GITHUB_SHA`, `GITHUB_REF`, `RUNNER_OS`, `RUNNER_TEMP`, etc.).
- **Workflow command files:** implement `GITHUB_ENV`, `GITHUB_OUTPUT`, `GITHUB_PATH`, `GITHUB_STEP_SUMMARY` — fresh files per step, parsed after each step, feeding the `env` and `steps.<id>.outputs` contexts for later steps.
- **Log commands:** parse `::group::`/`::endgroup::`, `::error::`, `::warning::`, `::notice::`, `::add-mask::` from step output; apply masking immediately.
- Step outcome model: `outcome` vs `conclusion` with `continue-on-error`; `if:` runtime evaluation using live `steps`/`needs` contexts and status functions; `timeout-minutes` enforced per step.
- After a job's steps and command files finish, evaluate its declared output expressions against the final step context. Populate only direct dependencies as `needs.<id>.result` (`success`, `failure`, `cancelled`, `skipped`) and `needs.<id>.outputs` before downstream job conditions run. Match GitHub's matrix-output merge, secret-redaction, and size rules; cite the documented or observed behavior.
- Job/step results roll up exactly as GitHub does (failure stops subsequent steps unless `if: always()`/`failure()` matches).

### Output and metrics

- Live streamed logs with group folding, per-step status line, end-of-run table: step outcomes and durations plus the stage breakdown (detection, plan, image ensure, container boot, overlay setup, exec) from timed spans.
- Append the versioned run record under `~/.litci/metrics/`; every new stage in this phase gets its span the day it is built.

## Out of scope

`uses:` steps, remote variable lookup, secrets prompting, auth, caching, services, convergent tool layers, parallel jobs. Run jobs sequentially in DAG order this phase.

## Exit criteria and verification

1. `fixtures/shell-ci/` contains a real small repo with two or more jobs and 6+ `run:` steps covering env layering, a custom job container, `GITHUB_OUTPUT`, declared job output → direct `needs` propagation, a group, `if: failure()`, and `continue-on-error`; `litci run` completes green with correct results.
2. Isolation invariant: a step running `rm -rf "$GITHUB_WORKSPACE"` succeeds inside both overlay and copy-in paths while the host tree hash remains unchanged.
3. `--write-back` lists and applies only the confirmed overlay diff; cancellation leaves the host unchanged.
4. Engine detection behavior covers all three states through integration tests with the injected external prober.
5. Planning rejects every unsupported runner label; the three supported labels map to the intended versioned images.
6. Cross-job tests cover outputs/results for success, failure, skipped jobs, direct dependencies, matrix merging, redaction, and size limits without duplicating oracle rules.
7. The complete TESTING.md pipeline passes; the repo's own workflow runs green under Greenlit as required from Phase 2 onward.
