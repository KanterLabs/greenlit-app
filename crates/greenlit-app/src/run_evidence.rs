//! Persistent source, lock, and result evidence for one invocation.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use greenlit_engine::planned::Evaluation;
use greenlit_engine::{
    ExecutionConclusion, ExecutionPlan, ExecutionResultV1, FeatureFinding, FindingDisposition,
    JobLockV1, LockedSource, ResultEvidence, RunLockV1, SourceSnapshot, SupportReport,
    opaque_revision,
};

pub(crate) struct RunEvidence {
    pub(crate) run_id: String,
    pub(crate) directory: PathBuf,
    pub(crate) source: SourceSnapshot,
}

pub(crate) struct LockInputs<'a> {
    pub(crate) workflow_path: &'a str,
    pub(crate) event_name: &'a str,
    pub(crate) inputs: &'a [(String, String)],
    pub(crate) selected_job: Option<&'a str>,
    pub(crate) plan: &'a ExecutionPlan,
    pub(crate) secrets: &'a [(String, String)],
    pub(crate) actions: BTreeMap<String, String>,
    pub(crate) containers: BTreeMap<String, String>,
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
        Ok(Self {
            run_id,
            directory,
            source,
        })
    }

    pub(crate) fn lock(&self, inputs: LockInputs<'_>) -> anyhow::Result<RunLockV1> {
        let LockInputs {
            workflow_path,
            event_name,
            inputs,
            selected_job,
            plan,
            secrets,
            actions,
            containers,
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
        lock.runners = runner_identities(plan);
        lock.secret_revisions = secrets
            .iter()
            .map(|(name, value)| (name.clone(), opaque_revision(value.as_bytes())))
            .collect();
        lock.actions = actions;
        lock.containers = containers;
        lock.compatibility = local_support_report();
        write_json_atomic(&self.directory.join("run-lock.json"), &lock)?;
        self.write_job_locks(plan, &lock)?;
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
        Ok(result)
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

fn runner_fingerprint(run_lock: &RunLockV1, key: &str) -> String {
    run_lock.runners.get(key).map_or_else(
        || opaque_revision(b"runner:deferred"),
        |runner| opaque_revision(format!("runner:{runner}:linux:amd64").as_bytes()),
    )
}

fn runner_identities(plan: &ExecutionPlan) -> BTreeMap<String, String> {
    let mut runners = BTreeMap::new();
    for job in &plan.jobs {
        if let Some(runner) = &job.runner
            && let Evaluation::Static(image) = &runner.evaluation
        {
            runners.insert(job.id.0.clone(), image.image_identifier().to_string());
        }
        for (index, leg) in job.legs.iter().enumerate() {
            if let Evaluation::Static(image) = &leg.runner.evaluation {
                runners.insert(
                    format!("{}[{index}]", job.id.0),
                    image.image_identifier().to_string(),
                );
            }
        }
    }
    runners
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
