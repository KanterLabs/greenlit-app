//! Persistent source, lock, and result evidence for one invocation.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};

use greenlit_engine::{
    ExecutionConclusion, ExecutionPlan, ExecutionResultV1, FeatureFinding, FindingDisposition,
    JobLockV1, LockedSource, MatrixKey, MatrixValue, ResultEvidence, RunLockV1,
    SUPPORT_CERTIFICATION_WITNESS, SourceSnapshot, SourceSnapshotError, SupportReport,
    TraceEventV1, opaque_revision,
};
use indexmap::IndexMap;
use rustix::fs::{Mode, RenameFlags, mkdirat, renameat_with};
use rustix::io::Errno;

mod private_fs;

pub(crate) use private_fs::create_private_artifact;
use private_fs::{
    create_new_private_directory, create_private_file_at, evidence_write_error,
    open_new_private_directory_at, open_private_directory_at, open_private_file_at,
    prepare_runs_directory, reject_unsafe_existing_file, unsafe_path_error,
};

pub(crate) struct RunEvidence {
    pub(crate) run_id: String,
    pub(crate) directory: PathBuf,
    pub(crate) source: SourceSnapshot,
    runs_handle: File,
    directory_handle: File,
    next_trace_sequence: Cell<u64>,
    terminal_result_written: Cell<bool>,
    command_finalized: Cell<bool>,
    result_publication_abandoned: Arc<AtomicBool>,
    masker: greenlit_engine::execution::Masker,
    support: RefCell<SupportReport>,
    content_store: greenlit_store::cas::CasStore,
    _publication_guard: greenlit_store::cas::RunPublicationGuard,
}

pub(crate) struct PreparedResult {
    result_bytes: Vec<u8>,
    completed_trace: PreparedTrace,
    terminal_conclusion: String,
    terminal_compatibility: String,
    terminal_assurance: String,
}

struct PreparedTrace {
    sequence: u64,
    bytes: Vec<u8>,
}

struct SourceAdoptionContext<'a> {
    repo_root: &'a Path,
    home: &'a Path,
    home_handle: &'a File,
    run_id: &'a str,
    runs_handle: &'a File,
    directory_handle: &'a File,
    directory: &'a Path,
}

impl PreparedResult {
    pub(crate) fn terminal_conclusion(&self) -> &str {
        &self.terminal_conclusion
    }

    pub(crate) fn terminal_compatibility(&self) -> &str {
        &self.terminal_compatibility
    }

    pub(crate) fn terminal_assurance(&self) -> &str {
        &self.terminal_assurance
    }
}

pub(crate) struct LockInputs<'a> {
    pub(crate) workflow_path: &'a str,
    pub(crate) event_name: &'a str,
    pub(crate) inputs: &'a [(String, String)],
    pub(crate) selected_job: Option<&'a str>,
    pub(crate) selected_matrix: &'a [(String, String)],
    pub(crate) offline: bool,
    pub(crate) clean: bool,
    pub(crate) hermetic: bool,
    pub(crate) runtime: &'a greenlit_runtime::RuntimeFingerprint,
    pub(crate) plan: &'a ExecutionPlan,
    pub(crate) secrets: &'a [(String, String)],
    pub(crate) actions: BTreeMap<String, String>,
    pub(crate) containers: BTreeMap<String, String>,
    pub(crate) runners: BTreeMap<String, greenlit_engine::RunnerLockV1>,
    pub(crate) toolchains: BTreeMap<String, String>,
}

