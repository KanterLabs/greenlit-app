//! Separate workflow export and read-only GitHub confirmation import.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use greenlit_engine::{
    Assurance, ExecutionResultV1, GithubEvidenceV1, GithubJobEvidenceV1, GithubStepEvidenceV1,
    ResultEvidence, RunLockV1,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::cli::{ConfirmArgs, ExportArgs};

const ARTIFACT_NAME: &str = "greenlit-evidence-v1";
const ARTIFACT_FILE: &str = "greenlit-evidence-v1.json";
const EXPORTED_WORKFLOW: &str = "greenlit-confirmation.yml";
const EXPORT_MARKER: &str = "# greenlit-confirmation-evidence-v1";
const SOURCE_COMMIT_PLACEHOLDER: &str = "__GREENLIT_GITHUB_SHA__";
const UPLOAD_ARTIFACT_COMMIT: &str = "ea165f8d65b6e75b540449e92b4886f43607fa02";
const MAX_API_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ARTIFACT_BYTES: usize = 32 * 1024 * 1024;

pub(crate) fn export(args: ExportArgs) -> anyhow::Result<()> {
    let runs = runs_root()?;
    let run_id = match args.run_id {
        Some(run_id) => validate_run_id(&run_id)?,
        None => latest_run_id(&runs)?,
    };
    let run_dir = completed_run_dir(&runs, &run_id)?;
    let lock: RunLockV1 = read_json(&run_dir.join("run-lock.json"))?;
    if lock.source.dirty {
        anyhow::bail!(
            "run {run_id} used uncommitted content and cannot be exported for GitHub confirmation\n  fix: commit the source, run it again, then export that clean run"
        );
    }
    let plan: serde_json::Value = read_json(&run_dir.join("execution-plan.json")).map_err(|_| {
        anyhow::anyhow!(
            "run {run_id} predates GitHub confirmation planning evidence\n  fix: run the workflow once with this litci build, then export the new run"
        )
    })?;
    let source_path = safe_source_path(&lock.source.workflow_path)?;
    let original = fs::read_to_string(run_dir.join("source").join(&source_path)).map_err(|error| {
        anyhow::anyhow!(
            "could not read the frozen workflow for run {run_id}: {error}\n  fix: preserve the run directory and use `litci doctor` to diagnose it"
        )
    })?;
    let jobs = evidence_jobs(&plan)?;
    if jobs.iter().any(|job| job.id == "greenlit_confirmation") {
        anyhow::bail!(
            "the workflow already has a job named greenlit_confirmation\n  fix: rename that job, rerun locally, then export again"
        );
    }
    let named = name_unnamed_steps(&original, &lock.source.workflow_path)?;
    let pinned = pin_workflow(&named, &lock)?;
    let semantic_digest = sha256_identity(pinned.as_bytes());
    let exported_path = format!(".github/workflows/{EXPORTED_WORKFLOW}");
    let evidence = GithubEvidenceV1 {
        schema_version: 1,
        source_commit: lock.source.commit.clone(),
        workflow_digest: lock.source.workflow_digest.clone(),
        exported_workflow_digest: semantic_digest,
        exported_workflow_path: exported_path,
        event: lock.event.clone(),
        inputs: lock.inputs.clone(),
        actions: lock.actions.clone(),
        containers: lock.containers.clone(),
        toolchains: lock.toolchains.clone(),
        jobs,
    };
    evidence.matches_lock(&lock).map_err(|reason| {
        anyhow::anyhow!(
            "could not export matching evidence: {reason}\n  fix: rerun the exact clean source and retry"
        )
    })?;
    let evidence_bytes = evidence.canonical_json().map_err(|error| {
        anyhow::anyhow!(
            "could not serialize GitHub evidence: {error}\n  fix: preserve the run directory and file a Greenlit defect"
        )
    })?;
    let workflow = append_evidence_job(&pinned, &evidence, &evidence_bytes)?;
    let output = args
        .output
        .unwrap_or_else(|| PathBuf::from(".litci").join("confirmation").join(&run_id));
    create_clean_export_dir(&output)?;
    write_atomic(&output.join(EXPORTED_WORKFLOW), workflow.as_bytes())?;
    write_atomic(&output.join(ARTIFACT_FILE), &evidence_bytes)?;
    let durable = run_dir.join("github-export");
    create_clean_export_dir(&durable)?;
    write_atomic(&durable.join(EXPORTED_WORKFLOW), workflow.as_bytes())?;
    write_atomic(&durable.join(ARTIFACT_FILE), &evidence_bytes)?;
    write_atomic(
        &durable.join("export-manifest.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "workflow_file": EXPORTED_WORKFLOW,
            "workflow_semantics_digest": evidence.exported_workflow_digest,
            "evidence_digest": evidence.digest().map_err(|error| anyhow::anyhow!(error))?,
            "upload_artifact_commit": UPLOAD_ARTIFACT_COMMIT,
        }))?
        .as_slice(),
    )?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(
        out,
        "Exported run {run_id} to {}\n  workflow: {}\n  evidence: {}",
        output.display(),
        output.join(EXPORTED_WORKFLOW).display(),
        output.join(ARTIFACT_FILE).display()
    )?;
    Ok(())
}

