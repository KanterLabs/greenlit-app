use greenlit_engine::{
    Assurance, ExecutionConclusion, ExecutionResultV1, FeatureFinding, FindingDisposition,
    GithubEvidenceV1, JobLockV1, LockedSource, ResultEvidence, RunLockV1,
    SUPPORT_CERTIFICATION_WITNESS, SupportReport,
};
use std::collections::BTreeMap;

#[test]
fn github_confirmation_requires_hermetic_supported_evidence() {
    let result = ExecutionResultV1::classify(&ResultEvidence {
        conclusion: ExecutionConclusion::Passed,
        support: SupportReport {
            findings: vec![FeatureFinding {
                code: SUPPORT_CERTIFICATION_WITNESS.to_string(),
                disposition: FindingDisposition::Supported,
                scope: "run".to_string(),
                reason: "the selected capability set is certified".to_string(),
            }],
        },
        clean: true,
        hermetic: false,
        github_confirmed: true,
    });
    assert_eq!(result.assurance, Assurance::Clean);
    assert!(
        result
            .reasons
            .iter()
            .any(|reason| reason.starts_with("github_confirmation_disqualified:"))
    );
}

#[test]
fn external_evidence_must_match_every_equivalent_lock_field() {
    let mut lock = RunLockV1::new(
        LockedSource {
            commit: "0123456789012345678901234567890123456789".to_string(),
            snapshot_digest: "sha256:source".to_string(),
            dirty: false,
            workflow_path: ".github/workflows/ci.yml".to_string(),
            workflow_digest: "sha256:workflow".to_string(),
        },
        "push",
    );
    lock.inputs.insert("mode".to_string(), "full".to_string());
    lock.actions.insert(
        "actions/checkout@v4".to_string(),
        "0123456789012345678901234567890123456789".to_string(),
    );
    lock.containers
        .insert("postgres:17".to_string(), "sha256:postgres".to_string());
    lock.toolchains
        .insert("node".to_string(), "24.4.1".to_string());
    let evidence = GithubEvidenceV1 {
        schema_version: 1,
        source_commit: lock.source.commit.clone(),
        workflow_digest: lock.source.workflow_digest.clone(),
        exported_workflow_digest: "sha256:export".to_string(),
        exported_workflow_path: ".github/workflows/greenlit-confirmation.yml".to_string(),
        event: lock.event.clone(),
        inputs: lock.inputs.clone(),
        actions: lock.actions.clone(),
        containers: lock.containers.clone(),
        toolchains: lock.toolchains.clone(),
        jobs: Vec::new(),
    };
    assert_eq!(evidence.matches_lock(&lock), Ok(()));
    let mut changed = evidence.clone();
    changed
        .actions
        .insert("actions/checkout@v4".to_string(), "moved".to_string());
    assert_eq!(
        changed.matches_lock(&lock),
        Err("resolved actions differ".to_string())
    );
    lock.source.dirty = true;
    assert_eq!(
        evidence.matches_lock(&lock),
        Err("the local run used uncommitted source".to_string())
    );
}

#[test]
fn job_lock_json_and_digest_are_byte_stable() {
    let lock = JobLockV1 {
        schema_version: 1,
        run_lock_digest: "sha256:run".to_string(),
        job_id: "test".to_string(),
        matrix: BTreeMap::from([
            ("node".to_string(), serde_json::json!(24)),
            ("os".to_string(), serde_json::json!("ubuntu-24.04")),
        ]),
        needs_evidence: BTreeMap::new(),
        environment_fingerprint: "sha256:environment".to_string(),
    };
    let first_json = lock.canonical_json().expect("lock should serialize");
    let second_json = lock.canonical_json().expect("lock should serialize again");
    let first_digest = lock.digest().expect("lock should hash");
    let second_digest = lock.digest().expect("lock should hash again");
    assert_eq!(first_json, second_json);
    assert_eq!(first_digest, second_digest);
}
