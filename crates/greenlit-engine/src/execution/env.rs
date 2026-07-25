//! Per-step environment layering and the runner/`github` context variables.
//!
//! GitHub composes a step's environment from several layers, most-specific
//! winning: the runner-provided defaults, then workflow `env:`, then job
//! `env:`, then values written to `GITHUB_ENV` by earlier steps, then the
//! step's own `env:`. `GITHUB_PATH` additions are prepended to `PATH`.
//! Environment variable names are case-sensitive on Linux.
//! <https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#env>
//! <https://docs.github.com/en/actions/reference/workflows-and-actions/variables#default-environment-variables>

use indexmap::IndexMap;

/// The four ordered environment layers whose union forms one step's
/// environment. Each map's keys are exact (case-sensitive) variable names.
#[derive(Debug, Clone, Copy)]
pub struct EnvLayers<'a> {
    /// Runner/`github` defaults plus the per-step command-file paths.
    pub base: &'a IndexMap<String, String>,
    /// Resolved workflow-level `env:`.
    pub workflow: &'a IndexMap<String, String>,
    /// Resolved job-level `env:`.
    pub job: &'a IndexMap<String, String>,
    /// Values written to `GITHUB_ENV` by earlier steps in this job. These
    /// persist across steps and override the workflow/job `env:` layers for
    /// later steps, which is the whole purpose of `GITHUB_ENV`.
    pub accumulated: &'a IndexMap<String, String>,
    /// Resolved step-level `env:` — the most specific layer.
    pub step: &'a IndexMap<String, String>,
}

/// Flattens the layers into one environment map, most-specific-wins. The
/// precedence, low to high, is
/// `base < workflow < job < GITHUB_ENV accumulation < step`.
pub fn layer_step_env(layers: EnvLayers<'_>) -> IndexMap<String, String> {
    let mut env: IndexMap<String, String> = IndexMap::new();
    for source in [
        layers.base,
        layers.workflow,
        layers.job,
        layers.accumulated,
        layers.step,
    ] {
        for (key, value) in source {
            env.insert(key.clone(), value.clone());
        }
    }
    env
}

/// Prepends `GITHUB_PATH` additions to the `PATH` variable of `env`.
/// `additions` is ordered highest-priority first — its first element becomes
/// the first `PATH` entry. Existing `PATH` (if any) is appended after.
pub fn apply_path_additions(env: &mut IndexMap<String, String>, additions: &[String]) {
    if additions.is_empty() {
        return;
    }
    let mut segments: Vec<String> = additions.to_vec();
    if let Some(existing) = env.get("PATH")
        && !existing.is_empty()
    {
        segments.push(existing.clone());
    }
    env.insert("PATH".to_string(), segments.join(":"));
}

/// The identity of one run, from which the documented default environment
/// variables are derived. v0 targets github.com and Linux x86_64, so the
/// server URLs and `RUNNER_OS`/`RUNNER_ARCH` are fixed constants rather than
/// fields.
#[derive(Debug, Clone, Default)]
pub struct RunnerEnv {
    /// `GITHUB_WORKFLOW`: the workflow's name.
    pub workflow: String,
    /// `GITHUB_REPOSITORY`: `owner/name`.
    pub repository: String,
    /// `GITHUB_REPOSITORY_OWNER`: the owner login.
    pub repository_owner: String,
    /// `GITHUB_SHA`: the commit SHA.
    pub sha: String,
    /// `GITHUB_REF`: the full ref (e.g. `refs/heads/main`).
    pub ref_full: String,
    /// `GITHUB_REF_NAME`: the short ref name (e.g. `main`).
    pub ref_name: String,
    /// `GITHUB_REF_TYPE`: `branch` or `tag`.
    pub ref_type: String,
    /// `GITHUB_EVENT_NAME`: the trigger event name.
    pub event_name: String,
    /// `GITHUB_ACTOR`: the triggering user login.
    pub actor: String,
    /// `GITHUB_JOB`: the job id.
    pub job: String,
    /// `GITHUB_RUN_ID`.
    pub run_id: String,
    /// `GITHUB_RUN_NUMBER`.
    pub run_number: String,
    /// `GITHUB_RUN_ATTEMPT`.
    pub run_attempt: String,
    /// `GITHUB_WORKSPACE`: the checkout path inside the container.
    pub workspace: String,
    /// `RUNNER_NAME`.
    pub runner_name: String,
    /// `RUNNER_TEMP`: a per-run temporary directory.
    pub runner_temp: String,
    /// `RUNNER_TOOL_CACHE`: the persistent toolcache directory.
    pub runner_tool_cache: String,
    /// `ACTIONS_CACHE_URL`, `ACTIONS_RESULTS_URL`, and
    /// `ACTIONS_RUNTIME_TOKEN`, when a run serves the local shim.
    ///
    /// These are the addresses `actions/cache` and `upload-artifact` reach
    /// their service at. They are `None` for `litci plan`, which starts no
    /// shim, and for any run whose store could not be opened -- an absent
    /// variable makes those actions fail the way they do on a runner with no
    /// cache service, which is honest, rather than pointing them at a URL
    /// nothing is listening on.
    pub actions_service: Option<ActionsService>,
}