pub(crate) fn confirm(args: ConfirmArgs) -> anyhow::Result<ExitCode> {
    let run_id = validate_run_id(&args.run_id)?;
    let run_dir = completed_run_dir(&runs_root()?, &run_id)?;
    let lock: RunLockV1 = read_json(&run_dir.join("run-lock.json"))?;
    let local_result: ExecutionResultV1 = read_json(&run_dir.join("result.json"))?;
    let expected: GithubEvidenceV1 =
        read_json(&run_dir.join("github-export").join(ARTIFACT_FILE)).map_err(|_| {
            anyhow::anyhow!(
                "run {run_id} has no durable GitHub export evidence\n  fix: run `litci export {run_id}`, commit that separate workflow, and retry after GitHub completes it"
            )
        })?;
    expected.matches_lock(&lock).map_err(|reason| {
        anyhow::anyhow!(
            "local export no longer matches run {run_id}: {reason}\n  fix: export the run again and use only that workflow"
        )
    })?;
    let (owner, repo) = parse_repository(&args.repository)?;
    let token = crate::auth::current_token().map_err(anyhow::Error::msg)?;
    let client = GithubClient::new(token)?;
    let base = format!("/repos/{owner}/{repo}");
    let remote_run: RemoteRun =
        client.get_json(&format!("{base}/actions/runs/{}", args.github_run))?;
    if remote_run.conclusion.as_deref() != Some("success") {
        anyhow::bail!(
            "GitHub run {} did not pass (conclusion: {})\n  fix: select a completed successful run of the exported workflow",
            args.github_run,
            remote_run.conclusion.as_deref().unwrap_or("not completed")
        );
    }
    if remote_run.head_sha != expected.source_commit {
        anyhow::bail!(
            "GitHub passed a different source commit ({})\n  fix: run the exported workflow at commit {}",
            remote_run.head_sha,
            expected.source_commit
        );
    }
    if remote_run.event != expected.event {
        anyhow::bail!(
            "GitHub passed event '{}' but the local lock used '{}'\n  fix: trigger the exported workflow with the matching event",
            remote_run.event,
            expected.event
        );
    }
    let remote_path = remote_run
        .path
        .split('@')
        .next()
        .unwrap_or(&remote_run.path);
    if remote_path != expected.exported_workflow_path {
        anyhow::bail!(
            "GitHub run used workflow {remote_path}, not {}\n  fix: select the run created from the exported workflow",
            expected.exported_workflow_path
        );
    }
    let workflow = client.get_bytes(
        &format!(
            "{base}/contents/{}?ref={}",
            expected.exported_workflow_path, expected.source_commit
        ),
        "application/vnd.github.raw+json",
        MAX_API_BYTES,
    )?;
    let semantic = exported_semantic_prefix(&workflow)?;
    if sha256_identity(semantic) != expected.exported_workflow_digest {
        anyhow::bail!(
            "the GitHub workflow bytes do not match the exported pinned workflow\n  fix: commit the exact file produced by `litci export {run_id}`"
        );
    }
    let jobs: RemoteJobs = client.get_json(&format!(
        "{base}/actions/runs/{}/jobs?per_page=100",
        args.github_run
    ))?;
    verify_jobs(&expected.jobs, &jobs)?;
    let artifacts: RemoteArtifacts = client.get_json(&format!(
        "{base}/actions/runs/{}/artifacts?per_page=100",
        args.github_run
    ))?;
    let artifact = unique_evidence_artifact(&artifacts)?;
    let archive = client.get_bytes(
        &format!("{base}/actions/artifacts/{}/zip", artifact.id),
        "application/octet-stream",
        MAX_ARTIFACT_BYTES as u64,
    )?;
    if sha256_identity(&archive) != artifact.digest {
        anyhow::bail!(
            "GitHub evidence artifact bytes do not match the API digest\n  fix: preserve the run and retry after downloading the intact artifact"
        );
    }
    let imported = evidence_from_zip(&archive)?;
    if imported != expected {
        anyhow::bail!(
            "GitHub evidence artifact does not match the local exported evidence\n  fix: use the exact workflow produced by `litci export {run_id}`"
        );
    }
    let upgraded = ExecutionResultV1::classify(&ResultEvidence {
        conclusion: local_result.conclusion,
        support: lock.compatibility.clone(),
        clean: lock.clean,
        hermetic: lock.hermetic,
        github_confirmed: true,
    });
    if upgraded.assurance != Assurance::GithubConfirmed {
        println!(
            "GitHub pass observed for matching source and workflow, but local run {run_id} is not eligible for confirmation ({:?}/{:?}/{:?}).",
            upgraded.conclusion, upgraded.compatibility, upgraded.assurance
        );
        return Ok(ExitCode::SUCCESS);
    }
    write_atomic(
        &run_dir.join("result.json"),
        serde_json::to_vec(&upgraded)?.as_slice(),
    )?;
    write_atomic(
        &run_dir.join("github-confirmation.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "repository": format!("{owner}/{repo}"),
            "github_run_id": args.github_run,
            "artifact_id": artifact.id,
            "artifact_digest": artifact.digest,
            "evidence_digest": imported.digest()?,
        }))?
        .as_slice(),
    )?;
    println!("GitHub confirmation recorded for local run {run_id}.");
    Ok(ExitCode::SUCCESS)
}

