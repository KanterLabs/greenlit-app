//! Versioned, machine-readable evidence used to lock and classify runs.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Whether Greenlit knows how faithfully one workflow feature is supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingDisposition {
    /// The behavior is implemented with no known semantic difference.
    Supported,
    /// The run can execute, but a known difference prevents equivalence.
    Degraded,
    /// Greenlit cannot execute the required semantics reliably.
    Unsupported,
}

/// One source-located compatibility fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureFinding {
    /// Stable machine-readable feature identifier.
    pub code: String,
    /// Compatibility disposition.
    pub disposition: FindingDisposition,
    /// Workflow path or logical scope affected by the finding.
    pub scope: String,
    /// Human-readable reason.
    pub reason: String,
}

/// Complete compatibility analysis for a locked run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupportReport {
    /// Findings sorted by code and scope before locking.
    pub findings: Vec<FeatureFinding>,
}

impl SupportReport {
    /// Returns the strongest compatibility restriction in the report.
    #[must_use]
    pub fn compatibility(&self) -> Compatibility {
        if self
            .findings
            .iter()
            .any(|finding| finding.disposition == FindingDisposition::Unsupported)
        {
            Compatibility::Unsupported
        } else if self
            .findings
            .iter()
            .any(|finding| finding.disposition == FindingDisposition::Degraded)
        {
            Compatibility::Degraded
        } else {
            Compatibility::Supported
        }
    }

    /// Sorts findings into their canonical lock order.
    pub fn canonicalize(&mut self) {
        self.findings.sort_by(|left, right| {
            (&left.code, &left.scope, &left.reason).cmp(&(&right.code, &right.scope, &right.reason))
        });
    }
}

/// Immutable source identity stored in a run lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedSource {
    /// Full Git commit at capture time.
    pub commit: String,
    /// Canonical current-worktree content digest.
    pub snapshot_digest: String,
    /// Whether current bytes differ from the commit.
    pub dirty: bool,
    /// Repository-relative workflow path.
    pub workflow_path: String,
    /// Digest of the selected workflow bytes.
    pub workflow_digest: String,
}

/// Immutable identity of the environment selected for one concrete job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerLockV1 {
    /// Runner expression or label requested by the workflow.
    pub requested_label: String,
    /// Concrete supported runner label selected after expression evaluation.
    pub resolved_label: String,
    /// Environment implementation that supplied the runner.
    pub provider: String,
    /// Local immutable image reference used to start the sandbox.
    pub image_reference: String,
    /// Engine-reported immutable image identity.
    pub image_digest: String,
    /// Selected operating system.
    pub os: String,
    /// Selected architecture.
    pub architecture: String,
    /// Greenlit runner implementation version.
    pub runner_version: String,
}

/// Version-one immutable pre-execution resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLockV1 {
    /// Schema discriminator.
    pub schema_version: u32,
    /// Frozen source identity.
    pub source: LockedSource,
    /// Synthetic event name.
    pub event: String,
    /// Supplied dispatch inputs, deterministically ordered.
    pub inputs: BTreeMap<String, String>,
    /// Requested job filter, if any.
    pub selected_job: Option<String>,
    /// Exact matrix selector supplied for the requested job.
    #[serde(default)]
    pub selected_matrix: BTreeMap<String, String>,
    /// Whether Greenlit preparation was restricted to verified local content.
    #[serde(default)]
    pub offline: bool,
    /// Whether transparent Greenlit mutable caches were disabled.
    #[serde(default)]
    pub clean: bool,
    /// Whether workflow external networking and late mutable inputs were
    /// forbidden.
    #[serde(default)]
    pub hermetic: bool,
    /// Host/runtime facts that contribute to the environment fingerprint.
    #[serde(default)]
    pub runtime: BTreeMap<String, String>,
    /// Runner identities by concrete job.
    pub runners: BTreeMap<String, RunnerLockV1>,
    /// Action requested refs mapped to resolved commits.
    pub actions: BTreeMap<String, String>,
    /// Container requested refs mapped to resolved OCI digests.
    pub containers: BTreeMap<String, String>,
    /// Toolchain name mapped to exact identity.
    pub toolchains: BTreeMap<String, String>,
    /// Secret name mapped only to an opaque revision digest.
    pub secret_revisions: BTreeMap<String, String>,
    /// Preflight semantic support report.
    pub compatibility: SupportReport,
}

