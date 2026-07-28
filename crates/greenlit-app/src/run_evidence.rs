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
use rustix::fs::{Mode, OFlags, fchmod, mkdirat, open, openat, renameat};
use rustix::io::Errno;

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;

pub(crate) struct RunEvidence {
    pub(crate) run_id: String,
    pub(crate) directory: PathBuf,
    pub(crate) source: SourceSnapshot,
    runs_handle: File,
    directory_handle: File,
    next_trace_sequence: Cell<u64>,
    terminal_result_written: Cell<bool>,
    result_publication_abandoned: Arc<AtomicBool>,
    sensitive_values: RefCell<Vec<Vec<u8>>>,
    support: RefCell<SupportReport>,
    content_store: greenlit_store::cas::CasStore,
    lease: RefCell<Option<greenlit_store::cas::LeaseGuard>>,
}

pub(crate) struct PreparedResult {
    result_bytes: Vec<u8>,
    terminal_conclusion: String,
    terminal_compatibility: String,
    terminal_assurance: String,
}

struct PreparedTrace {
    sequence: u64,
    bytes: Vec<u8>,
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
    pub(crate) fn capture(repo_root: &Path, sensitive_values: &[String]) -> anyhow::Result<Self> {
        let sensitive_values = sensitive_values
            .iter()
            .map(|value| value.as_bytes().to_vec())
            .collect::<Vec<_>>();
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
        let (runs, runs_handle) = prepare_runs_directory(&home)?;
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
        let captured = (|| {
            let source_root = directory.join("source");
            let source =
                SourceSnapshot::capture(repo_root, &source_root).map_err(|error| match error {
                    SourceSnapshotError::UnsafeRemote => anyhow::anyhow!(
                        "{error}\n  fix: remove the credential, or replace or remove remote.origin.url, then retry"
                    ),
                    error => anyhow::anyhow!(
                        "{error}\n  fix: stop concurrent source edits and ensure the repository is readable, then retry"
                    ),
                })?;
            write_json_atomic(
                &directory_handle,
                &directory,
                "source-manifest.json",
                &source.entries,
            )?;
            let content_store = greenlit_store::cas::CasStore::open(
                greenlit_store::cas::CasStore::default_path_under(&home),
            )
            .map_err(|error| {
                anyhow::anyhow!(
                    "could not open the verified content store: {error}\n  fix: ensure HOME has free space and is writable, then retry"
                )
            })?;
            ingest_source(&content_store, &source)?;
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
                result_publication_abandoned: Arc::new(AtomicBool::new(false)),
                sensitive_values: RefCell::new(sensitive_values.clone()),
                support: RefCell::new(local_support_report()),
                content_store,
                lease: RefCell::new(None),
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

    pub(crate) fn abandon_result_publication(&self) {
        self.result_publication_abandoned
            .store(true, Ordering::Release);
        self.terminal_result_written.set(true);
    }

    pub(crate) fn result_publication_gate(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.result_publication_abandoned)
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

    pub(crate) fn register_sensitive_values<I, V>(&self, values: I)
    where
        I: IntoIterator<Item = V>,
        V: AsRef<[u8]>,
    {
        self.sensitive_values
            .borrow_mut()
            .extend(values.into_iter().map(|value| value.as_ref().to_vec()));
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
        )?;
        write_json_atomic(
            &self.directory_handle,
            &self.directory,
            "execution-plan.json",
            plan,
        )?;
        self.write_job_locks(plan, &lock)?;
        let leased = self.source_digests()?;
        self.content_store
            .pin_objects("run-lock", &lock_digest, &leased)
            .and_then(|()| {
                self.content_store
                    .record_run_state(&self.run_id, Some(&lock_digest), "resolved")
            })
            .map_err(lifecycle_error)?;
        let lease = self
            .content_store
            .lease_guard(self.run_id.clone(), &leased)
            .map_err(lifecycle_error)?;
        self.lease.replace(Some(lease));
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
        let scan_result = crate::retained_secret_scan::scan_retained_run_and_prepared_bytes(
            &self.directory,
            self.sensitive_values.borrow().iter(),
            &[
                result_bytes.as_slice(),
                terminal_bytes.as_slice(),
                completed_trace.bytes.as_slice(),
            ],
        );
        if let Err(error) = scan_result {
            let retained_tree_is_clean =
                crate::retained_secret_scan::scan_retained_run_and_prepared_bytes(
                    &self.directory,
                    self.sensitive_values.borrow().iter(),
                    &[],
                )
                .is_ok();
            if !retained_tree_is_clean {
                self.remove_sensitive_retained_tree().map_err(|cleanup| {
                    sensitive_cleanup_error("rejected terminal publication", &error, cleanup)
                })?;
            }
            return Err(error);
        }
        self.append_prepared_trace(completed_trace)?;
        self.content_store
            .record_run_state(&self.run_id, None, "completed")
            .map_err(lifecycle_error)?;
        Ok(PreparedResult {
            result_bytes,
            terminal_conclusion,
            terminal_compatibility,
            terminal_assurance,
        })
    }

    pub(crate) fn publish_prepared_result(&self, prepared: PreparedResult) -> anyhow::Result<()> {
        if self.result_publication_abandoned.load(Ordering::Acquire) {
            anyhow::bail!(
                "could not publish a run result after retained event or output failure\n  fix: preserve the run directory and retry with writable output"
            );
        }
        // `result.json` is the retained publication marker. The journal is
        // already durable in the caller, and this exact byte sequence passed
        // the retained-secret invariant before the terminal was emitted.
        write_bytes_atomic(
            &self.directory_handle,
            &self.directory,
            "result.json",
            &prepared.result_bytes,
        )?;
        self.terminal_result_written.set(true);
        self.lease.replace(None);
        Ok(())
    }

    fn source_digests(&self) -> anyhow::Result<Vec<greenlit_store::cas::ObjectDigest>> {
        std::iter::once(&self.source.digest)
            .chain(self.source.entries.iter().map(|entry| &entry.digest))
            .map(|digest| {
                greenlit_store::cas::ObjectDigest::parse(digest).map_err(|error| {
                    anyhow::anyhow!(
                        "source manifest contains invalid identity {digest}: {error}\n  fix: preserve the run directory and file a Greenlit defect"
                    )
                })
            })
            .collect()
    }

    fn remove_sensitive_retained_tree(&self) -> anyhow::Result<()> {
        remove_private_run_tree(&self.runs_handle, &self.directory_handle, &self.directory)?;
        self.abandon_result_publication();
        self.lease.replace(None);
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
                write_json_atomic(&directory_handle, &directory, &format!("{key}.json"), &lock)?;
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
                )?;
            }
        }
        Ok(())
    }
}