fn evidence_jobs(plan: &serde_json::Value) -> anyhow::Result<Vec<GithubJobEvidenceV1>> {
    let jobs = plan.get("jobs").and_then(serde_json::Value::as_array).ok_or_else(|| {
        anyhow::anyhow!(
            "execution plan has no job list\n  fix: preserve the run directory and file a Greenlit defect"
        )
    })?;
    let mut result = Vec::new();
    for job in jobs {
        let id = string_at(job, "id")?;
        let legs = job.get("legs").and_then(serde_json::Value::as_array);
        if let Some(legs) = legs.filter(|legs| !legs.is_empty()) {
            for leg in legs {
                result.push(GithubJobEvidenceV1 {
                    id: id.clone(),
                    name: planned_string(leg.get("name"), &id),
                    steps: evidence_steps(leg.get("steps"))?,
                });
            }
        } else {
            result.push(GithubJobEvidenceV1 {
                id: id.clone(),
                name: planned_string(job.get("name"), &id),
                steps: evidence_steps(job.get("steps"))?,
            });
        }
    }
    Ok(result)
}

fn evidence_steps(value: Option<&serde_json::Value>) -> anyhow::Result<Vec<GithubStepEvidenceV1>> {
    value
        .and_then(serde_json::Value::as_array)
        .unwrap_or(&Vec::new())
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let id = step
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let name = step
                .get("name")
                .map(|value| planned_string(Some(value), ""))
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| default_step_name(index));
            Ok(GithubStepEvidenceV1 { index, id, name })
        })
        .collect()
}

