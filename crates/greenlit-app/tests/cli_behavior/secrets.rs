//! Integration oracle for the secrets resolution chain reaching `litci run`
//! before any container work, and the reserved `GITHUB_TOKEN` special case
//! (`PHASE-3-actions.md` Secrets/Auth). Every case uses the same
//! `DOCKER_HOST`-rejection trick `cli_behavior::run_preflight` already
//! establishes: a run that fails with the `DOCKER_HOST` message got past
//! secrets resolution; a run that fails some other way never reached engine
//! detection at all.

use super::support;
use super::support::Sandbox;
use std::os::unix::fs::PermissionsExt;

const SSH_DOCKER_HOST: (&str, &str) = ("DOCKER_HOST", "ssh://example");

const SECRET_WORKFLOW: &str = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    env:
      TOKEN: ${{ secrets.API_TOKEN }}
    steps:
      - run: echo hi
";

const GITHUB_TOKEN_WORKFLOW: &str = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    env:
      TOKEN: ${{ secrets.GITHUB_TOKEN }}
    steps:
      - run: echo hi
";

// `write-all`/`read-all` imply `id-token: write`, which v0 rejects outright
// as out-of-scope OIDC before planning even reaches the permission notice
// (`fixtures`/`plan_rejections.rs`'s `oidc_write_permissions_are_rejected`).
// An explicit `contents: write` exceeds the local token's read-only grant
// without touching that rejected scope.
const GITHUB_TOKEN_WORKFLOW_WITH_CONTENTS_WRITE: &str = "\
on: push
permissions:
  contents: write
jobs:
  build:
    runs-on: ubuntu-latest
    env:
      TOKEN: ${{ secrets.GITHUB_TOKEN }}
    steps:
      - run: echo hi
";

fn sandbox_with(workflow: &str) -> Sandbox {
    let sandbox = Sandbox::new();
    sandbox.write("wf.yml", workflow);
    sandbox.init_git();
    sandbox
}

#[test]
fn cli_secret_override_reaches_engine_detection_and_never_leaks() {
    const SENTINEL: &str = "cli-secret-value-must-not-leak-7391";
    let sandbox = sandbox_with(SECRET_WORKFLOW);
    let output = sandbox.run_with_env(
        &[
            "run",
            "-W",
            "wf.yml",
            "-s",
            &format!("API_TOKEN={SENTINEL}"),
        ],
        &[SSH_DOCKER_HOST],
    );
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("DOCKER_HOST"), "{stderr}");
    assert!(!stderr.contains(SENTINEL), "{stderr}");
    assert!(!support::stdout_text(&output).contains(SENTINEL));
}

#[test]
fn process_env_and_dotenv_secrets_also_reach_engine_detection() {
    const LEGACY_SECRET: &str = "from-legacy-dotenv-migrate-7391";
    let sandbox = sandbox_with(SECRET_WORKFLOW);
    sandbox.write(".litci/secrets", &format!("API_TOKEN={LEGACY_SECRET}\n"));
    let output = sandbox.run_with_env(&["run", "-W", "wf.yml"], &[SSH_DOCKER_HOST]);
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("DOCKER_HOST"), "{stderr}");
    assert!(!stderr.contains(LEGACY_SECRET), "{stderr}");

    let legacy = sandbox.root().join(".litci/secrets");
    let vault = sandbox.root().join(".litci/secrets.vault");
    let key = sandbox.home().join(".litci/vault.key");
    assert!(!legacy.exists(), "plaintext legacy file survived migration");
    let ciphertext = std::fs::read(&vault).expect("read encrypted vault");
    assert!(ciphertext.starts_with(b"greenlit-vault-v1\0"));
    assert!(
        !ciphertext
            .windows(LEGACY_SECRET.len())
            .any(|window| window == LEGACY_SECRET.as_bytes()),
        "vault contains the secret in plaintext"
    );
    assert_eq!(
        std::fs::metadata(&vault)
            .expect("stat encrypted vault")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(&key)
            .expect("stat vault key")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    let second = sandbox.run_with_env(&["run", "-W", "wf.yml"], &[SSH_DOCKER_HOST]);
    let second_stderr = support::stderr_text(&second);
    assert!(second_stderr.contains("DOCKER_HOST"), "{second_stderr}");
    assert!(!second_stderr.contains(LEGACY_SECRET), "{second_stderr}");
}