impl RunEvidence {
    pub(crate) fn capture(
        repo_root: &Path,
        masker: greenlit_engine::execution::Masker,
    ) -> anyhow::Result<Self> {
        let sensitive_values = snapshot_bytes(&masker)?;
        let home = std::env::var_os("HOME").ok_or_else(|| {
            anyhow::anyhow!(
                "could not persist run evidence because HOME is not set\n  fix: set HOME to an absolute writable directory, then retry"
            )
        })?;
        let home = PathBuf::from(home);
        if !home.is_absolute() {
            anyhow::bail!(
                "could not persist run evidence because HOME is not absolute\n  fix: set HOME to an absolute writable directory, then retry"
            );
        }
        let (runs, runs_handle, home_handle) = prepare_runs_directory(&home)?;
        let content_store = greenlit_store::cas::CasStore::open(
            greenlit_store::cas::CasStore::default_path_under(&home),
        )
        .map_err(|error| {
            anyhow::anyhow!(
                "could not open the verified content store: {error}\n  fix: ensure HOME has free space and is writable, then retry"
            )
        })?;
        content_store
            .recover_incomplete_run_publications(&runs)
            .map_err(recovery_error)?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| {
                anyhow::anyhow!(
                    "could not create a run identity from the system clock: {error}\n  fix: correct the system clock, then retry"
                )
            })?
            .as_nanos();
        let mut selected = None;
        for suffix in 0_u16..=u16::MAX {
            let run_id = format!("{timestamp:032x}-{:08x}-{suffix:04x}", std::process::id());
            let directory = runs.join(&run_id);
            match mkdirat(
                &runs_handle,
                run_id.as_str(),
                Mode::RUSR | Mode::WUSR | Mode::XUSR,
            ) {
                Ok(()) => {
                    let handle =
                        open_new_private_directory_at(&runs_handle, &runs, run_id.as_str())?;
                    selected = Some((run_id, directory, handle));
                    break;
                }
                Err(Errno::EXIST) => {
                    let existing = open_private_directory_at(&runs_handle, &runs, run_id.as_str())?;
                    drop(existing);
                }
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "could not create run evidence directory {}: {error}\n  fix: make HOME writable, then retry",
                        directory.display()
                    ));
                }
            }
        }
        let (run_id, directory, directory_handle) = selected.ok_or_else(|| {
            anyhow::anyhow!(
                "could not allocate a unique run identity\n  fix: retry after the current invocation finishes"
            )
        })?;
        let publication_guard = content_store
            .acquire_run_publication_guard(&runs, &run_id)
            .map_err(lifecycle_error)?;
        let captured = (|| {
            let mut source = capture_and_adopt_source(
                SourceAdoptionContext {
                    repo_root,
                    home: &home,
                    home_handle: &home_handle,
                    run_id: &run_id,
                    runs_handle: &runs_handle,
                    directory_handle: &directory_handle,
                    directory: &directory,
                },
                &sensitive_values,
            )?;
            source.root = directory.join("source");
            let manifest_bytes =
                serialize_json_line(&directory.join("source-manifest.json"), &source.entries)?;
            scan_prepared_bytes(&sensitive_values, &[manifest_bytes.as_slice()])?;
            write_bytes_atomic(
                &directory_handle,
                &directory,
                "source-manifest.json",
                &manifest_bytes,
                &sensitive_values,
            )?;
            let evidence = Self {
                run_id: run_id.clone(),
                directory: directory.clone(),
                source,
                runs_handle: runs_handle
                    .try_clone()
                    .map_err(|error| evidence_write_error(&runs, error))?,
                directory_handle: directory_handle
                    .try_clone()
                    .map_err(|error| evidence_write_error(&directory, error))?,
                next_trace_sequence: Cell::new(1),
                terminal_result_written: Cell::new(false),
                command_finalized: Cell::new(false),
                result_publication_abandoned: Arc::new(AtomicBool::new(false)),
                masker,
                support: RefCell::new(local_support_report()),
                content_store,
                _publication_guard: publication_guard,
            };
            evidence.append_trace(
                "source_locked",
                BTreeMap::from([
                    (
                        "snapshot_digest".to_string(),
                        evidence.source.digest.clone(),
                    ),
                    ("dirty".to_string(), evidence.source.dirty.to_string()),
                ]),
            )?;
            Ok(evidence)
        })();
        match captured {
            Ok(evidence) => Ok(evidence),
            Err(error) => {
                remove_sensitive_failed_capture(
                    &runs_handle,
                    &directory_handle,
                    &directory,
                    &sensitive_values,
                )
                .map_err(|cleanup| {
                    sensitive_cleanup_error("failed source capture", &error, cleanup)
                })?;
                Err(error)
            }
        }
    }

    pub(crate) fn merge_support(&self, report: &SupportReport) -> anyhow::Result<()> {
        {
            let mut support = self.support.borrow_mut();
            support.findings.extend(report.findings.iter().cloned());
            support.findings.push(FeatureFinding {
                code: SUPPORT_CERTIFICATION_WITNESS.to_string(),
                disposition: FindingDisposition::Supported,
                scope: "run".to_string(),
                reason:
                    "the frozen selected plan was evaluated by the active stabilization registry"
                        .to_string(),
            });
            support.canonicalize();
        }
        self.append_trace(
            "compatibility_analyzed",
            BTreeMap::from([(
                "compatibility".to_string(),
                format!("{:?}", self.support.borrow().compatibility()),
            )]),
        )
    }

    pub(crate) fn support_report(&self) -> SupportReport {
        self.support.borrow().clone()
    }

    fn current_sensitive_values(&self) -> anyhow::Result<Vec<Vec<u8>>> {
        snapshot_bytes(&self.masker)
    }

    pub(crate) fn abandon_result_publication(&self) {
        self.result_publication_abandoned
            .store(true, Ordering::Release);
    }

    pub(crate) fn result_publication_gate(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.result_publication_abandoned)
    }

    pub(crate) fn discard_uncommitted_tree(&self) -> anyhow::Result<()> {
        self.remove_sensitive_retained_tree()
    }

    pub(crate) fn apply_execution_policy(&self, clean: bool, hermetic: bool) -> anyhow::Result<()> {
        {
            let mut support = self.support.borrow_mut();
            if hermetic {
                support
                    .findings
                    .retain(|finding| finding.code != "network.external_uncaptured");
                support.findings.push(FeatureFinding {
                    code: "network.external_blocked".to_string(),
                    disposition: FindingDisposition::Supported,
                    scope: "run".to_string(),
                    reason: "workflow external traffic is blocked by the job namespace policy"
                        .to_string(),
                });
            }
            if clean {
                support.findings.push(FeatureFinding {
                    code: "cache.greenlit_mutable_disabled".to_string(),
                    disposition: FindingDisposition::Supported,
                    scope: "run".to_string(),
                    reason: "transparent Greenlit cache, artifact, and toolcache reuse is disabled"
                        .to_string(),
                });
            }
            support.canonicalize();
        }
        self.append_trace(
            "execution_policy_selected",
            BTreeMap::from([
                ("clean".to_string(), clean.to_string()),
                ("hermetic".to_string(), hermetic.to_string()),
            ]),
        )
    }

    pub(crate) fn lock(&self, inputs: LockInputs<'_>) -> anyhow::Result<RunLockV1> {
        let LockInputs {
            workflow_path,
            event_name,
            inputs,
            selected_job,
            selected_matrix,
            offline,
            clean,
            hermetic,
            runtime,
            plan,
            secrets,
            actions,
            containers,
            runners,
            toolchains,
        } = inputs;
        let workflow_digest = self
            .source
            .entries
            .iter()
            .find(|entry| entry.path == workflow_path)
            .map(|entry| entry.digest.clone())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "selected workflow {workflow_path} is absent from the frozen source\n  fix: keep workflows inside the repository and do not ignore them"
                )
            })?;
        let source = LockedSource {
            commit: self.source.commit.clone(),
            snapshot_digest: self.source.digest.clone(),
            dirty: self.source.dirty,
            workflow_path: workflow_path.to_string(),
            workflow_digest,
        };
        let mut lock = RunLockV1::new(source, event_name);
        lock.inputs = inputs.iter().cloned().collect();
        lock.selected_job = selected_job.map(str::to_string);
        lock.selected_matrix = selected_matrix.iter().cloned().collect();
        lock.offline = offline;
        lock.clean = clean;
        lock.hermetic = hermetic;
        lock.runtime = BTreeMap::from([
            ("implementation".to_string(), runtime.implementation.clone()),
            ("version".to_string(), runtime.version.clone()),
            ("kernel".to_string(), runtime.kernel.clone()),
            ("snapshotter".to_string(), runtime.snapshotter.clone()),
            (
                "privileged_infrastructure".to_string(),
                runtime.privileged_infrastructure.join(","),
            ),
        ]);
        lock.runners = runners;
        lock.secret_revisions = secrets
            .iter()
            .map(|(name, value)| (name.clone(), opaque_revision(value.as_bytes())))
            .collect();
        lock.actions = actions;
        lock.containers = containers;
        lock.toolchains = toolchains;
        lock.compatibility = self.support.borrow().clone();
        let lock_digest = lock.digest().map_err(|error| {
            anyhow::anyhow!(
                "could not identify the finalized run lock: {error}\n  fix: preserve the run directory and retry"
            )
        })?;
        write_json_atomic(
            &self.directory_handle,
            &self.directory,
            "run-lock.json",
            &lock,
            &self.current_sensitive_values()?,
        )?;
        write_json_atomic(
            &self.directory_handle,
            &self.directory,
            "execution-plan.json",
            plan,
            &self.current_sensitive_values()?,
        )?;
        self.write_job_locks(plan, &lock)?;
        self.content_store
            .record_run_state(&self.run_id, Some(&lock_digest), "resolved")
            .map_err(lifecycle_error)?;
        self.append_trace(
            "run_lock_finalized",
            BTreeMap::from([
                ("digest".to_string(), lock_digest),
                ("offline".to_string(), lock.offline.to_string()),
                ("clean".to_string(), lock.clean.to_string()),
                ("hermetic".to_string(), lock.hermetic.to_string()),
            ]),
        )?;
        self.append_trace(
            "runtime_fingerprinted",
            BTreeMap::from([
                ("implementation".to_string(), runtime.implementation.clone()),
                ("version".to_string(), runtime.version.clone()),
                ("kernel".to_string(), runtime.kernel.clone()),
                ("snapshotter".to_string(), runtime.snapshotter.clone()),
                (
                    "privileged_infrastructure".to_string(),
                    runtime.privileged_infrastructure.join(","),
                ),
            ]),
        )?;
        Ok(lock)
    }

    pub(crate) fn prepare_result(
        &self,
        conclusion: ExecutionConclusion,
        support: SupportReport,
        clean: bool,
        hermetic: bool,
    ) -> anyhow::Result<PreparedResult> {
        if self.result_publication_abandoned.load(Ordering::Acquire) {
            anyhow::bail!(
                "could not publish a run result after retained event or output failure\n  fix: preserve the run directory and retry with writable output"
            );
        }
        let result = ExecutionResultV1::classify(&ResultEvidence {
            conclusion,
            support,
            clean,
            hermetic,
            github_confirmed: false,
        });
        let terminal_conclusion = format!("{:?}", result.conclusion);
        let terminal_compatibility = format!("{:?}", result.compatibility);
        let terminal_assurance = format!("{:?}", result.assurance);
        let result_bytes = serialize_json_line(&self.directory.join("result.json"), &result)?;
        let terminal_semantics = crate::run_events::RunEvent::RunFinished {
            conclusion: terminal_conclusion.clone(),
            compatibility: terminal_compatibility.clone(),
            assurance: terminal_assurance.clone(),
            evidence: self.run_id.clone(),
        };
        let terminal_bytes =
            serialize_json_line(&self.directory.join("events.ndjson"), &terminal_semantics)?;
        let completed_trace = self.prepare_trace(
            "run_completed",
            BTreeMap::from([
                ("conclusion".to_string(), terminal_conclusion.clone()),
                ("compatibility".to_string(), terminal_compatibility.clone()),
                ("assurance".to_string(), terminal_assurance.clone()),
            ]),
        )?;
        let sensitive_values = self.current_sensitive_values()?;
        let scan_result = crate::retained_secret_scan::scan_retained_run_and_prepared_bytes(
            &self.directory,
            sensitive_values.iter(),
            &[
                result_bytes.as_slice(),
                terminal_bytes.as_slice(),
                completed_trace.bytes.as_slice(),
            ],
        );
        if let Err(error) = scan_result {
            self.remove_sensitive_retained_tree().map_err(|cleanup| {
                sensitive_cleanup_error("rejected terminal publication", &error, cleanup)
            })?;
            return Err(error);
        }
        Ok(PreparedResult {
            result_bytes,
            completed_trace,
            terminal_conclusion,
            terminal_compatibility,
            terminal_assurance,
        })
    }

    pub(crate) fn publish_prepared_result(&self, prepared: PreparedResult) -> anyhow::Result<()> {
        self.masker
            .ensure_healthy()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        if self.result_publication_abandoned.load(Ordering::Acquire) {
            anyhow::bail!(
                "could not publish a run result after retained event or output failure\n  fix: preserve the run directory and retry with writable output"
            );
        }
        let publication = (|| {
            self.append_prepared_trace(prepared.completed_trace)?;
            write_bytes_atomic(
                &self.directory_handle,
                &self.directory,
                "result.json",
                &prepared.result_bytes,
                &self.current_sensitive_values()?,
            )?;
            hold_result_publication("after-directory-sync");
            self.content_store
                .record_run_state(&self.run_id, None, "completed")
                .map_err(lifecycle_error)?;
            self.terminal_result_written.set(true);
            Ok(())
        })();
        if let Err(error) = publication {
            self.abandon_result_publication();
            return match self.remove_sensitive_retained_tree() {
                Ok(()) => Err(error),
                Err(cleanup) => Err(sensitive_cleanup_error(
                    "failed result transaction",
                    &error,
                    cleanup,
                )),
            };
        }
        Ok(())
    }

    pub(crate) fn finalize_command<T>(
        &self,
        outcome: anyhow::Result<T>,
        publish_fallback: impl FnOnce() -> anyhow::Result<()>,
    ) -> anyhow::Result<T> {
        self.command_finalized.set(true);
        let abandoned = self.result_publication_abandoned.load(Ordering::Acquire);
        let published = self.terminal_result_written.get();
        if (!published || abandoned)
            && let Err(scan_error) = self
                .current_sensitive_values()
                .and_then(|sensitive_values| {
                    crate::retained_secret_scan::scan_retained_run_and_prepared_bytes(
                        &self.directory,
                        sensitive_values.iter(),
                        &[],
                    )
                })
        {
            let primary = match outcome {
                Err(primary) => {
                    anyhow::anyhow!(
                        "{primary}\nadditionally, the final retained-run scan failed: {scan_error}"
                    )
                }
                Ok(_) => scan_error,
            };
            return match self.remove_sensitive_retained_tree() {
                Ok(()) => Err(primary),
                Err(cleanup) => Err(sensitive_cleanup_error(
                    "final command cleanup",
                    &primary,
                    cleanup,
                )),
            };
        }

        if published && !abandoned {
            return outcome;
        }
        if published {
            let primary = outcome.map_or_else(
                |error| error,
                |_| {
                    anyhow::anyhow!(
                        "a run result existed after retained event or output publication was abandoned\n  fix: remove the affected run directory and retry with writable output"
                    )
                },
            );
            return match self.remove_sensitive_retained_tree() {
                Ok(()) => Err(primary),
                Err(cleanup) => Err(sensitive_cleanup_error(
                    "abandoned result cleanup",
                    &primary,
                    cleanup,
                )),
            };
        }
        if abandoned {
            let primary = outcome.map_or_else(
                |error| error,
                |_| {
                    anyhow::anyhow!(
                        "run result publication was abandoned after retained event or output failure\n  fix: retry with writable stdout and HOME"
                    )
                },
            );
            return match self.mark_aborted() {
                Ok(()) => Err(primary),
                Err(abort) => Err(anyhow::anyhow!(
                    "{primary}\nadditionally, could not mark the unpublished run aborted: {abort}"
                )),
            };
        }

        let primary = match outcome {
            Err(error) => error,
            Ok(_) => {
                anyhow::anyhow!(
                    "the run returned successfully without publishing its terminal result\n  fix: preserve the run directory and file a Greenlit defect"
                )
            }
        };
        match publish_fallback() {
            Ok(())
                if self.terminal_result_written.get()
                    && !self.result_publication_abandoned.load(Ordering::Acquire) =>
            {
                Err(primary)
            }
            Ok(()) => {
                self.abandon_result_publication();
                let publication = anyhow::anyhow!(
                    "fallback terminal publication returned without a durable result marker"
                );
                let combined = anyhow::anyhow!(
                    "{primary}\nadditionally, could not publish the fallback preparation-failed result: {publication}"
                );
                match self.remove_sensitive_retained_tree() {
                    Ok(()) => Err(combined),
                    Err(cleanup) => Err(sensitive_cleanup_error(
                        "failed fallback publication",
                        &combined,
                        cleanup,
                    )),
                }
            }
            Err(publication) => {
                self.abandon_result_publication();
                let combined = anyhow::anyhow!(
                    "{primary}\nadditionally, could not publish the fallback preparation-failed result: {publication}"
                );
                match self.remove_sensitive_retained_tree() {
                    Ok(()) => Err(combined),
                    Err(cleanup) => Err(sensitive_cleanup_error(
                        "failed fallback publication",
                        &combined,
                        cleanup,
                    )),
                }
            }
        }
    }

    fn remove_sensitive_retained_tree(&self) -> anyhow::Result<()> {
        self.abandon_result_publication();
        if let Err(catalog) = self.mark_aborted() {
            return match remove_private_run_tree(
                &self.runs_handle,
                &self.directory_handle,
                &self.directory,
            ) {
                Ok(()) => Err(catalog),
                Err(cleanup) => Err(anyhow::anyhow!(
                    "{catalog}\nadditionally, could not remove the non-authoritative run tree: {cleanup}"
                )),
            };
        }
        remove_private_run_tree(&self.runs_handle, &self.directory_handle, &self.directory)
    }

    fn mark_aborted(&self) -> anyhow::Result<()> {
        self.content_store
            .record_run_state(&self.run_id, None, "aborted")
            .map_err(lifecycle_error)
    }

    fn append_trace(
        &self,
        event: &str,
        attributes: BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        let prepared = self.prepare_trace(event, attributes)?;
        self.append_prepared_trace(prepared)
    }

    fn prepare_trace(
        &self,
        event: &str,
        attributes: BTreeMap<String, String>,
    ) -> anyhow::Result<PreparedTrace> {
        let sequence = self.next_trace_sequence.get();
        let trace = TraceEventV1::new(sequence, event, attributes);
        let path = self.directory.join("trace.ndjson");
        let bytes = serialize_json_line(&path, &trace)?;
        Ok(PreparedTrace { sequence, bytes })
    }

    fn append_prepared_trace(&self, prepared: PreparedTrace) -> anyhow::Result<()> {
        let sequence = self.next_trace_sequence.get();
        if sequence != prepared.sequence {
            anyhow::bail!(
                "could not append the prepared run trace because its sequence changed\n  fix: preserve the run directory and file a Greenlit defect"
            );
        }
        scan_prepared_bytes(
            &self.current_sensitive_values()?,
            &[prepared.bytes.as_slice()],
        )?;
        let path = self.directory.join("trace.ndjson");
        let mut file = if sequence == 1 {
            create_private_file_at(
                &self.directory_handle,
                &path,
                OsStr::new("trace.ndjson"),
                true,
            )?
        } else {
            open_private_file_at(
                &self.directory_handle,
                &path,
                OsStr::new("trace.ndjson"),
                true,
            )?
        };
        file.write_all(&prepared.bytes)
            .map_err(|error| evidence_write_error(&path, error))?;
        file.sync_all()
            .map_err(|error| evidence_write_error(&path, error))?;
        self.next_trace_sequence.set(sequence.saturating_add(1));
        Ok(())
    }

    fn write_job_locks(&self, plan: &ExecutionPlan, run_lock: &RunLockV1) -> anyhow::Result<()> {
        let parent_digest = run_lock.digest().map_err(|error| {
            anyhow::anyhow!(
                "could not identify the finalized run lock: {error}\n  fix: preserve the run directory and retry"
            )
        })?;
        let directory = self.directory.join("job-locks");
        let directory_handle =
            create_new_private_directory(&self.directory_handle, &self.directory, "job-locks")?;
        for job in &plan.jobs {
            let matrix_legs = job.strategy.legs();
            if matrix_legs.is_empty() {
                let key = job.id.0.clone();
                let lock = JobLockV1 {
                    schema_version: 1,
                    run_lock_digest: parent_digest.clone(),
                    job_id: key.clone(),
                    matrix: BTreeMap::new(),
                    needs_evidence: BTreeMap::new(),
                    environment_fingerprint: runner_fingerprint(run_lock, &key),
                };
                write_json_atomic(
                    &directory_handle,
                    &directory,
                    &format!("{key}.json"),
                    &lock,
                    &self.current_sensitive_values()?,
                )?;
                continue;
            }
            for leg in matrix_legs {
                if !matrix_leg_selected(&leg.values, job.matrix_filter.as_ref()) {
                    continue;
                }
                let key = format!("{}[{}]", job.id.0, leg.index);
                let matrix = leg
                    .values
                    .iter()
                    .map(|(name, value)| {
                        serde_json::to_value(value)
                            .map(|json| (name.as_str().to_string(), json))
                            .map_err(|error| {
                                anyhow::anyhow!(
                                    "could not serialize matrix evidence for {key}: {error}\n  fix: preserve the run directory and retry"
                                )
                            })
                    })
                    .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
                let lock = JobLockV1 {
                    schema_version: 1,
                    run_lock_digest: parent_digest.clone(),
                    job_id: job.id.0.clone(),
                    matrix,
                    needs_evidence: BTreeMap::new(),
                    environment_fingerprint: runner_fingerprint(run_lock, &key),
                };
                write_json_atomic(
                    &directory_handle,
                    &directory,
                    &format!("{}-{}.json", job.id.0, leg.index),
                    &lock,
                    &self.current_sensitive_values()?,
                )?;
            }
        }
        Ok(())
    }
}

