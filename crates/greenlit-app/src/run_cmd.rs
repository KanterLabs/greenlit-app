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

use std::io;
use std::process::ExitCode;

use greenlit_engine::execution::env::RunnerEnv;
use greenlit_engine::git::find_repository_root;
use greenlit_engine::{Conclusion, ExecutionConclusion, analyze_support};
use greenlit_expr::Value;
use greenlit_metrics::{Invocation, MetricsStore};
use greenlit_runtime::{
    Cancellation, ContainerEngine, DockerEngine, EngineState, InteractiveConfirm,
    IsolationStrategy, RunConfig, RunReport, RuntimeAuthorization, RuntimeControl, SystemProber,
    WriteBackOutcome, detect, reject_hermetic_late_inputs, reject_uses_steps,
    run_plan_with_events_cancellable, run_write_back, validate_host, validate_request,
};

use crate::cli::{IsolationArg, RunArgs};
use crate::{errors, render, workflow_discovery, workflow_picker};

/// Run the command, returning the process exit code (a failed workflow run
/// exits non-zero without an error, since its table is the real output).
pub(crate) fn run(args: RunArgs) -> anyhow::Result<ExitCode> {
    let invocation = Invocation::start("run");
    let outcome = invocation.with_timing_subscriber(|| execute(&args, &invocation));

    let record = invocation.finish();
    let _ = render::diagnostics::render_timings(&record, &mut io::stderr());
    let metrics = MetricsStore::open_default()
        .and_then(|store| store.append(&record))
        .map_err(|error| errors::metrics_error(&error));
    match (outcome, metrics) {
        (Ok(code), Ok(())) => Ok(code),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(primary), Err(metrics_error)) => {
            let warning = format!(
                "warning: could not append the sanitized local run metric: {metrics_error}\n"
            );
            let _ = crate::render::terminal::write_sanitized(&mut io::stderr(), &warning);
            Err(primary)
        }
    }
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
    let cli_sensitive_values = crate::run_quarantine::explicit_sensitive_values(args);
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
    let workflow_path =
        workflow_discovery::resolve_workflow_path(args.workflow.as_deref(), &cwd, &repo_root)
            .and_then(|resolution| {
                workflow_picker::resolve_or_pick(resolution, &repo_root, !args.no_input)
            })
            .map_err(|message| anyhow::anyhow!(message))?;
    let evidence = invocation.time_stage("source-freeze", || {
        crate::run_evidence::RunEvidence::capture(&repo_root, &cli_sensitive_values)
    })?;
    let frozen_repo_root = evidence.source.root.clone();
    let workflow = invocation.time_stage("parse", || {
        greenlit_workflow::parse_workflow_file_with_name(
            frozen_repo_root.join(&workflow_path.source_name),
            workflow_path.source_name.clone(),
        )
        .map_err(|error| errors::parse_error(&error))
    })?;
    let local_variables = crate::run_quarantine::LocalVariables::read(&repo_root)?;
    let prepared = invocation.time_stage("plan", || {
        crate::run_quarantine::prepare(workflow, &frozen_repo_root, args, &local_variables)
    })?;
    let recorder = crate::run_events::RunEventRecorder::create(
        &evidence.directory,
        &evidence.run_id,
        args.format,
        args.log_mode,
        args.color,
        evidence.result_publication_gate(),
    )?;
    recorder.preparation_finished("source snapshot", Some(evidence.source.digest.clone()))?;
    evidence.apply_execution_policy(clean, args.hermetic)?;
    let crate::run_quarantine::PreparedQuarantine {
        workflow,
        git,
        event,
        plan: execution_plan,
        vars: vars_value,
        assessment,
    } = prepared;
    recorder.preparation_finished("workflow", Some(workflow_path.source_name.clone()))?;
    let mut support = analyze_support(&workflow);
    support
        .findings
        .extend(crate::run_quarantine::evidence_findings(&assessment));
    support.canonicalize();
    evidence.merge_support(&support)?;
    recorder.compatibility_findings(&evidence.support_report())?;
    recorder.preparation_finished("execution plan", None)?;
    recorder.preparation_finished("compatibility", None)?;
    if let Err(error) = crate::run_quarantine::reject_blocked(&assessment) {
        let support = evidence.support_report();
        publish_terminal_result(
            &recorder,
            &evidence,
            ExecutionConclusion::Blocked,
            support,
            clean,
            args.hermetic,
        )?;
        return Err(error);
    }
    crate::run_quarantine::render_degraded(&assessment);

    let repo_leaf = git
        .repository
        .rsplit('/')
        .next()
        .unwrap_or(&git.repository)
        .to_string();
    let event_kind: greenlit_engine::EventKind = args.event.into();
    reject_uses_steps(&execution_plan).map_err(|error| anyhow::anyhow!("{error}"))?;
    if args.hermetic {
        reject_hermetic_late_inputs(&execution_plan, &git.repository)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
    }

    let workflow_name = workflow
        .name
        .as_ref()
        .map(|name| name.value.clone())
        .unwrap_or_else(|| workflow_path.source_name.clone());
    let workspace = format!("/home/runner/work/{repo_leaf}/{repo_leaf}");
    let all_secrets: Vec<(String, String)> = Vec::new();
    let secrets_value = Value::object(Vec::<(String, Value)>::new());
    let (actions_config, pinned_resolver) = invocation
        .time_stage("action-resolve", || {
            build_action_runtime_config(None, args.offline)
        })
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
    recorder.preparation_finished(
        "actions",
        Some(format!("{} locked", action_preflight.actions.len())),
    )?;
    invocation.time_stage("action-resolve", || -> anyhow::Result<()> {
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
        Ok(())
    })?;
    let engine = invocation.time_stage("detection", || runtime.block_on(connect_engine()))?;
    recorder.preparation_finished("container runtime", None)?;
    let runtime_fingerprint = invocation
        .time_stage("runtime-fingerprint", || {
            runtime.block_on(engine.runtime_fingerprint())
        })
        .map_err(|error| {
            anyhow::anyhow!(
                "could not fingerprint the container runtime: {error}\n  fix: restart the local container daemon, then retry"
            )
        })?;
    let content_store = invocation.time_stage("content-store-open", open_content_store)?;
    let mut progress = recorder.clone();
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
    recorder.preparation_finished(
        "containers",
        Some(format!("{} locked", container_locks.len())),
    )?;
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
    recorder.preparation_finished("runners", Some(format!("{} locked", runner_locks.len())))?;
    let run_lock = invocation.time_stage("run-lock", || {
        evidence.lock(crate::run_evidence::LockInputs {
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
        })
    })?;
    recorder.preparation_finished("RunLock", Some(run_lock.source.snapshot_digest.clone()))?;

    // The local stores this run serves. A machine whose `HOME` cannot be
    // resolved simply runs without them -- `actions/cache` then behaves as it
    // does on a runner with no cache service, which is an honest miss rather
    // than a broken run.
    let store_config = build_store_config(clean, args.hermetic);
    let mut initial_masks = cli_sensitive_values;
    if let Some(store) = &store_config {
        // These shim capabilities authenticate workflow-visible services and
        // can be reflected just like repository secrets. Register them before
        // the first step can emit a log or structured event.
        let store_sensitive_values = [store.runtime_token.clone(), store.url_signature.clone()];
        evidence.register_sensitive_values(&store_sensitive_values);
        initial_masks.extend(store_sensitive_values);
    }

    let mut execution_image_locks = run_lock
        .containers
        .iter()
        .map(|(requested, digest)| {
            greenlit_store::oci::immutable_pull_reference(requested, digest)
                .map(|reference| (requested.clone(), reference))
                .map_err(|error| {
                    anyhow::anyhow!(
                        "could not reconstruct locked container reference '{requested}': {error}\n  fix: preserve the run directory and file a Greenlit defect"
                    )
                })
        })
        .collect::<anyhow::Result<std::collections::BTreeMap<_, _>>>()?;
    execution_image_locks.extend(run_lock.runners.iter().map(|(key, runner)| {
        (
            format!("__greenlit_runner:{key}"),
            runner.image_reference.clone(),
        )
    }));
    let config = RunConfig {
        repo_host_path: frozen_repo_root,
        workspace: workspace.clone(),
        strategy: resolved_strategy(args.isolation, args.write_back),
        runner_env: build_runner_env(&git, event_kind, workflow_name, workspace),
        github: event.github.clone(),
        vars: vars_value,
        inputs: event.inputs.clone(),
        secrets: secrets_value,
        // Every credential this run minted or resolved is masked before any
        // step runs, including the cache/artifact service capabilities.
        initial_masks,
        volume_namespace: evidence.run_id.clone(),
        locked_images: Some(execution_image_locks),
        write_back: args.write_back,
        dind: false,
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

    let mut logs = recorder.clone();
    let mut events = recorder.clone();
    let cancellation = Cancellation::new();
    let authorization = if args.allow_degraded {
        RuntimeAuthorization::AllowDegradedShell
    } else {
        RuntimeAuthorization::Enforce
    };
    let execution_result = runtime.block_on(async {
        let execution = run_plan_with_events_cancellable(
            &engine,
            &execution_plan,
            &config,
            RuntimeControl::with_assessment(authorization, &cancellation, &assessment),
            &mut logs,
            &mut events,
            &mut progress,
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
    });
    let report = match execution_result {
        Ok(report) => report,
        Err(error @ greenlit_runtime::ExecError::CapabilityQuarantined { .. }) => {
            let support = evidence.support_report();
            publish_terminal_result(
                &recorder,
                &evidence,
                ExecutionConclusion::Blocked,
                support,
                clean,
                args.hermetic,
            )?;
            return Err(anyhow::anyhow!("{error}"));
        }
        Err(error) => {
            let support = evidence.support_report();
            publish_terminal_result(
                &recorder,
                &evidence,
                ExecutionConclusion::PreparationFailed,
                support,
                clean,
                args.hermetic,
            )?;
            return Err(anyhow::anyhow!("{error}"));
        }
    };

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
        recorder.cache_summary("workflow cache", cache_counts.hits, cache_counts.misses)?;
    }
    let action_counts = config.actions.store.counts();
    for _ in 0..action_counts.hits {
        invocation.record_lookup("action-fetch", true);
    }
    for _ in 0..action_counts.misses {
        invocation.record_lookup("action-fetch", false);
    }
    recorder.cache_summary("actions", action_counts.hits, action_counts.misses)?;
    let node_runtime_counts = config.actions.node_runtime_store.counts();
    for _ in 0..node_runtime_counts.hits {
        invocation.record_lookup("action-runtime-fetch", true);
    }
    for _ in 0..node_runtime_counts.misses {
        invocation.record_lookup("action-runtime-fetch", false);
    }
    recorder.cache_summary(
        "action runtimes",
        node_runtime_counts.hits,
        node_runtime_counts.misses,
    )?;

    let conclusion = match report.overall {
        Conclusion::Success | Conclusion::Skipped => ExecutionConclusion::Passed,
        Conclusion::Failure => ExecutionConclusion::Failed,
        Conclusion::Cancelled => ExecutionConclusion::Canceled,
    };
    publish_terminal_result(
        &recorder,
        &evidence,
        conclusion,
        run_lock.compatibility.clone(),
        clean,
        args.hermetic,
    )?;

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

fn publish_terminal_result(
    recorder: &crate::run_events::RunEventRecorder,
    evidence: &crate::run_evidence::RunEvidence,
    conclusion: ExecutionConclusion,
    support: greenlit_engine::SupportReport,
    clean: bool,
    hermetic: bool,
) -> anyhow::Result<()> {
    if let Err(error) = recorder.flush_pending_logs() {
        evidence.abandon_result_publication();
        return Err(error);
    }
    let prepared = match evidence.prepare_result(conclusion, support, clean, hermetic) {
        Ok(prepared) => prepared,
        Err(error) => {
            evidence.abandon_result_publication();
            return Err(error);
        }
    };
    if let Err(error) = recorder.finish(
        prepared.terminal_conclusion(),
        prepared.terminal_compatibility(),
        prepared.terminal_assurance(),
    ) {
        evidence.abandon_result_publication();
        return Err(error);
    }
    if let Err(error) = recorder.verify_durable() {
        evidence.abandon_result_publication();
        return Err(error);
    }
    if let Err(error) = evidence.publish_prepared_result(prepared) {
        evidence.abandon_result_publication();
        return Err(error);
    }
    Ok(())
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