impl RunLockV1 {
    /// Creates a schema-one lock with empty resolution maps.
    #[must_use]
    pub fn new(source: LockedSource, event: impl Into<String>) -> Self {
        Self {
            schema_version: 1,
            source,
            event: event.into(),
            inputs: BTreeMap::new(),
            selected_job: None,
            selected_matrix: BTreeMap::new(),
            offline: false,
            clean: false,
            hermetic: false,
            runtime: BTreeMap::new(),
            runners: BTreeMap::new(),
            actions: BTreeMap::new(),
            containers: BTreeMap::new(),
            toolchains: BTreeMap::new(),
            secret_revisions: BTreeMap::new(),
            compatibility: SupportReport::default(),
        }
    }

    /// Returns byte-stable compact JSON for hashing and persistence.
    pub fn canonical_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// Returns the SHA-256 identity of canonical JSON, prefixed with `sha256:`.
    pub fn digest(&self) -> Result<String, serde_json::Error> {
        self.canonical_json().map(|bytes| sha256_identity(&bytes))
    }
}

/// Version-one lock finalized for one concrete job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobLockV1 {
    /// Schema discriminator.
    pub schema_version: u32,
    /// Parent RunLock digest.
    pub run_lock_digest: String,
    /// Concrete job identifier.
    pub job_id: String,
    /// Deterministically ordered matrix values.
    pub matrix: BTreeMap<String, serde_json::Value>,
    /// Digests of completed dependency evidence.
    pub needs_evidence: BTreeMap<String, String>,
    /// Immutable runner environment fingerprint.
    pub environment_fingerprint: String,
}

impl JobLockV1 {
    /// Returns byte-stable compact JSON for hashing and persistence.
    pub fn canonical_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// Returns the SHA-256 identity of canonical JSON.
    pub fn digest(&self) -> Result<String, serde_json::Error> {
        self.canonical_json().map(|bytes| sha256_identity(&bytes))
    }
}

/// One append-only fact in a run's execution trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceEventV1 {
    /// Schema discriminator.
    pub schema_version: u32,
    /// Monotonic sequence within this run, beginning at one.
    pub sequence: u64,
    /// Stable machine-readable event name.
    pub event: String,
    /// Deterministically ordered event attributes.
    pub attributes: BTreeMap<String, String>,
}

impl TraceEventV1 {
    /// Creates a schema-one trace event.
    #[must_use]
    pub fn new(
        sequence: u64,
        event: impl Into<String>,
        attributes: BTreeMap<String, String>,
    ) -> Self {
        Self {
            schema_version: 1,
            sequence,
            event: event.into(),
            attributes,
        }
    }

    /// Returns byte-stable compact JSON.
    pub fn canonical_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}

/// What happened while executing the selected work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionConclusion {
    /// Every selected required step passed.
    Passed,
    /// A selected required step failed.
    Failed,
    /// The run was canceled.
    Canceled,
    /// Preflight support policy blocked execution.
    Blocked,
    /// Immutable preparation failed before workflow execution.
    PreparationFailed,
    /// Recovery found a formerly active run after interruption.
    Aborted,
}

/// Strongest compatibility statement justified by support evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Compatibility {
    /// No known differences affect the run.
    Supported,
    /// Known differences prevent equivalence.
    Degraded,
    /// Required semantics are unavailable.
    Unsupported,
}

/// Strongest assurance statement justified by execution evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Assurance {
    /// No passing assurance is available.
    None,
    /// Passed locally in isolated execution.
    Local,
    /// Passed with Greenlit mutable caches disabled.
    Clean,
    /// Passed in an exact pinned hermetic environment.
    Hermetic,
    /// Matching external GitHub evidence also passed.
    GithubConfirmed,
}

/// Facts supplied to the pure result classifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultEvidence {
    /// Runtime conclusion.
    pub conclusion: ExecutionConclusion,
    /// Preflight support report.
    pub support: SupportReport,
    /// Whether mutable Greenlit caches were disabled.
    pub clean: bool,
    /// Whether all hermetic identity requirements were proven.
    pub hermetic: bool,
    /// Whether matching external GitHub evidence was verified.
    pub github_confirmed: bool,
}

/// Identity of one authored step in exported GitHub confirmation evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubStepEvidenceV1 {
    /// Stable zero-based authored position within the job.
    pub index: usize,
    /// Authored step id when one exists.
    pub id: Option<String>,
    /// Display name expected from the GitHub jobs API.
    pub name: String,
}

/// Identity of one expanded job in exported GitHub confirmation evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubJobEvidenceV1 {
    /// Authored job id.
    pub id: String,
    /// Exact GitHub display name, including matrix values when applicable.
    pub name: String,
    /// Authored steps in execution order.
    pub steps: Vec<GithubStepEvidenceV1>,
}