impl Drop for RunEvidence {
    fn drop(&mut self) {
        if self.command_finalized.get() {
            return;
        }
        let _ = self.finalize_command::<()>(
            Err(anyhow::anyhow!(
                "run evidence left scope before command finalization"
            )),
            || {
                Err(anyhow::anyhow!(
                    "terminal recorder is unavailable during Drop backstop"
                ))
            },
        );
    }
}

fn capture_and_adopt_source(
    context: SourceAdoptionContext<'_>,
    sensitive_values: &[Vec<u8>],
) -> anyhow::Result<SourceSnapshot> {
    let SourceAdoptionContext {
        repo_root,
        home,
        home_handle,
        run_id,
        runs_handle,
        directory_handle,
        directory,
    } = context;
    let stage_name = format!(".greenlit-source-stage-{run_id}");
    let stage_path = home.join(&stage_name);
    let captured =
        SourceSnapshot::capture_with_sensitive_values(repo_root, &stage_path, sensitive_values);
    let mut source = match captured {
        Ok(source) => source,
        Err(error @ SourceSnapshotError::SensitiveValue)
        | Err(error @ SourceSnapshotError::SensitiveValueLimit { .. }) => {
            remove_private_run_tree(runs_handle, directory_handle, directory)?;
            return Err(source_capture_error(error));
        }
        Err(error) => return Err(source_capture_error(error)),
    };
    let stage_handle = match open_private_directory_at(home_handle, home, &stage_name) {
        Ok(handle) => handle,
        Err(error) => {
            let cleanup = fs::remove_dir_all(&stage_path);
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(anyhow::anyhow!(
                    "{error}\nadditionally, could not remove private source staging at {}: {cleanup}",
                    stage_path.display()
                )),
            };
        }
    };
    #[cfg(litci_test_boundaries)]
    while std::env::var_os("LITCI_TEST_SOURCE_STAGE_HOLD").as_deref()
        == Some(OsStr::new("after-capture"))
    {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let manifest_bytes =
        serialize_json_line(&stage_path.join("source-manifest.json"), &source.entries)?;
    if let Err(error) = crate::retained_secret_scan::scan_retained_run_and_prepared_bytes(
        &stage_path,
        sensitive_values.iter(),
        &[manifest_bytes.as_slice()],
    ) {
        let stage_cleanup = remove_private_run_tree(home_handle, &stage_handle, &stage_path);
        let run_cleanup = remove_private_run_tree(runs_handle, directory_handle, directory);
        return match (stage_cleanup, run_cleanup) {
            (Ok(()), Ok(())) => Err(error),
            (stage, run) => Err(anyhow::anyhow!(
                "{error}\nadditionally, private source rejection cleanup failed: stage={stage:?}; run={run:?}"
            )),
        };
    }
    let target = OsStr::new("source");
    let adoption = renameat_with(
        home_handle,
        stage_name.as_str(),
        directory_handle,
        target,
        RenameFlags::NOREPLACE,
    )
    .map_err(|error| evidence_write_error(&directory.join(target), error))
    .and_then(|()| {
        directory_handle
            .sync_all()
            .map_err(|error| evidence_write_error(directory, error))
    })
    .and_then(|()| {
        home_handle
            .sync_all()
            .map_err(|error| evidence_write_error(home, error))
    });
    if let Err(error) = adoption {
        if stage_path.exists()
            && let Err(cleanup) = remove_private_run_tree(home_handle, &stage_handle, &stage_path)
        {
            return Err(anyhow::anyhow!(
                "{error}\nadditionally, could not remove private source staging: {cleanup}"
            ));
        }
        return Err(error);
    }
    source.root = directory.join(target);
    Ok(source)
}

