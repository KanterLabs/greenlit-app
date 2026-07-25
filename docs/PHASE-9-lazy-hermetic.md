# Phase 9 — Lazy and hermetic execution

**Prerequisites:** Phase 8 complete.
**Crates:** `greenlit-runtime`, `greenlit-store`, `greenlit-engine`, and
`greenlit-app`.

## Objective

Provider-capable hosts start before the complete logical runner is downloaded,
while every host retains a verified eager fallback and precise clean,
hermetic, and offline policies.

## Tasks

- Add backend-neutral `RunnerProvider` and `Snapshotter` interfaces.
- Keep the verified eager Docker provider as the universal fallback.
- Add a direct containerd/stargz provider for configured hosts; use eStargz
  access profiles, on-demand verified reads, and no CLI subprocess wrappers.
- Implement `--clean`, `--hermetic`, and `--offline` policy enforcement.
- Record kernel/runtime/provider fingerprints and privileged infrastructure;
  cap assurance whenever late mutable inputs or external traffic exist.

## Exit criteria

1. Lazy-provider first step starts before the full runner arrives.
2. An unprefetched file is fetched transparently and digest verified.
3. Eager and lazy providers produce identical semantic evidence.
4. Hermetic mode blocks external step traffic and rejects late mutable inputs.
5. Clean mode disables transparent mutable caches while retaining immutable
   CAS reuse.
6. The cumulative pipeline and dogfood pass.
