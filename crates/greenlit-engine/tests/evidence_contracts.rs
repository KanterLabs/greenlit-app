use std::collections::BTreeMap;

use greenlit_engine::{
    Assurance, Compatibility, ExecutionConclusion, ExecutionResultV1, FeatureFinding,
    FindingDisposition, JobLockV1, ResultEvidence, SupportReport,
};

#[test]
fn unsupported_behavior_never_receives_passing_assurance() {
    let support = SupportReport {
        findings: vec![FeatureFinding {
            code: "github.oidc".to_string(),
            disposition: FindingDisposition::Unsupported,
            scope: "jobs.deploy".to_string(),
            reason: "GitHub OIDC issuance is unavailable locally".to_string(),
        }],
    };
    let result = ExecutionResultV1::classify(&ResultEvidence {
        conclusion: ExecutionConclusion::Passed,
        support,
        clean: true,
        hermetic: true,
        github_confirmed: true,
    });
    assert_eq!(result.compatibility, Compatibility::Unsupported);
    assert_eq!(result.assurance, Assurance::None);
}

#[test]
fn github_confirmation_requires_hermetic_supported_evidence() {
    let result = ExecutionResultV1::classify(&ResultEvidence {
        conclusion: ExecutionConclusion::Passed,
        support: SupportReport::default(),
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