fn source_capture_error(error: SourceSnapshotError) -> anyhow::Error {
    match error {
        error @ SourceSnapshotError::UnsafeRemote => anyhow::anyhow!(
            "{error}\n  fix: remove the credential, or replace or remove remote.origin.url, then retry"
        ),
        error @ (SourceSnapshotError::SensitiveValue
        | SourceSnapshotError::SensitiveValueLimit { .. }) => anyhow::anyhow!(
            "{error}\n  fix: omit explicit secrets until Phase 16 certifies secret preflight, then retry"
        ),
        error => anyhow::anyhow!(
            "{error}\n  fix: stop concurrent source edits and ensure the repository is readable, then retry"
        ),
    }
}

fn matrix_leg_selected(
    values: &IndexMap<MatrixKey, MatrixValue>,
    filter: Option<&IndexMap<String, MatrixValue>>,
) -> bool {
    filter.is_none_or(|filter| {
        filter.iter().all(|(name, expected)| {
            values
                .iter()
                .find(|(key, _)| key.as_str() == name)
                .is_some_and(|(_, actual)| actual == expected)
        })
    })
}

fn lifecycle_error(error: greenlit_store::cas::CasError) -> anyhow::Error {
    anyhow::anyhow!(
        "could not persist active-run storage state: {error}\n  fix: run `litci doctor`, repair the reported metadata issue, then retry"
    )
}

