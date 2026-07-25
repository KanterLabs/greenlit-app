use std::io::Write;

use greenlit_engine::{
    ExecutionConclusion, ExecutionResultV1, LockedSource, ResultEvidence, RunLockV1, SupportReport,
};
use sha2::{Digest, Sha256};

use super::support::fake_github::{Canned, FakeGitHub};
use super::support::{Sandbox, stderr_text, stdout_text};

const RUN_ID: &str = "00000000000000000000000000000001-00000001-0000";
const COMMIT: &str = "0123456789012345678901234567890123456789";

fn seed_completed_run(sandbox: &Sandbox, hermetic: bool) {
    let workflow = "name: test\non: push\njobs:\n  test:\n    name: test\n    runs-on: ubuntu-24.04\n    steps:\n      - name: Pass\n        run: echo pass\n";
    let mut lock = RunLockV1::new(
        LockedSource {
            commit: COMMIT.to_string(),
            snapshot_digest: "sha256:source".to_string(),
            dirty: false,
            workflow_path: ".github/workflows/ci.yml".to_string(),
            workflow_digest: sha256(workflow.as_bytes()),
        },
        "push",
    );
    lock.clean = hermetic;
    lock.hermetic = hermetic;
    let result = ExecutionResultV1::classify(&ResultEvidence {
        conclusion: ExecutionConclusion::Passed,
        support: SupportReport::default(),
        clean: hermetic,
        hermetic,
        github_confirmed: false,
    });
    let root = format!(".litci/runs/{RUN_ID}");
    sandbox.write_home(
        &format!("{root}/run-lock.json"),
        &serde_json::to_string(&lock).unwrap(),
    );
    sandbox.write_home(
        &format!("{root}/result.json"),
        &serde_json::to_string(&result).unwrap(),
    );
    sandbox.write_home(&format!("{root}/source/.github/workflows/ci.yml"), workflow);
    sandbox.write_home(
        &format!("{root}/execution-plan.json"),
        r#"{"jobs":[{"id":"test","name":{"evaluation":"static","value":"test"},"legs":[],"steps":[{"id":null,"name":{"evaluation":"static","value":"Pass"},"kind":{"kind":"run","script":{"evaluation":"static","value":"echo pass"}}}]}]}"#,
    );
}

#[test]
fn export_is_separate_pinned_and_confirmation_needs_matching_external_evidence() {
    let sandbox = Sandbox::new();
    seed_completed_run(&sandbox, true);
    let output = sandbox.run(&["export", RUN_ID, "--output", "exported"]);
    assert!(output.status.success(), "{}", stderr_text(&output));
    let workflow =
        std::fs::read_to_string(sandbox.root().join("exported/greenlit-confirmation.yml"))
            .expect("exported workflow");
    assert!(workflow.contains("greenlit-confirmation-evidence-v1"));
    assert!(workflow.contains("actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02"));
    assert!(
        !sandbox
            .root()
            .join(".github/workflows/greenlit-confirmation.yml")
            .exists()
    );

    let mismatch = sandbox.run(&[
        "confirm",
        RUN_ID,
        "--repository",
        "owner/repo",
        "--github-run",
        "42",
    ]);
    assert!(!mismatch.status.success());
    assert!(stderr_text(&mismatch).contains("could not read GitHub evidence"));
}