fn default_step_name(index: usize) -> String {
    format!("Greenlit step {}", index + 1)
}

fn planned_string(value: Option<&serde_json::Value>, fallback: &str) -> String {
    value
        .and_then(|value| value.get("value"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or(fallback)
        .to_string()
}

fn string_at(value: &serde_json::Value, key: &str) -> anyhow::Result<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("execution plan is missing string field {key}"))
}

fn pin_workflow(original: &str, lock: &RunLockV1) -> anyhow::Result<String> {
    let mut output = String::with_capacity(original.len());
    for line in original.lines() {
        let mut pinned = line.to_string();
        if line.contains("uses:") {
            for (requested, commit) in &lock.actions {
                pinned = pinned.replace(requested, &replace_action_ref(requested, commit));
            }
        }
        if line.contains("image:") {
            for (requested, digest) in &lock.containers {
                pinned = pinned.replace(requested, &replace_container_ref(requested, digest));
            }
        }
        output.push_str(&pinned);
        output.push('\n');
    }
    for requested in lock.actions.keys() {
        if output.contains(requested) {
            anyhow::bail!(
                "action reference {requested} remained mutable after export\n  fix: preserve the run directory and file a Greenlit defect"
            );
        }
    }
    Ok(output)
}

fn name_unnamed_steps(original: &str, workflow_path: &str) -> anyhow::Result<String> {
    let parsed = greenlit_workflow::parse_workflow(workflow_path, original).map_err(|error| {
        anyhow::anyhow!(
            "could not parse the frozen workflow for export: {error}\n  fix: rerun the workflow with this litci build, then export again"
        )
    })?;
    let mut insertions = BTreeMap::new();
    for job in parsed.jobs {
        for (index, step) in job.steps.into_iter().enumerate() {
            if step.name.is_none() {
                insertions.insert(step.span.start.line as usize, index + 1);
            }
        }
    }
    let mut output = String::with_capacity(original.len() + insertions.len() * 40);
    for (line_number, line) in original.lines().enumerate() {
        let one_based = line_number + 1;
        if let Some(index) = insertions.get(&one_based) {
            let trimmed = line.trim_start();
            let indent_len = line.len().saturating_sub(trimmed.len());
            let Some(rest) = trimmed.strip_prefix("- ") else {
                anyhow::bail!(
                    "could not assign a stable name to an exported step at line {one_based}\n  fix: preserve the run directory and file a Greenlit defect"
                );
            };
            let indent = &line[..indent_len];
            output.push_str(indent);
            output.push_str(&format!("- name: Greenlit step {index}\n"));
            output.push_str(indent);
            output.push_str("  ");
            output.push_str(rest);
            output.push('\n');
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }
    Ok(output)
}

fn replace_action_ref(requested: &str, commit: &str) -> String {
    requested.rsplit_once('@').map_or_else(
        || requested.to_string(),
        |(path, _)| format!("{path}@{commit}"),
    )
}

fn replace_container_ref(requested: &str, digest: &str) -> String {
    let without_digest = requested
        .split_once('@')
        .map_or(requested, |(name, _)| name);
    let slash = without_digest.rfind('/').unwrap_or(0);
    let name = without_digest[slash..]
        .rfind(':')
        .map_or(without_digest, |offset| &without_digest[..slash + offset]);
    format!("{name}@{digest}")
}

fn append_evidence_job(
    pinned: &str,
    evidence: &GithubEvidenceV1,
    _evidence_bytes: &[u8],
) -> anyhow::Result<String> {
    let needs = evidence
        .jobs
        .iter()
        .map(|job| job.id.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ");
    let mut workflow_evidence = evidence.clone();
    workflow_evidence.source_commit = SOURCE_COMMIT_PLACEHOLDER.to_string();
    let encoded = base64_encode(&workflow_evidence.canonical_json().map_err(|error| {
        anyhow::anyhow!(
            "could not serialize the exported evidence template: {error}\n  fix: preserve the run directory and file a Greenlit defect"
        )
    })?);
    let input_checks = evidence
        .inputs
        .iter()
        .enumerate()
        .map(|(index, (name, expected))| {
            format!(
                "      - name: Verify locked input {name}\n        shell: bash\n        env:\n          GREENLIT_INPUT_{index}: ${{{{ inputs.{name} }}}}\n        run: test \"$(printf '%s' \"$GREENLIT_INPUT_{index}\" | base64 -w0)\" = '{}'\n",
                base64_encode(expected.as_bytes())
            )
        })
        .collect::<String>();
    Ok(format!(
        "{pinned}{EXPORT_MARKER}\n  greenlit_confirmation:\n    name: Greenlit evidence\n    needs: [{needs}]\n    runs-on: ubuntu-24.04\n    permissions:\n      contents: read\n    steps:\n{input_checks}      - name: Write Greenlit evidence\n        shell: bash\n        run: |\n          printf '%s' '{encoded}' | base64 --decode > {ARTIFACT_FILE}\n          sed -i \"s/{SOURCE_COMMIT_PLACEHOLDER}/$GITHUB_SHA/\" {ARTIFACT_FILE}\n      - name: Upload Greenlit evidence\n        uses: actions/upload-artifact@{UPLOAD_ARTIFACT_COMMIT}\n        with:\n          name: {ARTIFACT_NAME}\n          path: {ARTIFACT_FILE}\n          if-no-files-found: error\n          retention-days: 7\n"
    ))
}

fn exported_semantic_prefix(workflow: &[u8]) -> anyhow::Result<&[u8]> {
    let marker = format!("\n{EXPORT_MARKER}\n");
    workflow
        .windows(marker.len())
        .position(|window| window == marker.as_bytes())
        .map(|position| &workflow[..position + 1])
        .ok_or_else(|| {
            anyhow::anyhow!(
                "GitHub workflow lacks the Greenlit evidence boundary\n  fix: use the exact exported workflow"
            )
        })
}

fn verify_jobs(expected: &[GithubJobEvidenceV1], remote: &RemoteJobs) -> anyhow::Result<()> {
    if remote.total_count > remote.jobs.len() {
        anyhow::bail!(
            "GitHub run has more than 100 jobs and evidence pagination is incomplete\n  fix: reduce the exported matrix below 100 jobs"
        );
    }
    for expected_job in expected {
        let job = remote
            .jobs
            .iter()
            .find(|job| job.name == expected_job.name)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "GitHub run lacks expected job '{}'\n  fix: use the exact exported workflow run",
                    expected_job.name
                )
            })?;
        if job.conclusion.as_deref() != Some("success") {
            anyhow::bail!(
                "GitHub job '{}' did not pass\n  fix: select a successful exported workflow run",
                job.name
            );
        }
        for expected_step in &expected_job.steps {
            let step = job
                .steps
                .iter()
                .find(|step| step.name == expected_step.name)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "GitHub job '{}' lacks expected step '{}'\n  fix: use the exact exported workflow run",
                        job.name,
                        expected_step.name
                    )
                })?;
            if step.conclusion.as_deref() != Some("success") {
                anyhow::bail!(
                    "GitHub step '{}' in job '{}' did not pass\n  fix: select a successful exported workflow run",
                    step.name,
                    job.name
                );
            }
        }
    }
    let evidence_job = remote
        .jobs
        .iter()
        .find(|job| job.name == "Greenlit evidence");
    if evidence_job.and_then(|job| job.conclusion.as_deref()) != Some("success") {
        anyhow::bail!(
            "the Greenlit evidence job did not pass\n  fix: wait for the exported workflow to complete successfully"
        );
    }
    Ok(())
}

