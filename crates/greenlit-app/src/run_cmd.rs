//! `litci run`: resolve the execution plan (as `litci plan` does), detect the
//! container engine, then execute the plan in isolated containers — one per
//! job, one exec per step, with dependency-ready jobs running concurrently —
//! streaming live logs and printing an end-of-run table.
//!
//! `PHASE-2-execution.md` objective: "`litci run` executes a shell-only
//! workflow green, end to end." Planning/evaluation semantics are the engine's;
//! container execution is `greenlit-runtime`'s executor. This module wires the
//! two, records the run's metrics (`AGENTS.md` Metrics: "every `plan` or `run`
//! invocation appends one NDJSON record"), and renders the human summary.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::process::ExitCode;

use greenlit_engine::execution::env::RunnerEnv;
use greenlit_engine::git::{collect_git_context, find_repository_root};
use greenlit_engine::{
    Conclusion, DEFAULT_MAX_MATRIX_LEGS, ExecutionConclusion, ExecutionPlan, JobId, MatrixPlan,
    MatrixValue, PlanOptions, analyze_support, build_synthetic_event, plan, validate_v0_support,
};
use greenlit_expr::Value;
use greenlit_metrics::{Invocation, MetricsStore};
use greenlit_runtime::{
    Cancellation, ContainerEngine, DockerEngine, EngineState, InteractiveConfirm,
    IsolationStrategy, RunConfig, RunReport, StepReport, SystemProber, WriteBackOutcome, detect,
    reject_hermetic_late_inputs, reject_uses_steps, run_plan_cancellable, run_write_back,
    validate_host, validate_request,
};

use crate::cli::{IsolationArg, RunArgs};
use crate::{auth, errors, render, secrets, vars, workflow_discovery, workflow_picker};

/// Run the command, returning the process exit code (a failed workflow run
/// exits non-zero without an error, since its table is the real output).
pub(crate) fn run(args: RunArgs) -> anyhow::Result<ExitCode> {
    let invocation = Invocation::start("run");
    let outcome = invocation.with_timing_subscriber(|| execute(&args, &invocation));

    let record = invocation.finish();
    let _ = render::diagnostics::render_timings(&record, &mut io::stderr());
    let metrics_result = MetricsStore::open_default()
        .and_then(|store| store.append(&record))
        .map_err(|error| errors::metrics_error(&error));

    let code = outcome?;
    metrics_result?;
    Ok(code)
}

/// The isolation strategy the run actually uses. `--write-back` upgrades the
/// default `auto` to a hard overlay requirement: under `auto`, a host where
/// unprivileged overlayfs is unavailable falls back to copy-in at container
/// start, leaving the exported upper layer empty — the run would then report
/// "no changes" while silently discarding every write the workflow made.
/// (`--write-back` with `--isolation copy-in` is already rejected up front.)
fn resolved_strategy(isolation: IsolationArg, write_back: bool) -> IsolationStrategy {
    if write_back {
        IsolationStrategy::Overlay
    } else {
        isolation.into()
    }
}

