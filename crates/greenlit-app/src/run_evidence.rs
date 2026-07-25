//! Persistent source, lock, and result evidence for one invocation.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use greenlit_engine::{
    ExecutionConclusion, ExecutionPlan, ExecutionResultV1, FeatureFinding, FindingDisposition,
    JobLockV1, LockedSource, MatrixKey, MatrixValue, ResultEvidence, RunLockV1, SourceSnapshot,
    SupportReport, TraceEventV1, opaque_revision,
};
use indexmap::IndexMap;

pub(crate) struct RunEvidence {
    pub(crate) run_id: String,
    pub(crate) directory: PathBuf,
    pub(crate) source: SourceSnapshot,
    next_trace_sequence: Cell<u64>,
    terminal_result_written: Cell<bool>,
    support: RefCell<SupportReport>,
}

pub(crate) struct LockInputs<'a> {
    pub(crate) workflow_path: &'a str,
    pub(crate) event_name: &'a str,
    pub(crate) inputs: &'a [(String, String)],
    pub(crate) selected_job: Option<&'a str>,
    pub(crate) selected_matrix: &'a [(String, String)],
    pub(crate) offline: bool,
    pub(crate) plan: &'a ExecutionPlan,
    pub(crate) secrets: &'a [(String, String)],
    pub(crate) actions: BTreeMap<String, String>,
    pub(crate) containers: BTreeMap<String, String>,
    pub(crate) runners: BTreeMap<String, greenlit_engine::RunnerLockV1>,
    pub(crate) toolchains: BTreeMap<String, String>,
}

impl RunEvidence {
    pub(crate) fn capture(repo_root: &Path) -> anyhow::Result<Self> {
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
        let runs = home.join(".litci").join("runs");
        fs::create_dir_all(&runs).map_err(|error| {
            anyhow::anyhow!(
                "could not create run evidence directory {}: {error}\n  fix: make HOME writable, then retry",
                runs.display()
            )
        })?;
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
            match fs::create_dir(&directory) {
                Ok(()) => {
                    selected = Some((run_id, directory));
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "could not create run evidence directory {}: {error}\n  fix: make HOME writable, then retry",
                        directory.display()
                    ));
                }
            }
        }
        let (run_id, directory) = selected.ok_or_else(|| {
            anyhow::anyhow!(
                "could not allocate a unique run identity\n  fix: retry after the current invocation finishes"
            )
        })?;
        let source_root = directory.join("source");
        let source = SourceSnapshot::capture(repo_root, &source_root).map_err(|error| {
            anyhow::anyhow!("{error}\n  fix: stop concurrent source edits and ensure the repository is readable, then retry")
        })?;
        write_json_atomic(&directory.join("source-manifest.json"), &source.entries)?;
        ingest_source(&home, &source)?;
        let evidence = Self {
            run_id,
            directory,
            source,
            next_trace_sequence: Cell::new(1),
            terminal_result_written: Cell::new(false),
            support: RefCell::new(local_support_report()),
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
    }

    pub(crate) fn merge_support(&self, report: &SupportReport) -> anyhow::Result<()> {
        {
            let mut support = self.support.borrow_mut();
            support.findings.extend(report.findings.iter().cloned());
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

    pub(crate) fn lock(&self, inputs: LockInputs<'_>) -> anyhow::Result<RunLockV1> {
        let LockInputs {
            workflow_path,
            event_name,
            inputs,
            selected_job,
            selected_matrix,
            offline,
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
        lock.runners = runners;
        lock.secret_revisions = secrets
            .iter()
            .map(|(name, value)| (name.clone(), opaque_revision(value.as_bytes())))
            .collect();
        lock.actions = actions;
        lock.containers = containers;
        lock.toolchains = toolchains;
        lock.compatibility = self.support.borrow().clone();
        write_json_atomic(&self.directory.join("run-lock.json"), &lock)?;
        self.write_job_locks(plan, &lock)?;
        self.append_trace(
            "run_lock_finalized",
            BTreeMap::from([
                (
                    "digest".to_string(),
                    lock.digest().map_err(|error| {
                        anyhow::anyhow!(
                            "could not identify the finalized run lock: {error}\n  fix: preserve the run directory and retry"
                        )
                    })?,
                ),
                ("offline".to_string(), lock.offline.to_string()),
            ]),
        )?;
        Ok(lock)
    }

    pub(crate) fn write_result(
        &self,
        conclusion: ExecutionConclusion,
        support: SupportReport,
    ) -> anyhow::Result<ExecutionResultV1> {
        let result = ExecutionResultV1::classify(&ResultEvidence {
            conclusion,
            support,
            clean: false,
            hermetic: false,
            github_confirmed: false,
        });
        write_json_atomic(&self.directory.join("result.json"), &result)?;
        self.append_trace(
            "run_completed",
            BTreeMap::from([
                ("conclusion".to_string(), format!("{:?}", result.conclusion)),
                (
                    "compatibility".to_string(),
                    format!("{:?}", result.compatibility),
                ),
                ("assurance".to_string(), format!("{:?}", result.assurance)),
            ]),
        )?;
        self.terminal_result_written.set(true);
        Ok(result)
    }

    fn append_trace(
        &self,
        event: &str,
        attributes: BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        let sequence = self.next_trace_sequence.get();
        let trace = TraceEventV1::new(sequence, event, attributes);
        let path = self.directory.join("trace.ndjson");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| evidence_write_error(&path, error))?;
        serde_json::to_writer(&mut file, &trace)
            .map_err(|error| evidence_write_error(&path, error))?;
        file.write_all(b"\n")
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
        fs::create_dir(&directory).map_err(|error| evidence_write_error(&directory, error))?;
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
                write_json_atomic(&directory.join(format!("{key}.json")), &lock)?;
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
                    &directory.join(format!("{}-{}.json", job.id.0, leg.index)),
                    &lock,
                )?;
            }
        }
        Ok(())
    }
}