fn unique_evidence_artifact(artifacts: &RemoteArtifacts) -> anyhow::Result<&RemoteArtifact> {
    let matches = artifacts
        .artifacts
        .iter()
        .filter(|artifact| artifact.name == ARTIFACT_NAME && !artifact.expired)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [artifact] => Ok(artifact),
        [] => anyhow::bail!(
            "GitHub run has no unexpired {ARTIFACT_NAME} artifact\n  fix: use a successful, unexpired exported workflow run"
        ),
        _ => anyhow::bail!(
            "GitHub run has multiple {ARTIFACT_NAME} artifacts\n  fix: use the unmodified exported workflow, which uploads exactly one"
        ),
    }
}

fn evidence_from_zip(bytes: &[u8]) -> anyhow::Result<GithubEvidenceV1> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|error| {
        anyhow::anyhow!(
            "GitHub evidence artifact is not a readable ZIP archive: {error}\n  fix: preserve the run and retry with the intact artifact"
        )
    })?;
    if archive.len() != 1 {
        anyhow::bail!(
            "GitHub evidence artifact contains {} entries, expected exactly one\n  fix: use the unmodified exported workflow",
            archive.len()
        );
    }
    let entry = archive.by_index(0)?;
    if entry.name() != ARTIFACT_FILE || entry.size() > MAX_API_BYTES {
        anyhow::bail!(
            "GitHub evidence artifact has an unexpected or oversized member\n  fix: use the unmodified exported workflow"
        );
    }
    let mut content = Vec::with_capacity(entry.size() as usize);
    entry.take(MAX_API_BYTES + 1).read_to_end(&mut content)?;
    serde_json::from_slice(&content).map_err(|error| {
        anyhow::anyhow!(
            "GitHub evidence artifact JSON is invalid: {error}\n  fix: use the unmodified exported workflow"
        )
    })
}