fn execute(args: &RunArgs, invocation: &Invocation) -> anyhow::Result<ExitCode> {
    let clean = args.clean || args.hermetic;
    validate_host().map_err(|host| anyhow::anyhow!("{host}\n  fix: {}", host.fix()))?;
    validate_request(args.write_back, args.no_input).map_err(|error| anyhow::anyhow!("{error}"))?;
    if !args.matrix.is_empty() && args.job.is_none() {
        anyhow::bail!("`--matrix` needs one selected job\n  fix: add `--job <job-id>`");
    }
    if args.write_back && args.job.is_none() {
        anyhow::bail!("`--write-back` applies one selected job only\n  fix: add `--job <job-id>`");
    }
    if args.write_back && args.isolation == IsolationArg::CopyIn {
        anyhow::bail!(
            "`--write-back` needs the container's overlay upper layer, which `--isolation copy-in` never creates (it copies the checkout in instead)\n  fix: drop `--isolation copy-in` (the default `auto` uses overlay when the host supports it), or omit `--write-back`"
        );
    }

    let cwd = std::env::current_dir().map_err(|error| {
        anyhow::anyhow!(
            "could not determine the current directory: {error}\n  fix: change to an accessible repository directory, then retry"
        )
    })?;
    let repo_root = find_repository_root(&cwd)
        .map_err(|error| errors::event_error(&greenlit_engine::EventError::Git(error)))?;
    crate::daemon::prepare(&repo_root, args.no_daemon);
    let workflow_path =
        workflow_discovery::resolve_workflow_path(args.workflow.as_deref(), &cwd, &repo_root)
            .and_then(|resolution| {
                workflow_picker::resolve_or_pick(resolution, &repo_root, !args.no_input)
            })
            .map_err(|message| anyhow::anyhow!(message))?;
    let evidence = invocation.time_stage("source-freeze", || {
        crate::run_evidence::RunEvidence::capture(&repo_root)
    })?;
    evidence.apply_execution_policy(clean, args.hermetic)?;
    let frozen_repo_root = evidence.source.root.clone();
    let frozen_workflow_path = frozen_repo_root.join(&workflow_path.source_name);

    let workflow = invocation
        .time_stage("parse", || {
            greenlit_workflow::parse_workflow_file_with_name(
                &frozen_workflow_path,
                workflow_path.source_name.as_str(),
            )
        })
        .map_err(|error| errors::parse_error(&error))?;
    evidence.merge_support(&analyze_support(&workflow))?;
    validate_v0_support(&workflow).map_err(|error| errors::plan_error(&error))?;

    let extraction = greenlit_workflow::extract_static(&workflow)
        .map_err(|error| errors::parse_error(&error))?;

    // Local git metadata is collected once, up front: the remote `vars`
    // lookup needs the repository owner/name before planning even starts,
    // and every later use (`build_runner_env`) reuses this same value
    // instead of re-collecting it.
    let git = collect_git_context(&frozen_repo_root)
        .map_err(|error| errors::event_error(&greenlit_engine::EventError::Git(error)))?;
    let repo_leaf = git
        .repository
        .rsplit('/')
        .next()
        .unwrap_or(&git.repository)
        .to_string();

    let dotenv_vars =
        vars::read_dotenv_vars(&repo_root).map_err(|message| anyhow::anyhow!(message))?;
    let vars_outcome = if args.offline {
        vars::resolve_vars(
            &extraction,
            &args.vars,
            dotenv_vars.as_deref().unwrap_or_default(),
            dotenv_vars.is_some(),
        )
        .map_or_else(vars::VarsOutcome::LocalError, vars::VarsOutcome::Resolved)
    } else {
        vars::resolve_vars_with_remote(
            &extraction,
            &args.vars,
            dotenv_vars.as_deref().unwrap_or_default(),
            dotenv_vars.is_some(),
            &git.repository_owner,
            &repo_leaf,
        )
    };
    let vars_value = errors::vars_outcome(vars_outcome)?;

    // Secrets are collected next, before engine detection and well before
    // any container starts (`PHASE-3-actions.md` Secrets: "before any
    // container starts, prompt for every referenced-but-unresolved
    // secret"). `GITHUB_TOKEN` is deliberately excluded from the ordinary
    // chain and resolved separately just below.
    let dotenv_secrets =
        secrets::read_dotenv_secrets(&repo_root).map_err(|message| anyhow::anyhow!(message))?;
    let secrets_outcome = secrets::resolve_secrets(
        &extraction,
        &args.secrets,
        dotenv_secrets.as_deref().unwrap_or_default(),
        &repo_root,
        args.no_input,
    )
    .map_err(|failure| errors::secrets_error(&failure))?;

    let references_token = extraction.references_github_token
        || extraction.secrets.contains_key(secrets::GITHUB_TOKEN_NAME);
    let token_local_override = secrets::local_override_for(
        secrets::GITHUB_TOKEN_NAME,
        &args.secrets,
        dotenv_secrets.as_deref().unwrap_or_default(),
    );
    let (github_token, token_notice) = if references_token {
        auth::resolve_github_token(token_local_override)
    } else {
        (String::new(), None)
    };
    if let Some(notice) = &token_notice {
        eprintln!("{notice}");
    }

    let mut all_secrets = secrets_outcome.resolved.clone();
    if references_token && !github_token.is_empty() {
        all_secrets.push((secrets::GITHUB_TOKEN_NAME.to_string(), github_token.clone()));
    }
    let secrets_value = Value::object(
        all_secrets
            .iter()
            .map(|(name, value)| (name.clone(), Value::String(value.clone())))
            .collect(),
    );

    let event_kind: greenlit_engine::EventKind = args.event.into();
    let dispatch_inputs: HashMap<String, String> = args.inputs.iter().cloned().collect();
    let mut event =
        build_synthetic_event(event_kind, &frozen_repo_root, &workflow, &dispatch_inputs)
            .map_err(|error| errors::event_error(&error))?;
    if references_token {
        // Baked in *before* planning: plan-time partial evaluation folds
        // `github.*` field access against this concrete object, so a later
        // `RunConfig.github` patch would be too late for any statically
        // foldable `${{ github.token }}` use.
        event.github = auth::inject_github_token(event.github, &github_token);
    }
    let plan_options = PlanOptions {
        vars: vars_value.clone(),
        max_matrix_legs: DEFAULT_MAX_MATRIX_LEGS,
    };
    let execution_plan = invocation
        .time_stage("plan", || plan(&workflow, &event, &plan_options))
        .map_err(|error| errors::plan_error(&error))?;
    let execution_plan = match &args.job {
        Some(job) => prune_to_job(&execution_plan, job, &args.matrix, args.write_back)?,
        None => execution_plan,
    };
    // Fail before any engine work: a `uses:` step that is certain to execute
    // would otherwise surface only after image ensure, container boot, and a
    // potentially long workspace copy.
    reject_uses_steps(&execution_plan).map_err(|error| anyhow::anyhow!("{error}"))?;
    if args.hermetic {
        reject_hermetic_late_inputs(&execution_plan, &git.repository)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
    }
    if references_token && !github_token.is_empty() {
        let effective_permissions: Vec<Option<&greenlit_engine::PermissionsPlan>> = execution_plan
            .jobs
            .iter()
            .map(|job| {
                job.permissions
                    .as_ref()
                    .or(execution_plan.permissions.as_ref())
            })
            .collect();
        if let Some(notice) = auth::token_permission_notice(&effective_permissions) {
            eprintln!("{notice}");
        }
    }

    let workflow_name = workflow
        .name
        .as_ref()
        .map(|name| name.value.clone())
        .unwrap_or_else(|| workflow_path.source_name.clone());
    let workspace = format!("/home/runner/work/{repo_leaf}/{repo_leaf}");

    // Action resolution reuses whatever token `litci auth` already stored
    // (`PHASE-3-actions.md` "Action execution": "pick `GitHubApiResolver`
    // (token) when Some, else `GitLsRemoteResolver`") — independent of
    // whether *this workflow* references `secrets.GITHUB_TOKEN`/
    // `github.token` (host-side action fetching is not workflow-token
    // injection).
    let action_token = auth::current_token().ok().flatten();
    let (actions_config, pinned_resolver) = build_action_runtime_config(action_token, args.offline)
        .map_err(|message| anyhow::anyhow!(message))?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            anyhow::anyhow!(
                "could not start the async runtime: {error}\n  fix: retry; if it persists, file an issue"
            )
        })?;
    let action_preflight = invocation
        .time_stage("action-resolve", || {
            runtime.block_on(greenlit_runtime::preflight_plan_actions(
                &execution_plan,
                &actions_config,
                &frozen_repo_root,
                &workspace,
            ))
        })
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    runtime
        .block_on(pinned_resolver.freeze())
        .map_err(|error| anyhow::anyhow!(
            "could not finalize action resolutions: {error}\n  fix: retry after the mutable action ref stops changing"
        ))?;
    let pinned_actions = pinned_resolver.resolutions().map_err(|error| {
        anyhow::anyhow!(
            "could not read finalized action resolutions: {error}\n  fix: preserve the run directory and retry"
        )
    })?;
    for (reference, commit) in pinned_actions {
        if !action_preflight
            .actions
            .values()
            .any(|locked| locked == &commit)
        {
            anyhow::bail!(
                "finalized action resolution {reference}={commit} is absent from the preflight action inventory\n  fix: preserve the run directory and file a Greenlit defect"
            );
        }
    }
    let engine = invocation.time_stage("detection", || runtime.block_on(connect_engine()))?;
    let runtime_fingerprint = runtime
        .block_on(engine.runtime_fingerprint())
        .map_err(|error| {
            anyhow::anyhow!(
                "could not fingerprint the container runtime: {error}\n  fix: restart the local container daemon, then retry"
            )
        })?;
    let content_store = open_content_store()?;
    let mut progress = render::progress::renderer_for_stderr();
    let container_locks = invocation
        .time_stage("image-resolve", || {
            runtime.block_on(greenlit_runtime::preflight_plan_images(
                &engine,
                &execution_plan,
                &action_preflight.container_images,
                &content_store,
                args.offline,
                &mut progress,
            ))
        })
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let runner_locks = invocation
        .time_stage("runner-resolve", || {
            runtime.block_on(greenlit_runtime::preflight_plan_runners(
                &engine,
                &execution_plan,
                &content_store,
                args.offline,
                &mut progress,
            ))
        })
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let run_lock = evidence.lock(crate::run_evidence::LockInputs {
        workflow_path: &workflow_path.source_name,
        event_name: event_kind.event_name(),
        inputs: &args.inputs,
        selected_job: args.job.as_deref(),
        selected_matrix: &args.matrix,
        offline: args.offline,
        clean,
        hermetic: args.hermetic,
        runtime: &runtime_fingerprint,
        plan: &execution_plan,
        secrets: &all_secrets,
        actions: action_preflight.actions,
        containers: container_locks,
        runners: runner_locks,
        toolchains: action_preflight.toolchains,
    })?;

    // The local stores this run serves. A machine whose `HOME` cannot be
    // resolved simply runs without them -- `actions/cache` then behaves as it
    // does on a runner with no cache service, which is an honest miss rather
    // than a broken run.
    let store_config = build_store_config(clean, args.hermetic);

    let config = RunConfig {
        repo_host_path: frozen_repo_root,
        workspace: workspace.clone(),
        strategy: resolved_strategy(args.isolation, args.write_back),
        runner_env: build_runner_env(&git, event_kind, workflow_name, workspace),
        github: event.github.clone(),
        vars: vars_value,
        inputs: event.inputs.clone(),
        secrets: secrets_value,
        // Every secret this run actually resolved — from any source in the
        // chain, not only `-s` — is masked before any step runs
        // (`PHASE-3-actions.md` Secrets: "Every resolved secret value
        // registers with the Phase 2 masker before any step runs").
        initial_masks: all_secrets.iter().map(|(_, value)| value.clone()).collect(),
        volume_namespace: evidence.run_id.clone(),
        locked_images: Some(run_lock.containers.clone()),
        write_back: args.write_back,
        readiness: greenlit_runtime::ReadinessConfig::default(),
        actions: actions_config,
        store: store_config.clone(),
        resources: greenlit_runtime::ResourceLimits {
            nano_cpus: args.cpus,
            memory_bytes: args.memory,
            pids: args.pids_limit,
            disk_bytes: args.disk_limit,
        },
    };

    // `Stdout` (not a lock guard) is `Send`, which the executor's streaming sink
    // requires; each write still locks internally. Phase progress renders on
    // stderr so this stream stays the machine-parseable run log.
    let mut out = io::stdout();
    let cancellation = Cancellation::new();
    let report = runtime
        .block_on(async {
            let execution = run_plan_cancellable(
                &engine,
                &execution_plan,
                &config,
                &mut out,
                &mut progress,
                &cancellation,
            );
            tokio::pin!(execution);
            tokio::select! {
                result = &mut execution => result,
                signal = tokio::signal::ctrl_c() => {
                    cancellation.cancel();
                    let result = execution.await;
                    match signal {
                        Ok(()) => result,
                        Err(error) => {
                            result?;
                            Err(greenlit_runtime::ExecError::Infrastructure {
                                message: format!("could not listen for cancellation: {error}"),
                                fix: "retry the run".to_string(),
                            })
                        }
                    }
                }
            }
        })
        .map_err(|error| anyhow::anyhow!("{error}"))?;

    for job in &report.jobs {
        for step in &job.steps {
            invocation.record_step_duration(job.id.clone(), step.label.clone(), step.duration);
        }
    }

    // Action-store and Node-runtime-cache hit/miss counts, for the
    // end-of-run stage breakdown (`AGENTS.md` Metrics: "instrument spans
    // ... hit/miss counters"; `PHASE-3-actions.md`: "surface the store
    // hit/miss counts into the run's end-of-run breakdown"). Reuses the
    // existing `hit_miss` record field/`record_lookup` API from Phase 1 —
    // no metrics-schema change, so no deliberate snapshot update is needed
    // (`TESTING.md`: the metrics record schema is a declared-stable
    // snapshot surface).
    if let Some(store) = config.store.as_ref() {
        // Drained from the store rather than read off a tracing span: the
        // shim serves on a spawned task, which does not inherit the scoped
        // subscriber the timing layer installs, so a span opened inside a
        // handler would silently never be recorded.
        let cache_counts = store.cache.counts();
        for _ in 0..cache_counts.hits {
            invocation.record_lookup("cache", true);
        }
        for _ in 0..cache_counts.misses {
            invocation.record_lookup("cache", false);
        }
        // Bytes are a property of the counter, not of any one lookup, so they
        // are added once rather than divided across the loops above. A run
        // that saved nothing adds nothing.
        if cache_counts.bytes_written > 0 {
            invocation.record_lookup_bytes("cache", true, cache_counts.bytes_written);
        }
    }
    let action_counts = config.actions.store.counts();
    for _ in 0..action_counts.hits {
        invocation.record_lookup("action-fetch", true);
    }
    for _ in 0..action_counts.misses {
        invocation.record_lookup("action-fetch", false);
    }
    let node_runtime_counts = config.actions.node_runtime_store.counts();
    for _ in 0..node_runtime_counts.hits {
        invocation.record_lookup("action-runtime-fetch", true);
    }
    for _ in 0..node_runtime_counts.misses {
        invocation.record_lookup("action-runtime-fetch", false);
    }

    render_run_table(&report, &mut out).map_err(|error| {
        anyhow::anyhow!(
            "could not write the run summary: {error}\n  fix: ensure stdout is writable"
        )
    })?;
    let conclusion = if report.failed() {
        ExecutionConclusion::Failed
    } else {
        ExecutionConclusion::Passed
    };
    let result = evidence.write_result(
        conclusion,
        run_lock.compatibility.clone(),
        clean,
        args.hermetic,
    )?;
    writeln!(
        io::stderr(),
        "evidence: {} ({:?}/{:?}/{:?})",
        evidence.run_id,
        result.conclusion,
        result.compatibility,
        result.assurance
    )
    .map_err(|error| {
        anyhow::anyhow!(
            "could not report the run evidence identity: {error}\n  fix: ensure stderr is writable"
        )
    })?;

    if args.write_back {
        // Every ran job kept its container alive (`RunConfig::write_back`,
        // `JobReport::container_id`) specifically so its overlay diff can be
        // exported here, after the whole run is known. Each job's diff is
        // independent (own read-only lower + own throwaway upper), so each
        // gets its own listing and confirmation, in the run's job order;
        // containers are removed once write-back has finished with them
        // (or if the run itself failed the writeback loop still runs, since
        // a failed job's earlier steps may still have produced a diff worth
        // reviewing).
        let target = args.job.as_deref().ok_or_else(|| {
            anyhow::anyhow!("`--write-back` has no selected job\n  fix: add `--job <job-id>`")
        })?;
        let write_back_result =
            runtime.block_on(write_back_one(&engine, &report, &repo_root, target));
        // Best-effort teardown of every preserved container, regardless of
        // whether write-back itself succeeded — a leaked container must
        // never be the difference between a successful and failed `run`.
        for job in &report.jobs {
            if let Some(container) = &job.container_id {
                let _ = runtime.block_on(engine.remove_container(container));
            }
        }
        write_back_result?;
    }

    Ok(if report.failed() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn open_content_store() -> anyhow::Result<greenlit_store::cas::CasStore> {
    let home = std::env::var_os("HOME").ok_or_else(|| {
        anyhow::anyhow!(
            "could not determine the user home directory (HOME is not set)\n  fix: set HOME, then retry"
        )
    })?;
    let home = std::path::Path::new(&home);
    if !home.is_absolute() {
        anyhow::bail!(
            "could not determine the user home directory (HOME is not absolute)\n  fix: set HOME to an absolute path, then retry"
        );
    }
    greenlit_store::cas::CasStore::open(greenlit_store::cas::CasStore::default_path_under(home))
        .map_err(|error| {
            anyhow::anyhow!(
                "could not open the verified content store: {error}\n  fix: ensure HOME has free space and is writable, then retry"
            )
        })
}

/// Export, list, confirm, and apply the selected job's overlay diff.
///
/// # Errors
///
/// Returns an error when the selected job's export/apply fails.
async fn write_back_one(
    engine: &DockerEngine,
    report: &RunReport,
    repo_root: &std::path::Path,
    target: &str,
) -> anyhow::Result<()> {
    for job in report.jobs.iter().filter(|job| job.id == target) {
        let Some(container) = &job.container_id else {
            continue;
        };
        let stdin = io::stdin();
        let mut confirm = InteractiveConfirm::new(stdin.lock(), io::stderr(), repo_root);
        let outcome = run_write_back(engine, container, repo_root, &mut confirm)
            .await
            .map_err(|error| anyhow::anyhow!("write-back failed for job '{}': {error}", job.id))?;
        match outcome {
            WriteBackOutcome::NoChanges => {
                println!("write-back: job '{}' made no changes", job.id);
            }
            WriteBackOutcome::Cancelled => {
                println!("write-back: job '{}' changes were not applied", job.id);
            }
            WriteBackOutcome::Applied(changes) => {
                println!(
                    "write-back: applied {} change(s) from job '{}'",
                    changes.len(),
                    job.id
                );
            }
        }
    }
    Ok(())
}

/// Detect and connect to the container engine, mapping every failure state to a
/// message plus the one action that fixes it (`AGENTS.md` UX invariant).
async fn connect_engine() -> anyhow::Result<DockerEngine> {
    match detect(&SystemProber::new()).await {
        EngineState::Available { endpoint } => {
            DockerEngine::connect(&endpoint).map_err(|error| {
                anyhow::anyhow!(
                    "reached the container engine at {} but could not open a client: {error}\n  fix: restart the daemon, then retry",
                    endpoint.describe()
                )
            })
        }
        EngineState::DaemonStopped(fix)
        | EngineState::NotInstalled(fix)
        | EngineState::UnsupportedDockerHost(fix) => {
            Err(anyhow::anyhow!("{}\n  fix: {}", fix.message, fix.action))
        }
    }
}

/// Builds the real (network-capable) action-resolution/fetch/runtime
/// configuration `litci run` hands the executor.
///
/// Picks [`greenlit_actions::resolve::GitHubApiResolver`] when a token is
/// available, [`greenlit_actions::resolve::GitLsRemoteResolver`] otherwise
/// (`PHASE-3-actions.md` "Action execution"), and the standard tarball-
/// then-git-clone [`greenlit_actions::store::FallbackFetcher`] composition
/// its own module docs recommend.
///
/// # Errors
/// Returns a message (with a fix, per `AGENTS.md`'s UX invariant) if the
/// user home directory cannot be determined.
pub(crate) fn build_action_runtime_config(
    token: Option<String>,
    offline: bool,
) -> Result<
    (
        greenlit_runtime::ActionRuntimeConfig,
        std::sync::Arc<greenlit_actions::resolve::PinnedRefResolver>,
    ),
    String,
> {
    use greenlit_actions::resolve::{
        GitHubApiResolver, GitLsRemoteResolver, PersistentRefResolver, PinnedRefResolver,
        RefResolver,
    };
    use greenlit_actions::store::{
        ActionFetcher, ActionStore, FallbackFetcher, GitCloneFetcher, OfflineActionFetcher,
        TarballFetcher,
    };
    use greenlit_runtime::HttpRuntimeBundleFetcher;
    use greenlit_runtime::executor::actions::node_runtime::{PinnedNodeBundleSpecs, RuntimeStore};
    use std::sync::Arc;

    let home = std::env::var_os("HOME").ok_or_else(|| {
        "could not determine the user home directory (HOME is not set)\n  fix: set HOME, then retry"
            .to_string()
    })?;
    let home = std::path::Path::new(&home);
    let cas = greenlit_store::cas::CasStore::open(
        greenlit_store::cas::CasStore::default_path_under(home),
    )
    .map_err(|error| {
        format!(
            "could not open the verified content store: {error}\n  fix: ensure HOME has free space and is writable, then retry"
        )
    })?;
    let store = ActionStore::with_cas(ActionStore::default_path_under(home), cas.clone());
    let node_runtime_store =
        RuntimeStore::with_cas(RuntimeStore::default_path_under(home), cas.clone(), offline);

    let inner_resolver: Arc<dyn RefResolver> = match &token {
        Some(t) => Arc::new(GitHubApiResolver::new(t.clone())),
        None => Arc::new(GitLsRemoteResolver::new()),
    };
    let inner_resolver = Arc::new(PersistentRefResolver::new(inner_resolver, cas, offline));
    let fetcher: Arc<dyn ActionFetcher> = if offline {
        Arc::new(OfflineActionFetcher)
    } else {
        match &token {
            Some(t) => Arc::new(FallbackFetcher::new(
                TarballFetcher::with_token(t.clone()),
                GitCloneFetcher::new(),
            )) as Arc<dyn ActionFetcher>,
            None => Arc::new(FallbackFetcher::new(
                TarballFetcher::new(),
                GitCloneFetcher::new(),
            )),
        }
    };
    let resolver = Arc::new(PinnedRefResolver::new(inner_resolver));

    Ok((
        greenlit_runtime::ActionRuntimeConfig {
            resolver: resolver.clone(),
            store,
            fetcher,
            node_runtime_fetcher: Arc::new(HttpRuntimeBundleFetcher::new()),
            node_runtime_specs: Arc::new(PinnedNodeBundleSpecs),
            node_runtime_store,
            github_token: token,
        },
        resolver,
    ))
}

/// Opens the local cache, artifact, and toolcache stores for this run.
///
/// A machine whose `HOME` cannot be resolved runs without them, and
/// `actions/cache` then behaves as it does on a runner with no cache service
/// — an honest miss rather than a broken run.
///
/// The runtime token is per-run and never leaves the machine: it exists so a
/// container other than this run's cannot read this run's cache through the
/// shim, which is reachable from the job network.
fn build_store_config(clean: bool, hermetic: bool) -> Option<greenlit_runtime::StoreConfig> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    if !home.is_absolute() {
        return None;
    }
    let minted = crate::runtime_token::mint()?;
    Some(greenlit_runtime::StoreConfig {
        cache: greenlit_store::CacheStore::at(greenlit_store::CacheStore::default_path_under(
            &home,
        )),
        artifacts: greenlit_store::ArtifactStore::at(
            greenlit_store::ArtifactStore::default_path_under(&home),
        ),
        toolcache_root: home.join(".litci").join("toolcache"),
        package_cache_root: home.join(".litci").join("package-cache"),
        serve_mutable_caches: !clean,
        allow_external_network: !hermetic,
        runtime_token: minted.value,
        url_signature: minted.url_signature,
    })
}