impl Drop for RunEvidence {
    fn drop(&mut self) {
        if self.terminal_result_written.get()
            || self.result_publication_abandoned.load(Ordering::Acquire)
        {
            return;
        }
        let support = self.support.borrow().clone();
        let Ok(prepared) = self.prepare_result(
            ExecutionConclusion::PreparationFailed,
            support,
            false,
            false,
        ) else {
            return;
        };
        let _ = self.publish_prepared_result(prepared);
    }
}

fn ingest_source(
    store: &greenlit_store::cas::CasStore,
    source: &SourceSnapshot,
) -> anyhow::Result<()> {
    use greenlit_store::cas::ObjectDigest;
    for entry in &source.entries {
        let digest = ObjectDigest::parse(&entry.digest).map_err(|error| {
            anyhow::anyhow!(
                "source manifest contains invalid identity {}: {error}\n  fix: preserve the run directory and file a Greenlit defect",
                entry.digest
            )
        })?;
        let path = source.root.join(&entry.path);
        match entry.kind {
            greenlit_engine::SourceEntryKind::File => {
                store
                    .put_file_verified(&digest, &path)
                    .map_err(|error| content_error(&entry.path, error))?;
            }
            greenlit_engine::SourceEntryKind::Symlink => {
                let target = fs::read_link(&path).map_err(|error| {
                    anyhow::anyhow!(
                        "could not ingest source symlink {}: {error}\n  fix: stop concurrent source edits and retry",
                        entry.path
                    )
                })?;
                store
                    .put_verified(&digest, target.as_os_str().as_encoded_bytes())
                    .map_err(|error| content_error(&entry.path, error))?;
            }
        }
    }
    let manifest = serde_json::to_vec(&source.entries).map_err(|error| {
        anyhow::anyhow!(
            "could not serialize the source CAS manifest: {error}\n  fix: preserve the run directory and file a Greenlit defect"
        )
    })?;
    let manifest_digest = ObjectDigest::parse(&source.digest).map_err(|error| {
        anyhow::anyhow!(
            "source tree has invalid identity {}: {error}\n  fix: preserve the run directory and file a Greenlit defect",
            source.digest
        )
    })?;
    store
        .put_verified(&manifest_digest, &manifest)
        .map_err(|error| content_error("source-manifest.json", error))?;
    Ok(())
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

fn content_error(path: &str, error: greenlit_store::cas::CasError) -> anyhow::Error {
    anyhow::anyhow!(
        "could not publish verified source content {path}: {error}\n  fix: run `litci doctor`; corrupted content will be quarantined automatically"
    )
}

fn lifecycle_error(error: greenlit_store::cas::CasError) -> anyhow::Error {
    anyhow::anyhow!(
        "could not persist active-run storage state: {error}\n  fix: run `litci doctor`, repair the reported metadata issue, then retry"
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
) -> anyhow::Result<()> {
    let path = parent_path.join(name);
    let bytes = serialize_json_line(&path, value)?;
    write_bytes_atomic(parent, parent_path, name, &bytes)
}

fn serialize_json_line(path: &Path, value: &impl serde::Serialize) -> anyhow::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value).map_err(|error| evidence_write_error(path, error))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn write_bytes_atomic(
    parent: &File,
    parent_path: &Path,
    name: &str,
    bytes: &[u8],
) -> anyhow::Result<()> {
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
    renameat(parent, temp_name, parent, target_name)
        .map_err(|error| evidence_write_error(&path, error))?;
    parent
        .sync_all()
        .map_err(|error| evidence_write_error(parent_path, error))?;
    Ok(())
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

fn prepare_runs_directory(home: &Path) -> anyhow::Result<(PathBuf, File)> {
    let home_handle = open(
        home,
        OFlags::RDONLY
            | OFlags::DIRECTORY
            | OFlags::CLOEXEC
            | OFlags::NOFOLLOW
            | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| {
        anyhow::anyhow!(
            "could not open HOME for run evidence at {}: {error}\n  fix: set HOME to an absolute writable directory owned by the current user, then retry",
            home.display()
        )
    })?;
    validate_current_owner(home, &home_handle.metadata().map_err(|error| {
        anyhow::anyhow!(
            "could not inspect HOME for run evidence at {}: {error}\n  fix: set HOME to an absolute writable directory owned by the current user, then retry",
            home.display()
        )
    })?)?;
    let litci = create_or_open_private_directory(&home_handle, home, ".litci")?;
    let litci_path = home.join(".litci");
    let runs = create_or_open_private_directory(&litci, &litci_path, "runs")?;
    Ok((litci_path.join("runs"), runs))
}

fn create_or_open_private_directory(
    parent: &File,
    parent_path: &Path,
    name: &str,
) -> anyhow::Result<File> {
    match mkdirat(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
        Ok(()) => open_new_private_directory_at(parent, parent_path, name),
        Err(Errno::EXIST) => open_private_directory_at(parent, parent_path, name),
        Err(error) => Err(evidence_write_error(&parent_path.join(name), error)),
    }
}

fn create_new_private_directory(
    parent: &File,
    parent_path: &Path,
    name: &str,
) -> anyhow::Result<File> {
    let path = parent_path.join(name);
    match mkdirat(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
        Ok(()) => open_new_private_directory_at(parent, parent_path, name),
        Err(Errno::EXIST) => {
            let existing = open_private_directory_at(parent, parent_path, name)?;
            drop(existing);
            Err(evidence_write_error(
                &path,
                "a directory already exists at this write-once evidence path",
            ))
        }
        Err(error) => Err(evidence_write_error(&path, error)),
    }
}

fn open_new_private_directory_at(
    parent: &File,
    parent_path: &Path,
    name: &str,
) -> anyhow::Result<File> {
    let path = parent_path.join(name);
    let file = open_directory_at(parent, &path, name)?;
    let metadata = file
        .metadata()
        .map_err(|error| evidence_write_error(&path, error))?;
    if !metadata.is_dir() {
        return Err(unsafe_path_error(&path, "path is not a directory"));
    }
    validate_current_owner(&path, &metadata)?;
    let mode = metadata.mode() & 0o7777;
    if mode & !0o2700 != 0 {
        return Err(unsafe_path_error(
            &path,
            format!("new private directory has unexpected mode 0{mode:03o}"),
        ));
    }
    // Linux inherits SGID from a parent directory even when mkdirat requests
    // 0700. Clear that inherited bit on the descriptor for this newly created
    // inode before any retained child is written. Existing paths take the
    // strict open path below and are never repaired.
    fchmod(&file, Mode::RUSR | Mode::WUSR | Mode::XUSR)
        .map_err(|error| evidence_write_error(&path, error))?;
    validate_private_directory(&path, &file)?;
    Ok(file)
}

fn open_private_directory_at(
    parent: &File,
    parent_path: &Path,
    name: &str,
) -> anyhow::Result<File> {
    let path = parent_path.join(name);
    let file = open_directory_at(parent, &path, name)?;
    validate_private_directory(&path, &file)?;
    Ok(file)
}

fn open_directory_at(parent: &File, path: &Path, name: &str) -> anyhow::Result<File> {
    openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| unsafe_path_error(path, error))
}

fn create_private_file_at(
    parent: &File,
    path: &Path,
    name: &OsStr,
    append: bool,
) -> anyhow::Result<File> {
    let mut flags =
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    if append {
        flags |= OFlags::APPEND;
    }
    let file = match openat(parent, name, flags, Mode::RUSR | Mode::WUSR) {
        Ok(fd) => File::from(fd),
        Err(Errno::EXIST) => {
            reject_unsafe_existing_file(parent, path, name)?;
            return Err(evidence_write_error(
                path,
                "a file already exists at this write-once evidence path",
            ));
        }
        Err(error) => return Err(evidence_write_error(path, error)),
    };
    validate_private_file(path, &file)?;
    Ok(file)
}

fn open_private_file_at(
    parent: &File,
    path: &Path,
    name: &OsStr,
    append: bool,
) -> anyhow::Result<File> {
    let mut flags = OFlags::WRONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    if append {
        flags |= OFlags::APPEND;
    }
    let file = openat(parent, name, flags, Mode::empty())
        .map(File::from)
        .map_err(|error| unsafe_path_error(path, error))?;
    validate_private_file(path, &file)?;
    Ok(file)
}

fn reject_unsafe_existing_file(parent: &File, path: &Path, name: &OsStr) -> anyhow::Result<()> {
    match openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => validate_private_file(path, &File::from(fd)),
        Err(Errno::NOENT) => Ok(()),
        Err(error) => Err(unsafe_path_error(path, error)),
    }
}