/// Where the workflow-facing shim is, and the token that opens it.
///
/// The URLs carry the port the shim actually bound and the address the job
/// container reaches the host at, so they are only knowable once the run's
/// network exists -- which is why this is set per job rather than built with
/// the rest of the environment.
#[derive(Debug, Clone, Default)]
pub struct ActionsService {
    /// `ACTIONS_CACHE_URL`. Must end in `/`: the client concatenates onto it.
    pub cache_url: String,
    /// `ACTIONS_RESULTS_URL`, the artifact service's base.
    pub results_url: String,
    /// `ACTIONS_RUNTIME_TOKEN`, this run's bearer token.
    pub runtime_token: String,
}

impl RunnerEnv {
    /// Builds the documented default environment variables. Command-file paths
    /// (`GITHUB_ENV`, `GITHUB_OUTPUT`, `GITHUB_PATH`, `GITHUB_STEP_SUMMARY`)
    /// are per-step and are added by the driver on top of this base.
    /// <https://docs.github.com/en/actions/reference/workflows-and-actions/variables#default-environment-variables>
    pub fn into_map(self) -> IndexMap<String, String> {
        let mut env = IndexMap::new();
        let mut set = |key: &str, value: String| {
            env.insert(key.to_string(), value);
        };
        // GitHub sets CI and GITHUB_ACTIONS to signal an Actions environment.
        set("CI", "true".to_string());
        set("GITHUB_ACTIONS", "true".to_string());
        set("GITHUB_WORKFLOW", self.workflow);
        set("GITHUB_REPOSITORY", self.repository);
        set("GITHUB_REPOSITORY_OWNER", self.repository_owner);
        set("GITHUB_SHA", self.sha);
        set("GITHUB_REF", self.ref_full);
        set("GITHUB_REF_NAME", self.ref_name);
        set("GITHUB_REF_TYPE", self.ref_type);
        set("GITHUB_EVENT_NAME", self.event_name);
        set("GITHUB_ACTOR", self.actor);
        set("GITHUB_JOB", self.job);
        set("GITHUB_RUN_ID", self.run_id);
        set("GITHUB_RUN_NUMBER", self.run_number);
        set("GITHUB_RUN_ATTEMPT", self.run_attempt);
        set("GITHUB_WORKSPACE", self.workspace);
        set("GITHUB_SERVER_URL", "https://github.com".to_string());
        set("GITHUB_API_URL", "https://api.github.com".to_string());
        set(
            "GITHUB_GRAPHQL_URL",
            "https://api.github.com/graphql".to_string(),
        );
        set("RUNNER_OS", "Linux".to_string());
        set("RUNNER_ARCH", "X64".to_string());
        set("RUNNER_NAME", self.runner_name);
        set("RUNNER_TEMP", self.runner_temp);
        set("RUNNER_TOOL_CACHE", self.runner_tool_cache);
        if let Some(service) = self.actions_service {
            set("ACTIONS_CACHE_URL", service.cache_url);
            set("ACTIONS_RESULTS_URL", service.results_url);
            set("ACTIONS_RUNTIME_TOKEN", service.runtime_token);
        }
        env
    }
}