fn snapshot_bytes(masker: &greenlit_engine::execution::Masker) -> anyhow::Result<Vec<Vec<u8>>> {
    let snapshot = masker
        .healthy_snapshot()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(snapshot
        .values()
        .iter()
        .map(|value| value.as_bytes().to_vec())
        .collect())
}

fn recovery_error(error: greenlit_store::cas::CasError) -> anyhow::Error {
    anyhow::anyhow!(
        "could not recover incomplete retained runs before starting a new invocation: {error}\n  fix: preserve ~/.litci, run `litci doctor`, repair the reported metadata issue, then retry"
    )
}

fn runner_fingerprint(run_lock: &RunLockV1, key: &str) -> String {
    run_lock.runners.get(key).map_or_else(
        || opaque_revision(b"runner:deferred"),
        |runner| {
            opaque_revision(
                format!(
                    "runner:{}:{}:{}:{};runtime:{:?}",
                    runner.provider,
                    runner.image_digest,
                    runner.os,
                    runner.architecture,
                    run_lock.runtime
                )
                .as_bytes(),
            )
        },
    )
}

fn local_support_report() -> SupportReport {
    let mut report = SupportReport {
        findings: vec![
            FeatureFinding {
                code: "runtime.host_kernel".to_string(),
                disposition: FindingDisposition::Degraded,
                scope: "run".to_string(),
                reason: "workflow containers share the local host kernel".to_string(),
            },
            FeatureFinding {
                code: "network.external_uncaptured".to_string(),
                disposition: FindingDisposition::Degraded,
                scope: "run".to_string(),
                reason: "external network responses are not captured".to_string(),
            },
            FeatureFinding {
                code: "runner.profile_self_hosted".to_string(),
                disposition: FindingDisposition::Degraded,
                scope: "run".to_string(),
                reason: "the locked runner profile is GitHub's official self-hosted ARC image, not the complete GitHub-hosted runner image".to_string(),
            },
            FeatureFinding {
                code: "runner.user_root".to_string(),
                disposition: FindingDisposition::Degraded,
                scope: "run".to_string(),
                reason: "Greenlit runner-profile steps execute as root while GitHub-hosted Ubuntu steps execute as the runner user".to_string(),
            },
        ],
    };
    report.canonicalize();
    report
}

