# Phase 7 — Fresh execution

**Prerequisites:** Phase 6 complete.
**Crates:** `greenlit-engine`, `greenlit-runtime`, `greenlit-store`, and
`greenlit-app`.

## Objective

Jobs execute concurrently from immutable snapshots and one-use writable
sandboxes. No user-modified filesystem is ever reused.

## Tasks

- Materialize job-private workspaces with reflink-first, bounded-copy fallback;
  use a new container writable layer and network for every job.
- Remove live-repository copy-in, provisioning shims, container commits,
  per-repository images, and completed-container reuse.
- Run the DAG asynchronously with deterministic matrices, runtime matrix
  expansion, `needs`, outputs, `max-parallel`, `fail-fast`, fair machine and
  project limits, concurrency groups, and cancellation.
- Preserve action, command-file, checkout, service, artifact, cache, shell,
  condition, timeout, masking, and post-step fidelity under parallelism.
- Scope services, Docker-action siblings, volumes, ports, command files, and
  networks to one job. Apply configured resource limits.
- Retain `--write-back` for exactly one selected job with an unchanged-source
  precondition.

## Exit criteria

1. Parallel jobs cannot observe each other's writes or runtime resources.
2. Dynamic matrices and dependent outputs match GitHub ordering and outcomes.
3. Cancellation reaches steps, actions, services, downloads, and queued jobs
   within one second.
4. Service health failures identify the service and retain its logs.
5. Cold and warm execution have identical semantic evidence.
6. The cumulative pipeline and dogfood pass.