#[test]
fn no_input_fails_fast_listing_the_missing_secret_name_before_engine_detection() {
    let sandbox = sandbox_with(SECRET_WORKFLOW);
    let output = sandbox.run_with_env(&["run", "-W", "wf.yml", "--no-input"], &[SSH_DOCKER_HOST]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("API_TOKEN"), "{stderr}");
    assert!(stderr.contains("fix:"), "{stderr}");
    assert!(
        !stderr.contains("DOCKER_HOST"),
        "must fail before engine detection: {stderr}"
    );
}

#[test]
fn github_token_secret_never_blocks_a_no_input_run_the_way_an_ordinary_secret_does() {
    // Unlike an ordinary secret, `secrets.GITHUB_TOKEN` never fails a
    // `--no-input` run just because no local override/auth exists — it
    // resolves to the empty string instead
    // (`crate::auth::resolve_github_token`), so the run still reaches
    // engine detection.
    let sandbox = sandbox_with(GITHUB_TOKEN_WORKFLOW);
    let output = sandbox.run_with_env(&["run", "-W", "wf.yml", "--no-input"], &[SSH_DOCKER_HOST]);
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("DOCKER_HOST"), "{stderr}");
    assert!(
        !stderr.contains("could not be prompted for"),
        "must not be treated as an ordinary missing secret: {stderr}"
    );
}

#[test]
fn github_token_resolves_from_the_stored_auth_token_without_leaking_it() {
    let sandbox = sandbox_with(GITHUB_TOKEN_WORKFLOW);
    sandbox.seed_auth_token("ghu_should_never_appear_in_output");
    let output = sandbox.run_with_env(&["run", "-W", "wf.yml", "--no-input"], &[SSH_DOCKER_HOST]);
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("DOCKER_HOST"), "{stderr}");
    assert!(
        !stderr.contains("ghu_should_never_appear_in_output"),
        "{stderr}"
    );
    // A real token was available and the workflow references it, so no
    // "no local token configured" notice should fire.
    assert!(!stderr.contains("no local token is configured"), "{stderr}");
    assert!(!support::stdout_text(&output).contains("ghu_should_never_appear_in_output"));
}

#[test]
fn a_local_override_for_github_token_wins_over_the_stored_auth_token() {
    let sandbox = sandbox_with(GITHUB_TOKEN_WORKFLOW);
    sandbox.seed_auth_token("ghu_stored_token_must_not_be_used");
    let output = sandbox.run_with_env(
        &[
            "run",
            "-W",
            "wf.yml",
            "-s",
            "GITHUB_TOKEN=locally-overridden-token",
        ],
        &[SSH_DOCKER_HOST],
    );
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("DOCKER_HOST"), "{stderr}");
    assert!(!stderr.contains("locally-overridden-token"), "{stderr}");
    assert!(
        !stderr.contains("ghu_stored_token_must_not_be_used"),
        "{stderr}"
    );
}

#[test]
fn a_permissions_block_exceeding_the_local_token_grant_prints_one_notice() {
    // `PHASE-3-actions.md` Auth: "when a workflow requests more than the
    // token grants or more than `contents: read`, print one actionable
    // notice naming the difference." `contents: write` exceeds it.
    let sandbox = sandbox_with(GITHUB_TOKEN_WORKFLOW_WITH_CONTENTS_WRITE);
    sandbox.seed_auth_token("ghu_permission_notice_probe");
    let output = sandbox.run_with_env(&["run", "-W", "wf.yml", "--no-input"], &[SSH_DOCKER_HOST]);
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("DOCKER_HOST"), "{stderr}");
    assert!(stderr.contains("cannot narrow"), "{stderr}");
    assert!(stderr.contains("write access"), "{stderr}");
    assert!(!stderr.contains("ghu_permission_notice_probe"), "{stderr}");
}

#[test]
fn a_permissions_block_never_notices_when_the_token_is_not_even_used() {
    // The same excessive `permissions:` block on a workflow that never
    // references the token at all must stay silent — the notice only
    // matters for a run that actually injects one.
    let sandbox = sandbox_with(
        "on: push\npermissions:\n  contents: write\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
    );
    let output = sandbox.run_with_env(&["run", "-W", "wf.yml"], &[SSH_DOCKER_HOST]);
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("DOCKER_HOST"), "{stderr}");
    assert!(!stderr.contains("cannot narrow"), "{stderr}");
}