fn write_json_atomic(
    parent: &File,
    parent_path: &Path,
    name: &str,
    value: &impl serde::Serialize,
    sensitive_values: &[Vec<u8>],
) -> anyhow::Result<()> {
    let path = parent_path.join(name);
    let bytes = serialize_json_line(&path, value)?;
    write_bytes_atomic(parent, parent_path, name, &bytes, sensitive_values)
}

fn serialize_json_line(path: &Path, value: &impl serde::Serialize) -> anyhow::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value).map_err(|error| evidence_write_error(path, error))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn scan_prepared_bytes(
    sensitive_values: &[Vec<u8>],
    prepared_bytes: &[&[u8]],
) -> anyhow::Result<()> {
    crate::retained_secret_scan::scan_prepared_bytes_for_sensitive_values(
        sensitive_values.iter(),
        prepared_bytes,
    )
}

fn write_bytes_atomic(
    parent: &File,
    parent_path: &Path,
    name: &str,
    bytes: &[u8],
    sensitive_values: &[Vec<u8>],
) -> anyhow::Result<()> {
    scan_prepared_bytes(sensitive_values, &[bytes])?;
    let path = parent_path.join(name);
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    let temp_name = temp.file_name().ok_or_else(|| {
        anyhow::anyhow!(
            "could not persist run evidence at {}: temporary path has no file name\n  fix: preserve the run directory and file a Greenlit defect",
            temp.display()
        )
    })?;
    let mut file = create_private_file_at(parent, &temp, temp_name, false)?;
    file.write_all(bytes)
        .map_err(|error| evidence_write_error(&temp, error))?;
    file.sync_all()
        .map_err(|error| evidence_write_error(&temp, error))?;
    let target_name = OsStr::new(name);
    reject_unsafe_existing_file(parent, &path, target_name)?;
    renameat_with(
        parent,
        temp_name,
        parent,
        target_name,
        RenameFlags::NOREPLACE,
    )
    .map_err(|error| evidence_write_error(&path, error))?;
    sync_atomic_parent(parent, parent_path, name)?;
    Ok(())
}