fn validate_private_directory(path: &Path, directory: &File) -> anyhow::Result<()> {
    let metadata = directory
        .metadata()
        .map_err(|error| evidence_write_error(path, error))?;
    if !metadata.is_dir() {
        return Err(unsafe_path_error(path, "path is not a directory"));
    }
    validate_private_metadata(path, &metadata, PRIVATE_DIRECTORY_MODE)
}

fn validate_private_file(path: &Path, file: &File) -> anyhow::Result<()> {
    let metadata = file
        .metadata()
        .map_err(|error| evidence_write_error(path, error))?;
    if !metadata.is_file() {
        return Err(unsafe_path_error(path, "path is not a regular file"));
    }
    validate_private_metadata(path, &metadata, PRIVATE_FILE_MODE)
}

fn validate_private_metadata(
    path: &Path,
    metadata: &fs::Metadata,
    required_mode: u32,
) -> anyhow::Result<()> {
    validate_current_owner(path, metadata)?;
    let mode = metadata.mode() & 0o7777;
    if mode != required_mode {
        anyhow::bail!(
            "refused unsafe run evidence path {} because its mode is 0{mode:03o}\n  fix: change its mode to 0{required_mode:03o} and ensure it is owned by the current user, then retry",
            path.display()
        );
    }
    Ok(())
}

fn validate_current_owner(path: &Path, metadata: &fs::Metadata) -> anyhow::Result<()> {
    let current_uid = rustix::process::getuid().as_raw();
    if metadata.uid() == current_uid {
        return Ok(());
    }
    anyhow::bail!(
        "refused unsafe run evidence path {} because it is owned by uid {}, not the current uid {current_uid}\n  fix: move the path aside or make it private and owned by the current user, then retry",
        path.display(),
        metadata.uid()
    )
}

fn unsafe_path_error(path: &Path, error: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!(
        "refused unsafe run evidence path {}: {error}\n  fix: move the path aside or make it private and owned by the current user, then retry",
        path.display()
    )
}

fn evidence_write_error(path: &Path, error: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!(
        "could not persist run evidence at {}: {error}\n  fix: ensure HOME has free space and is writable, then retry",
        path.display()
    )
}