/// Build the runner/`github` environment template from local git metadata.
fn build_runner_env(
    git: &greenlit_engine::git::GitContext,
    event_kind: greenlit_engine::EventKind,
    workflow_name: String,
    workspace: String,
) -> RunnerEnv {
    RunnerEnv {
        workflow: workflow_name,
        repository: git.repository.clone(),
        repository_owner: git.repository_owner.clone(),
        sha: git.sha.clone(),
        ref_full: format!("refs/heads/{}", git.branch),
        ref_name: git.branch.clone(),
        ref_type: "branch".to_string(),
        event_name: event_kind.event_name().to_string(),
        actor: git.actor.clone(),
        job: String::new(),
        run_id: "1".to_string(),
        run_number: "1".to_string(),
        run_attempt: "1".to_string(),
        workspace,
        runner_name: "greenlit".to_string(),
        runner_temp: "/tmp".to_string(),
        runner_tool_cache: "/opt/hostedtoolcache".to_string(),
        actions_service: None,
    }
}

/// Prune `plan` to the requested job and its transitive `needs:` closure.
fn prune_to_job(
    plan: &ExecutionPlan,
    job: &str,
    selected_matrix: &[(String, String)],
    write_back: bool,
) -> anyhow::Result<ExecutionPlan> {
    let target = JobId(job.to_string());
    if !plan.jobs.iter().any(|candidate| candidate.id == target) {
        anyhow::bail!(
            "no job '{job}' in this workflow\n  fix: pass -j with a job id defined under `jobs:`"
        );
    }
    let mut keep: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<JobId> = VecDeque::from([target]);
    while let Some(id) = queue.pop_front() {
        if !keep.insert(id.0.clone()) {
            continue;
        }
        if let Some(job_plan) = plan.jobs.iter().find(|candidate| candidate.id == id) {
            for need in &job_plan.needs {
                queue.push_back(need.clone());
            }
        }
    }
    let mut pruned = plan.clone();
    pruned
        .jobs
        .retain(|candidate| keep.contains(&candidate.id.0));
    pruned.topo_order.retain(|id| keep.contains(&id.0));
    let selected = parse_matrix_filter(selected_matrix)?;
    let target = pruned
        .jobs
        .iter_mut()
        .find(|candidate| candidate.id.0 == job)
        .ok_or_else(|| anyhow::anyhow!("selected job disappeared while pruning"))?;
    if selected.is_some() && target.strategy.matrix.is_none() {
        anyhow::bail!(
            "job '{job}' is not a matrix job\n  fix: omit `--matrix`, or select a job with `strategy.matrix`"
        );
    }
    if write_back && target.strategy.matrix.is_some() && selected.is_none() {
        anyhow::bail!(
            "`--write-back` needs one exact matrix case for job '{job}'\n  fix: repeat `--matrix KEY=JSON_VALUE` for every property in the desired case"
        );
    }
    if let (Some(filter), Some(MatrixPlan::Static { legs, .. })) =
        (&selected, &target.strategy.matrix)
    {
        let matches = legs
            .iter()
            .filter(|leg| matrix_leg_matches(leg, filter))
            .count();
        if matches != 1 {
            anyhow::bail!(
                "matrix selection for job '{job}' matched {matches} cases\n  fix: specify every matrix property so exactly one case matches"
            );
        }
    }
    target.matrix_filter = selected;
    Ok(pruned)
}