/// Version-one external evidence artifact uploaded by an exported workflow.
///
/// The artifact contains only immutable, non-secret identities. GitHub run,
/// job, step, workflow-content, and artifact metadata are checked separately
/// before this document can upgrade a local result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubEvidenceV1 {
    /// Schema discriminator.
    pub schema_version: u32,
    /// Full clean source commit.
    pub source_commit: String,
    /// Digest of the original selected workflow.
    pub workflow_digest: String,
    /// Digest of the separate fully pinned exported workflow.
    pub exported_workflow_digest: String,
    /// Repository-relative path at which the exported workflow must run.
    pub exported_workflow_path: String,
    /// Trigger event selected locally.
    pub event: String,
    /// Typed dispatch inputs rendered into stable strings.
    pub inputs: BTreeMap<String, String>,
    /// Action aliases mapped to full commits.
    pub actions: BTreeMap<String, String>,
    /// Container aliases mapped to OCI digests.
    pub containers: BTreeMap<String, String>,
    /// Toolchain requests mapped to exact identities.
    pub toolchains: BTreeMap<String, String>,
    /// Expanded job and authored-step identities.
    pub jobs: Vec<GithubJobEvidenceV1>,
}

impl GithubEvidenceV1 {
    /// Returns byte-stable compact JSON.
    pub fn canonical_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// Returns the SHA-256 identity of the canonical artifact bytes.
    pub fn digest(&self) -> Result<String, serde_json::Error> {
        self.canonical_json().map(|bytes| sha256_identity(&bytes))
    }

    /// Verifies every lock field which can be equivalent across local and
    /// GitHub execution. Secrets and host runtime fingerprints are
    /// intentionally excluded: the exported workflow never receives secret
    /// values and GitHub necessarily supplies a different control plane.
    pub fn matches_lock(&self, lock: &RunLockV1) -> Result<(), String> {
        if lock.source.dirty {
            return Err("the local run used uncommitted source".to_string());
        }
        let checks = [
            (
                self.source_commit == lock.source.commit,
                "source commit differs",
            ),
            (
                self.workflow_digest == lock.source.workflow_digest,
                "workflow semantics digest differs",
            ),
            (self.event == lock.event, "event differs"),
            (self.inputs == lock.inputs, "workflow inputs differ"),
            (self.actions == lock.actions, "resolved actions differ"),
            (
                self.containers == lock.containers,
                "resolved containers differ",
            ),
            (
                self.toolchains == lock.toolchains,
                "resolved toolchains differ",
            ),
        ];
        checks
            .into_iter()
            .find_map(|(matches, reason)| (!matches).then(|| reason.to_string()))
            .map_or(Ok(()), Err)
    }
}

/// Version-one independently dimensioned execution result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionResultV1 {
    /// Schema discriminator.
    pub schema_version: u32,
    /// Runtime outcome.
    pub conclusion: ExecutionConclusion,
    /// Semantic compatibility.
    pub compatibility: Compatibility,
    /// Strongest proven assurance.
    pub assurance: Assurance,
    /// Machine-readable explanations for limitations.
    pub reasons: Vec<String>,
}

impl ExecutionResultV1 {
    /// Classifies a result without allowing an unsupported or failed run to
    /// acquire a green assurance.
    #[must_use]
    pub fn classify(evidence: &ResultEvidence) -> Self {
        let compatibility = evidence.support.compatibility();
        let passing = evidence.conclusion == ExecutionConclusion::Passed
            && compatibility != Compatibility::Unsupported;
        let assurance = if !passing {
            Assurance::None
        } else if evidence.github_confirmed
            && evidence.hermetic
            && compatibility == Compatibility::Supported
        {
            Assurance::GithubConfirmed
        } else if evidence.hermetic && compatibility == Compatibility::Supported {
            Assurance::Hermetic
        } else if evidence.clean {
            Assurance::Clean
        } else {
            Assurance::Local
        };
        let mut reasons = evidence
            .support
            .findings
            .iter()
            .filter(|finding| finding.disposition != FindingDisposition::Supported)
            .map(|finding| format!("{}: {}", finding.code, finding.reason))
            .collect::<Vec<_>>();
        if evidence.github_confirmed && assurance != Assurance::GithubConfirmed {
            reasons.push(
                "github_confirmation_disqualified: local identity or compatibility did not qualify"
                    .to_string(),
            );
        }
        Self {
            schema_version: 1,
            conclusion: evidence.conclusion,
            compatibility,
            assurance,
            reasons,
        }
    }

    /// Returns byte-stable compact JSON.
    pub fn canonical_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}

fn sha256_identity(bytes: &[u8]) -> String {
    format!("sha256:{}", hex_digest(&Sha256::digest(bytes)))
}

/// Produces an opaque revision identity without retaining the secret value.
#[must_use]
pub fn opaque_revision(secret: &[u8]) -> String {
    sha256_identity(secret)
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}