impl Drop for RunEvidence {
    fn drop(&mut self) {
        if self.terminal_result_written.get() {
            return;
        }
        let _ = self.write_result(
            ExecutionConclusion::PreparationFailed,
            self.support.borrow().clone(),
        );
    }
}

fn ingest_source(home: &Path, source: &SourceSnapshot) -> anyhow::Result<()> {
    use greenlit_store::cas::{CasStore, ObjectDigest};

    let store = CasStore::open(CasStore::default_path_under(home)).map_err(|error| {
        anyhow::anyhow!(
            "could not open the verified content store: {error}\n  fix: ensure HOME has free space and is writable, then retry"
        )
    })?;
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

fn runner_fingerprint(run_lock: &RunLockV1, key: &str) -> String {
    run_lock.runners.get(key).map_or_else(
        || opaque_revision(b"runner:deferred"),
        |runner| {
            opaque_revision(
                format!(
                    "runner:{}:{}:{}:{}",
                    runner.provider, runner.image_digest, runner.os, runner.architecture
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

fn write_json_atomic(path: &Path, value: &impl serde::Serialize) -> anyhow::Result<()> {
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|error| evidence_write_error(&temp, error))?;
    serde_json::to_writer(&mut file, value).map_err(|error| evidence_write_error(&temp, error))?;
    file.write_all(b"\n")
        .map_err(|error| evidence_write_error(&temp, error))?;
    file.sync_all()
        .map_err(|error| evidence_write_error(&temp, error))?;
    fs::rename(&temp, path).map_err(|error| evidence_write_error(path, error))?;
    sync_parent(path)?;
    Ok(())
}

fn sync_parent(path: &Path) -> anyhow::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "could not persist run evidence at {}: path has no parent\n  fix: set HOME to an absolute writable directory",
            path.display()
        )
    })?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| evidence_write_error(parent, error))
}

fn evidence_write_error(path: &Path, error: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!(
        "could not persist run evidence at {}: {error}\n  fix: ensure HOME has free space and is writable, then retry",
        path.display()
    )
}