#[test]
fn a_matching_github_artifact_upgrades_only_an_eligible_local_result() {
    let sandbox = Sandbox::new();
    seed_completed_run(&sandbox, true);
    let exported = sandbox.run(&["export", RUN_ID, "--output", "exported"]);
    assert!(exported.status.success(), "{}", stderr_text(&exported));
    let workflow =
        std::fs::read(sandbox.root().join("exported/greenlit-confirmation.yml")).unwrap();
    let evidence =
        std::fs::read(sandbox.root().join("exported/greenlit-evidence-v1.json")).unwrap();
    let zip = evidence_zip(&evidence);
    let artifact_digest = sha256(&zip);
    let fake = FakeGitHub::bind();
    let base = fake.base_url();
    let server = fake.serve(vec![
        Canned::json(
            200,
            "OK",
            format!(
                r#"{{"head_sha":"{COMMIT}","path":".github/workflows/greenlit-confirmation.yml@main","event":"push","conclusion":"success"}}"#
            ),
        ),
        Canned::bytes(200, "OK", workflow),
        Canned::json(
            200,
            "OK",
            r#"{"total_count":2,"jobs":[{"name":"test","conclusion":"success","steps":[{"name":"Pass","conclusion":"success"}]},{"name":"Greenlit evidence","conclusion":"success","steps":[]}]}"#,
        ),
        Canned::json(
            200,
            "OK",
            format!(
                r#"{{"artifacts":[{{"id":7,"name":"greenlit-evidence-v1","expired":false,"digest":"{artifact_digest}"}}]}}"#
            ),
        ),
        Canned::bytes(200, "OK", zip),
    ]);
    let confirmed = sandbox.run_with_env(
        &[
            "confirm",
            RUN_ID,
            "--repository",
            "owner/repo",
            "--github-run",
            "42",
        ],
        &[("LITCI_TEST_GITHUB_CONFIRM_API_BASE", &base)],
    );
    assert!(confirmed.status.success(), "{}", stderr_text(&confirmed));
    assert!(stdout_text(&confirmed).contains("confirmation recorded"));
    let paths = server.join().unwrap();
    assert_eq!(paths.len(), 5);
    let result: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            sandbox
                .home()
                .join(format!(".litci/runs/{RUN_ID}/result.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(result["assurance"], "github_confirmed");
}

#[test]
fn one_remote_job_cannot_satisfy_two_locked_jobs_with_the_same_display_name() {
    let sandbox = Sandbox::new();
    seed_completed_run(&sandbox, true);
    sandbox.write_home(
        &format!(".litci/runs/{RUN_ID}/execution-plan.json"),
        r#"{"jobs":[{"id":"first","name":{"evaluation":"static","value":"duplicate"},"legs":[],"steps":[{"id":null,"name":{"evaluation":"static","value":"Pass"},"kind":{"kind":"run","script":{"evaluation":"static","value":"echo pass"}}}]},{"id":"second","name":{"evaluation":"static","value":"duplicate"},"legs":[],"steps":[{"id":null,"name":{"evaluation":"static","value":"Pass"},"kind":{"kind":"run","script":{"evaluation":"static","value":"echo pass"}}}]}]}"#,
    );
    let exported = sandbox.run(&["export", RUN_ID, "--output", "exported"]);
    assert!(exported.status.success(), "{}", stderr_text(&exported));
    let workflow =
        std::fs::read(sandbox.root().join("exported/greenlit-confirmation.yml")).unwrap();
    let fake = FakeGitHub::bind();
    let base = fake.base_url();
    let server = fake.serve(vec![
        Canned::json(
            200,
            "OK",
            format!(
                r#"{{"head_sha":"{COMMIT}","path":".github/workflows/greenlit-confirmation.yml@main","event":"push","conclusion":"success"}}"#
            ),
        ),
        Canned::bytes(200, "OK", workflow),
        Canned::json(
            200,
            "OK",
            r#"{"total_count":2,"jobs":[{"name":"duplicate","conclusion":"success","steps":[{"name":"Pass","conclusion":"success"}]},{"name":"Greenlit evidence","conclusion":"success","steps":[]}]}"#,
        ),
    ]);
    let rejected = sandbox.run_with_env(
        &[
            "confirm",
            RUN_ID,
            "--repository",
            "owner/repo",
            "--github-run",
            "42",
        ],
        &[("LITCI_TEST_GITHUB_CONFIRM_API_BASE", &base)],
    );
    assert!(!rejected.status.success());
    assert!(stderr_text(&rejected).contains("lacks expected job 'duplicate'"));
    assert_eq!(server.join().unwrap().len(), 3);
}

fn evidence_zip(evidence: &[u8]) -> Vec<u8> {
    let cursor = std::io::Cursor::new(Vec::new());
    let mut archive = zip::ZipWriter::new(cursor);
    archive
        .start_file(
            "greenlit-evidence-v1.json",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
    archive.write_all(evidence).unwrap();
    archive.finish().unwrap().into_inner()
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut value = String::from("sha256:");
    for byte in digest {
        value.push_str(&format!("{byte:02x}"));
    }
    value
}