struct GithubClient {
    agent: ureq::Agent,
    base_url: String,
    token: Option<String>,
}

impl GithubClient {
    fn new(token: Option<String>) -> anyhow::Result<Self> {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .max_redirects(5)
            .build();
        let base_url = std::env::var("LITCI_TEST_GITHUB_CONFIRM_API_BASE")
            .unwrap_or_else(|_| "https://api.github.com".to_string());
        if !base_url.starts_with("https://") && !base_url.starts_with("http://127.0.0.1:") {
            anyhow::bail!(
                "GitHub API endpoint is not trusted\n  fix: remove the test-only confirmation endpoint override"
            );
        }
        Ok(Self {
            agent: config.into(),
            base_url,
            token,
        })
    }

    fn get_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> anyhow::Result<T> {
        let bytes = self.get_bytes(path, "application/vnd.github+json", MAX_API_BYTES)?;
        serde_json::from_slice(&bytes).map_err(|error| {
            anyhow::anyhow!(
                "GitHub returned invalid evidence metadata for {path}: {error}\n  fix: retry after GitHub Actions is available"
            )
        })
    }

    fn get_bytes(&self, path: &str, accept: &str, limit: u64) -> anyhow::Result<Vec<u8>> {
        let request = self
            .agent
            .get(format!("{}{}", self.base_url, path))
            .header("Accept", accept)
            .header("X-GitHub-Api-Version", "2026-03-10")
            .header(
                "User-Agent",
                concat!("greenlit-app/", env!("CARGO_PKG_VERSION")),
            );
        let request = if let Some(token) = &self.token {
            request.header("Authorization", format!("Bearer {token}"))
        } else {
            request
        };
        let mut response = request.call().map_err(|error| {
            anyhow::anyhow!(
                "could not read GitHub evidence at {path}: {error}\n  fix: run `litci auth` for private repositories, check connectivity, then retry"
            )
        })?;
        let mut bytes = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(limit + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            anyhow::bail!(
                "GitHub evidence response at {path} exceeded {limit} bytes\n  fix: use the exact bounded Greenlit evidence artifact"
            );
        }
        Ok(bytes)
    }
}

#[derive(Deserialize)]
struct RemoteRun {
    head_sha: String,
    path: String,
    event: String,
    conclusion: Option<String>,
}

#[derive(Deserialize)]
struct RemoteJobs {
    total_count: usize,
    jobs: Vec<RemoteJob>,
}

#[derive(Deserialize)]
struct RemoteJob {
    name: String,
    conclusion: Option<String>,
    #[serde(default)]
    steps: Vec<RemoteStep>,
}