fn sync_atomic_parent(parent: &File, parent_path: &Path, _name: &str) -> anyhow::Result<()> {
    #[cfg(litci_test_boundaries)]
    if _name == "result.json"
        && std::env::var_os("LITCI_TEST_RESULT_DIRECTORY_SYNC_FAILURE").as_deref()
            == Some(OsStr::new("after-rename"))
    {
        return Err(evidence_write_error(
            &parent_path.join(_name),
            "injected directory sync failure after result rename",
        ));
    }
    parent
        .sync_all()
        .map_err(|error| evidence_write_error(parent_path, error))
}

fn hold_result_publication(_point: &str) {
    #[cfg(litci_test_boundaries)]
    while std::env::var_os("LITCI_TEST_RESULT_PUBLICATION_HOLD").as_deref()
        == Some(OsStr::new(_point))
    {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn remove_sensitive_failed_capture(
    runs_handle: &File,
    directory_handle: &File,
    directory: &Path,
    sensitive_values: &[Vec<u8>],
) -> anyhow::Result<()> {
    if sensitive_values.is_empty()
        || crate::retained_secret_scan::scan_retained_run_and_prepared_bytes(
            directory,
            sensitive_values.iter(),
            &[],
        )
        .is_ok()
    {
        return Ok(());
    }
    remove_private_run_tree(runs_handle, directory_handle, directory)
}

fn remove_private_run_tree(
    runs_handle: &File,
    directory_handle: &File,
    directory: &Path,
) -> anyhow::Result<()> {
    let expected = directory_handle
        .metadata()
        .map_err(|error| evidence_write_error(directory, error))?;
    let current = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(evidence_write_error(directory, error)),
    };
    if !current.is_dir()
        || current.uid() != rustix::process::getuid().as_raw()
        || (current.dev(), current.ino()) != (expected.dev(), expected.ino())
    {
        return Err(unsafe_path_error(
            directory,
            "the exact private run directory changed before sensitive cleanup",
        ));
    }
    fs::remove_dir_all(directory).map_err(|error| {
        anyhow::anyhow!(
            "could not remove sensitive run evidence at {}: {error}\n  fix: stop processes accessing the private run directory, remove it, then retry",
            directory.display()
        )
    })?;
    runs_handle
        .sync_all()
        .map_err(|error| evidence_write_error(directory, error))
}

fn sensitive_cleanup_error(
    context: &str,
    primary: &anyhow::Error,
    cleanup: anyhow::Error,
) -> anyhow::Error {
    anyhow::anyhow!(
        "{primary}\nadditionally, {context} could not remove potentially sensitive retained evidence: {cleanup}"
    )
}