fn parse_matrix_filter(
    selected: &[(String, String)],
) -> anyhow::Result<Option<indexmap::IndexMap<String, MatrixValue>>> {
    if selected.is_empty() {
        return Ok(None);
    }
    let mut filter = indexmap::IndexMap::new();
    for (key, raw) in selected {
        if filter.contains_key(key) {
            anyhow::bail!(
                "matrix property '{key}' was selected more than once\n  fix: pass each `--matrix KEY=JSON_VALUE` once"
            );
        }
        let value = serde_json::from_str::<serde_json::Value>(raw)
            .unwrap_or_else(|_| serde_json::Value::String(raw.clone()));
        filter.insert(key.clone(), json_matrix_value(&value)?);
    }
    Ok(Some(filter))
}

fn json_matrix_value(value: &serde_json::Value) -> anyhow::Result<MatrixValue> {
    Ok(match value {
        serde_json::Value::Null => MatrixValue::Null,
        serde_json::Value::Bool(value) => MatrixValue::Bool(*value),
        serde_json::Value::Number(value) => MatrixValue::Number(value.as_f64().ok_or_else(|| {
            anyhow::anyhow!(
                "matrix numeric selector is outside the supported range\n  fix: use a finite JSON number"
            )
        })?),
        serde_json::Value::String(value) => MatrixValue::String(value.as_str().into()),
        serde_json::Value::Array(values) => MatrixValue::Sequence(
            values
                .iter()
                .map(json_matrix_value)
                .collect::<anyhow::Result<Vec<_>>>()?
                .into(),
        ),
        serde_json::Value::Object(values) => MatrixValue::Mapping(
            values
                .iter()
                .map(|(key, value)| Ok((key.clone(), json_matrix_value(value)?)))
                .collect::<anyhow::Result<indexmap::IndexMap<_, _>>>()?
                .into(),
        ),
    })
}

