//! `litci clean`: what it reclaims, what it refuses to touch, and what it
//! does when nothing is there.
//!
//! The behavior that matters is not "files got deleted" but *which* files:
//! everything this removes is derived and cheap to rebuild, while credentials
//! and the user's own run history are not. Deleting either of those would be
//! a silent, unrecoverable loss, so both are asserted explicitly.
//!
//! Every case points `DOCKER_HOST` at a transport detection rejects. `clean`
//! removes *images* as well as directories, and a test that reached the
//! developer's real daemon would delete their real Greenlit images as a side
//! effect — which is exactly what happened before this was added, and which
//! also made the cases race each other. Pointing at a dead socket keeps the
//! filesystem behavior under test and exercises the documented degradation
//! path (caches still reclaimed, images reported as skipped) at the same
//! time.

pub mod support;

use support::Sandbox;

/// A `DOCKER_HOST` transport detection *rejects outright*, so no test ever
/// reaches a daemon.
///
/// An unreachable socket path is not enough: detection falls through to the
/// real Docker socket when `DOCKER_HOST` simply fails to answer, which would
/// put the developer's own images in range of a `clean --yes`. A rejected
/// transport short-circuits before any probe.
const NO_DAEMON: (&str, &str) = ("DOCKER_HOST", "ssh://greenlit-tests-never-connect");

/// Seeds every derived store plus the two things a clean must never remove.
fn seed_litci_home(sandbox: &Sandbox) {
    for (path, body) in [
        (".litci/cache/entries/1/blob", "cached"),
        (".litci/artifacts/1/blob", "artifact"),
        (".litci/toolcache/node/20.0.0/x64/bin/node", "toolchain"),
        (".litci/actions/actions/checkout/abc/action.yml", "name: x"),
        (".litci/node-runtimes/node20/standard/sha/bin/node", "rt"),
        // Not derived: must survive.
        (".litci/auth.json", "{\"token\":\"secret-value\"}"),
        (".litci/metrics/runs.ndjson", "{\"schema_version\":1}\n"),
    ] {
        sandbox.write_home(path, body);
    }
}

#[test]
fn clean_removes_every_derived_store() {
    let sandbox = Sandbox::new();
    seed_litci_home(&sandbox);

    let output = sandbox.run_with_env(&["clean", "--yes"], &[NO_DAEMON]);
    assert!(
        output.status.success(),
        "clean failed: {}",
        support::stderr_text(&output)
    );

    let home = sandbox.home();
    for derived in [
        ".litci/cache",
        ".litci/artifacts",
        ".litci/toolcache",
        ".litci/actions",
        ".litci/node-runtimes",
    ] {
        assert!(
            !home.join(derived).exists(),
            "{derived} is derived and should have been reclaimed"
        );
    }
}

#[test]
fn clean_never_touches_credentials_or_run_history() {
    let sandbox = Sandbox::new();
    seed_litci_home(&sandbox);

    let output = sandbox.run_with_env(&["clean", "--yes"], &[NO_DAEMON]);
    assert!(output.status.success());

    let home = sandbox.home();
    // Credentials are not a cache: removing them would sign the user out of
    // something they never asked to lose.
    assert_eq!(
        std::fs::read_to_string(home.join(".litci/auth.json")).expect("auth.json survives"),
        "{\"token\":\"secret-value\"}"
    );
    // The invocation history is derived from nothing and is what `litci stats`
    // trends over.
    assert_eq!(
        std::fs::read_to_string(home.join(".litci/metrics/runs.ndjson")).expect("metrics survive"),
        "{\"schema_version\":1}\n"
    );
}

#[test]
fn clean_reports_what_it_will_remove_before_removing_it() {
    let sandbox = Sandbox::new();
    seed_litci_home(&sandbox);

    let output = sandbox.run_with_env(&["clean", "--yes"], &[NO_DAEMON]);
    let stdout = support::stdout_text(&output);

    assert!(
        stdout.contains("This will remove:"),
        "the user sees the list before it happens: {stdout}"
    );
    assert!(
        stdout.contains("Credentials and run history are not touched."),
        "and is told what is safe: {stdout}"
    );
    assert!(
        stdout.contains("Reclaimed"),
        "and how much came back: {stdout}"
    );
}

#[test]
fn declining_the_prompt_removes_nothing() {
    let sandbox = Sandbox::new();
    seed_litci_home(&sandbox);

    // No `--yes`, and stdin answers "n".
    let output = sandbox.run_with_stdin(&["clean"], &[NO_DAEMON], "n\n");
    assert!(output.status.success(), "declining is not a failure");
    assert!(
        support::stdout_text(&output).contains("No changes made."),
        "the refusal is stated plainly"
    );
    assert!(
        sandbox.home().join(".litci/cache").exists(),
        "nothing was removed"
    );
}

#[test]
fn an_empty_store_is_not_an_error() {
    let sandbox = Sandbox::new();

    let output = sandbox.run_with_env(&["clean", "--yes"], &[NO_DAEMON]);
    assert!(output.status.success());
    assert!(
        support::stdout_text(&output).contains("Nothing to clean"),
        "a fresh machine says so rather than reporting an empty removal"
    );
}

#[test]
fn an_unreachable_daemon_still_reclaims_the_caches() {
    let sandbox = Sandbox::new();
    seed_litci_home(&sandbox);

    let output = sandbox.run_with_env(&["clean", "--yes"], &[NO_DAEMON]);
    assert!(output.status.success(), "an absent daemon is not a failure");

    let stdout = support::stdout_text(&output);
    assert!(
        stdout.contains("images could not be listed"),
        "the user is told which half was skipped and why: {stdout}"
    );
    assert!(
        !sandbox.home().join(".litci/cache").exists(),
        "the on-disk half is reclaimed regardless of the daemon"
    );
}