#[derive(Deserialize)]
struct RemoteStep {
    name: String,
    conclusion: Option<String>,
}

#[derive(Deserialize)]
struct RemoteArtifacts {
    artifacts: Vec<RemoteArtifact>,
}

#[derive(Deserialize)]
struct RemoteArtifact {
    id: u64,
    name: String,
    expired: bool,
    digest: String,
}

fn parse_repository(value: &str) -> anyhow::Result<(&str, &str)> {
    let Some((owner, repo)) = value.split_once('/') else {
        anyhow::bail!("invalid GitHub repository '{value}'\n  fix: pass --repository OWNER/REPO");
    };
    if owner.is_empty()
        || repo.is_empty()
        || repo.contains('/')
        || !owner
            .bytes()
            .chain(repo.bytes())
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        anyhow::bail!("invalid GitHub repository '{value}'\n  fix: pass --repository OWNER/REPO");
    }
    Ok((owner, repo))
}

fn safe_source_path(value: &str) -> anyhow::Result<PathBuf> {
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        anyhow::bail!(
            "locked workflow path is unsafe\n  fix: preserve the run directory and file a Greenlit defect"
        );
    }
    Ok(path.to_path_buf())
}

fn runs_root() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| {
        anyhow::anyhow!(
            "could not find run evidence because HOME is not set\n  fix: set HOME to an absolute directory, then retry"
        )
    })?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        anyhow::bail!(
            "could not find run evidence because HOME is not absolute\n  fix: set HOME to an absolute directory, then retry"
        );
    }
    Ok(home.join(".litci").join("runs"))
}

fn validate_run_id(run_id: &str) -> anyhow::Result<String> {
    if run_id.is_empty()
        || !run_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        anyhow::bail!(
            "invalid run identity '{run_id}'\n  fix: copy the exact run identity printed by `litci run`"
        );
    }
    Ok(run_id.to_string())
}

fn latest_run_id(runs: &Path) -> anyhow::Result<String> {
    fs::read_dir(runs)
        .map_err(|error| anyhow::anyhow!("could not list {}: {error}", runs.display()))?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("result.json").is_file())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| validate_run_id(name).is_ok())
        .max()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no completed local run exists\n  fix: run `litci run` once, then retry"
            )
        })
}

fn completed_run_dir(runs: &Path, run_id: &str) -> anyhow::Result<PathBuf> {
    let directory = runs.join(run_id);
    if !directory.join("result.json").is_file() {
        anyhow::bail!(
            "completed run evidence '{run_id}' does not exist\n  fix: copy a completed run identity printed by `litci run`"
        );
    }
    Ok(directory)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> anyhow::Result<T> {
    let bytes = fs::read(path).map_err(|error| {
        anyhow::anyhow!(
            "could not read evidence {}: {error}\n  fix: use `litci doctor` to diagnose local state",
            path.display()
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        anyhow::anyhow!(
            "evidence {} is invalid JSON: {error}\n  fix: preserve the directory and use `litci doctor`",
            path.display()
        )
    })
}

fn create_clean_export_dir(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        anyhow::bail!(
            "export destination {} already exists\n  fix: choose a new --output directory or move the old export aside",
            path.display()
        );
    }
    fs::create_dir_all(path).map_err(|error| {
        anyhow::anyhow!(
            "could not create export directory {}: {error}\n  fix: choose a writable --output directory",
            path.display()
        )
    })
}

fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    File::open(path.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()?;
    Ok(())
}

fn sha256_identity(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut identity = String::with_capacity(71);
    identity.push_str("sha256:");
    for byte in digest {
        identity.push_str(&format!("{byte:02x}"));
    }
    identity
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let block = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        output.push(ALPHABET[((block >> 18) & 63) as usize] as char);
        output.push(ALPHABET[((block >> 12) & 63) as usize] as char);
        output.push(if chunk.len() > 1 {
            ALPHABET[((block >> 6) & 63) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[(block & 63) as usize] as char
        } else {
            '='
        });
    }
    output
}