fn matrix_leg_matches(
    leg: &greenlit_engine::MatrixLeg,
    filter: &indexmap::IndexMap<String, MatrixValue>,
) -> bool {
    leg.values.len() == filter.len()
        && filter.iter().all(|(key, value)| {
            leg.values
                .get(key.as_str())
                .is_some_and(|actual| actual == value)
        })
}

/// Render the end-of-run table: each job, its steps' outcomes and durations,
/// then the overall result.
fn render_run_table(report: &RunReport, out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "\nRun summary")?;
    for job in &report.jobs {
        writeln!(out, "  {} [{}]", job.display, job.result.as_github_str())?;
        for step in &job.steps {
            writeln!(
                out,
                "    {} {}  {}",
                step_sigil(step),
                step.label,
                format_duration(step)
            )?;
        }
    }
    writeln!(out, "\nResult: {}", report.overall.as_github_str())?;
    Ok(())
}

/// A short sigil for a step's conclusion in the summary table.
fn step_sigil(step: &StepReport) -> &'static str {
    match step.conclusion {
        Conclusion::Success => "\u{2713}",
        Conclusion::Failure => "\u{2717}",
        Conclusion::Cancelled => "\u{29b8}",
        Conclusion::Skipped => "\u{2013}",
    }
}

/// Format a step's duration, or a dash for a step that did not run.
fn format_duration(step: &StepReport) -> String {
    if !step.ran {
        return "—".to_string();
    }
    let millis = step.duration.as_millis();
    if millis >= 1000 {
        format!("{:.1}s", step.duration.as_secs_f64())
    } else {
        format!("{millis}ms")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_back_requires_overlay_never_auto() {
        // `auto` may fall back to copy-in at container start, which would turn
        // `--write-back` into a silent no-op; requesting write-back must
        // therefore pin the strategy to a required overlay.
        assert_eq!(
            resolved_strategy(IsolationArg::Auto, true),
            IsolationStrategy::Overlay
        );
        assert_eq!(
            resolved_strategy(IsolationArg::Overlay, true),
            IsolationStrategy::Overlay
        );
    }

    #[test]
    fn without_write_back_the_requested_isolation_passes_through() {
        assert_eq!(
            resolved_strategy(IsolationArg::Auto, false),
            IsolationStrategy::Auto
        );
        assert_eq!(
            resolved_strategy(IsolationArg::Overlay, false),
            IsolationStrategy::Overlay
        );
        assert_eq!(
            resolved_strategy(IsolationArg::CopyIn, false),
            IsolationStrategy::CopyIn
        );
    }
}
