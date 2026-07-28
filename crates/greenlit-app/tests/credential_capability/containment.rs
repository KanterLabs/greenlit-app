use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::net::TcpListener;
use std::path::PathBuf;

use crate::credential_capability_support::{
    CredentialPayload, PersistentCredential, assert_clean, assert_failure, assert_stderr_contains,
};
use crate::support::Sandbox;

pub(super) fn assert_plan_and_run_quarantine(
    sandbox: &Sandbox,
    credential: &PersistentCredential,
    payload: &CredentialPayload,
    secrets: &[&str],
) {
    assert_quarantined_command(
        sandbox,
        credential,
        &["plan", "-W", "wf.yml"],
        false,
        payload,
        secrets,
    );
    assert_quarantined_command(
        sandbox,
        credential,
        &["run", "-W", "wf.yml", "--no-input"],
        true,
        payload,
        secrets,
    );
}

fn assert_quarantined_command(
    sandbox: &Sandbox,
    credential: &PersistentCredential,
    args: &[&str],
    retains_result: bool,
    payload: &CredentialPayload,
    secrets: &[&str],
) {
    let listener =
        TcpListener::bind("127.0.0.1:0").expect("bind credential quarantine recording boundary");
    listener
        .set_nonblocking(true)
        .expect("make credential quarantine boundary nonblocking");
    let base_url = format!(
        "http://{}",
        listener
            .local_addr()
            .expect("credential quarantine boundary address")
    );
    let before = retained_runs(sandbox);
    let output = sandbox.run_with_credential_keyring(
        args,
        credential.description(),
        &[
            ("LITCI_TEST_GITHUB_OAUTH_BASE_URL", base_url.as_str()),
            ("LITCI_TEST_GITHUB_API_BASE_URL", base_url.as_str()),
            ("DOCKER_HOST", "ssh://credential-quarantine.invalid"),
        ],
    );
    assert_clean(&output, sandbox, secrets);
    assert_failure(&output, "credential variable-context quarantine");
    assert_stderr_contains(
        &output,
        "uncertified capability `variable.context` at `wf.yml:5:9`",
        "credential variable-context quarantine",
    );
    assert_stderr_contains(
        &output,
        "stabilization Phase 16",
        "credential variable-context quarantine",
    );
    assert_stderr_contains(
        &output,
        "remove the reachable `vars` context reference",
        "credential variable-context quarantine",
    );
    match listener.accept() {
        Err(error) if error.kind() == ErrorKind::WouldBlock => {}
        Ok(_) => panic!("credential quarantine contacted GitHub"),
        Err(error) => panic!("could not inspect credential quarantine boundary: {error}"),
    }
    assert!(
        !sandbox.home().join(".litci/daemon/v1.sock").exists(),
        "credential quarantine started the daemon"
    );
    credential.assert_payload_unchanged(payload);
    let after = retained_runs(sandbox);
    if retains_result {
        let created = after.difference(&before).collect::<Vec<_>>();
        assert_eq!(
            created.len(),
            1,
            "credential quarantine retained the wrong number of runs"
        );
        let result: serde_json::Value = serde_json::from_slice(
            &std::fs::read(created[0].join("result.json"))
                .expect("read credential quarantine result"),
        )
        .expect("parse credential quarantine result");
        assert_eq!(result["conclusion"], "blocked");
        assert_eq!(result["compatibility"], "unsupported");
        assert_eq!(result["assurance"], "none");
    } else {
        assert_eq!(
            after, before,
            "credential-quarantined plan unexpectedly retained a run"
        );
    }
}

fn retained_runs(sandbox: &Sandbox) -> BTreeSet<PathBuf> {
    let root = sandbox.home().join(".litci/runs");
    match std::fs::read_dir(root) {
        Ok(entries) => entries
            .map(|entry| entry.expect("read retained credential run").path())
            .filter(|path| path.is_dir())
            .collect(),
        Err(error) if error.kind() == ErrorKind::NotFound => BTreeSet::new(),
        Err(error) => panic!("could not enumerate retained credential runs: {error}"),
    }
}
