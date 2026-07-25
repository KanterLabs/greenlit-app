# Phase 6 — Verified content

**Prerequisites:** Phase 5 complete.
**Crates:** `greenlit-store`, `greenlit-actions`, `greenlit-runtime`, and
`greenlit-app`.

## Objective

All immutable runner, action, container, toolchain, source, and download
content is machine-wide, digest verified, safe under concurrency, resumable,
and reusable offline.

## Tasks

- Add the SHA-256 filesystem CAS and SQLite-WAL metadata catalog for objects,
  trees, aliases, references, downloads, leases, runs, and resources.
- Publish atomically, resume Range downloads, single-flight across processes,
  quarantine corruption, and never substitute a nearby version.
- Lazily migrate verified legacy action and Node-runtime entries; never reuse
  legacy converged images.
- Resolve OCI indices/platform manifests through the Distribution API and pull
  only immutable digests.
- Replace dynamic apt convergence and command shims with locked Greenlit runner
  profiles and verified toolchain materializations.
- Emit itemized preparation progress and distinguish Greenlit setup traffic
  from workflow traffic.

## Exit criteria

1. Concurrent requests for one digest download it once.
2. Corruption is rejected, quarantined, and refetched.
3. Interrupted downloads resume and publish only after verification.
4. A second unchanged run performs zero Greenlit-controlled downloads.
5. Fully cached locks run offline; missing offline content names the exact
   identity and performs no substitution.
6. The cumulative pipeline and dogfood pass.
