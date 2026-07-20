# Phase 3 — Actions, variables, secrets, auth

**Prerequisites:** Phase 2 complete (`litci run` handles `run:` steps).
**Crates:** `greenlit-actions` (new), `greenlit-app` (add `auth`), `greenlit-engine` and `greenlit-runtime` (extend).

## Objective

Workflows using marketplace actions run green. Local and authenticated GitHub variables resolve before planning completes, secrets are collected before execution, and `litci auth` provides a read-only token through the `greenlit-app` GitHub App device flow.

## Tasks

### Action resolution (greenlit-actions)

- Parse `uses:` forms: `owner/repo@ref`, `owner/repo/subdir@ref`, `./local/path`, `docker://image:tag`. Resolve refs (tag, branch, SHA) to a commit SHA via the GitHub API when a token exists, or via `git ls-remote` tokenless.
- Fetch action source (tarball download; fall back to shallow git clone) into a content-addressed action store at `~/.litci/actions/<owner>/<repo>/<sha>/`. Never re-fetch a stored SHA.
- Parse `action.yml`/`action.yaml`: `inputs` (with defaults and required), `outputs`, `runs` (`using: node20|node24|composite|docker`, `main`, `pre`, `post`, `pre-if`, `post-if`).
- Instrument spans for resolve and fetch plus action-store hit/miss counters; include them in the end-of-run breakdown and metrics record.

### Action execution

- **JavaScript actions:** pin one released `actions/runner` version and its checksums, obtain the same `externals/node20` and `externals/node24` runtime bundles plus the runner-defined Alpine variants from the runner's [external-runtime packaging script](https://github.com/actions/runner/blob/main/src/Misc/externals.sh), and record that source in `ARCHITECTURE.md`. Do not depend on Phase 4's runner-image manifest for action runtimes. Detect the job-container libc and mount the same standard/Alpine runtime variant GitHub uses. Env protocol: `INPUT_<NAME>`, workflow command files, `STATE_` save-state, and **pre/post steps** — post steps run in reverse order at job end regardless of failure.
- **Composite actions:** execute nested steps with correct input scoping (`inputs` context inside the composite), nested expression evaluation, and output mapping.
- **Docker actions:** build from the action's Dockerfile or pull `docker://` image through the engine trait; pass args/entrypoint/env per spec; run as a sibling container sharing the job workspace and network — never through a host socket.
- JavaScript and composite steps execute in a configured job container; Docker actions remain isolated siblings. Complete job-container private-registry credential handling through host-side auth without exposing credentials in logs.
- `actions/checkout` works tokenless for the already-present local repo: detect self-checkout and satisfy it from the isolated workspace instead of a network clone. Checkout of a different repository performs a real clone and requires a token.

### Variables (greenlit-engine + greenlit-app)

- Final v0 resolution chain: repeatable `--var KEY=VALUE` → same-named process environment variable → `.litci/vars` dotenv → authenticated GitHub configuration variables.
- Local values override remote values. For GitHub values, repository variables override organization variables. Environment variables are out with v0 environments/deployments.
- Use Phase 1 extraction to request only literal names. If a dynamic `vars[...]` lookup exists, fetch the complete applicable repository and organization maps before planning.
- If any referenced value remains unresolved locally and no authentication is available, stop before engine detection with `litci auth` as the single fix action. After a successful authenticated lookup, a name absent from GitHub resolves to empty string, matching GitHub.
- Variable lookup is host-side. It may use the stored token without injecting `GITHUB_TOKEN` or `github.token` into the workflow.
- The GitHub App must have read-only access to repository and organization Actions variables. Verify the installed app's current permissions and API endpoint requirements during implementation; if the supplied app lacks them, stop for an owner-side permission update.

### Secrets (greenlit-engine + greenlit-app)

- Resolution chain: `-s KEY=VAL` → process environment → `.litci/secrets` (dotenv; create `0600`; append `.litci/` to `.gitignore` if missing) → interactive prompt.
- Use Phase 1 static extraction: before any container starts, prompt for every referenced-but-unresolved secret; offer to save to `.litci/secrets`. Non-interactive mode (`--no-input`) fails fast listing missing names.
- Every resolved secret value registers with the Phase 2 masker before any step runs.

### Auth (greenlit-app)

- `litci auth`: GitHub App device flow using the supplied public client ID `Iv23liyZuAdn5DSMxtyh`; print user code + verification URL, poll for the token, store token + refresh token in the system keyring (fallback: `0600` under `~/.litci/` with a warning), and refresh transparently on expiry.
- Fallbacks: `litci auth --pat` (paste a fine-grained PAT and print guidance for required read-only repository/variable permissions), `litci auth --gh` (use `gh auth token` and print a broad-scope warning).
- Inject `GITHUB_TOKEN`/`github.token` only into workflows that reference it; host-only action fetching or variable lookup does not imply injection.
- The workflow `permissions:` block cannot narrow a locally supplied token. Parse it; when a workflow requests more than the token grants or more than `contents: read`, print one actionable notice naming the difference. Document the limitation.
- Verify device-flow endpoints, refresh behavior, GitHub App permissions, and parameters against GitHub's current documentation during implementation; do not code from memory.

## Out of scope

`actions/cache` and artifacts (Phase 4 shims), service containers, convergent tool images, parallelism.

## Exit criteria and verification

1. `fixtures/actions-ci/`: workflow with `actions/checkout`, one `setup-*` action, one composite action, one Docker action, local and remote variable references, and secret references — `litci run` green, with pre-run auth/variable/secret collection exercised through scripted integration boundaries.
2. Variable tests cover local precedence, repository-over-organization precedence, missing-auth failure, authenticated missing-name → empty string, dynamic-map fetch, API permission errors, and no workflow token injection for lookup-only runs.
3. Post-step ordering: checkout's post step runs even when a later step fails.
4. Action store invariant: a second run performs zero network fetches for pinned SHAs, asserted at the recording network boundary.
5. `litci auth --pat` is covered through integration behavior; device flow and variable endpoints use a mocked external GitHub endpoint.
6. Node 20 and Node 24 actions execute with the checksum-pinned runtime bundle in both ordinary and custom job containers.
7. The complete TESTING.md pipeline and Phase 2 dogfood run pass.
