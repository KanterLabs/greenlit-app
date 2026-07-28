use super::support::{Sandbox, stderr_text, stdout_text};

const DIAGNOSTIC: &str = "`litci export` and `litci confirm` are disabled until Phase 27 certifies verified consumers; retry after Phase 27 is complete\n";

#[test]
fn export_and_confirmation_are_hard_disabled_without_filesystem_or_auth_state_changes() {
    let sandbox = Sandbox::new();

    let exported = sandbox.run(&["export", "missing-run", "--output", "blocked-export"]);
    assert_eq!(exported.status.code(), Some(1));
    assert_eq!(stdout_text(&exported), "");
    assert_eq!(stderr_text(&exported), DIAGNOSTIC);
    assert!(!sandbox.root().join("blocked-export").exists());
    assert!(directory_is_empty(sandbox.home()));

    let confirmed = sandbox.run(&[
        "confirm",
        "missing-run",
        "--repository",
        "owner/repo",
        "--github-run",
        "42",
    ]);

    assert_eq!(confirmed.status.code(), Some(1));
    assert_eq!(stdout_text(&confirmed), "");
    assert_eq!(stderr_text(&confirmed), DIAGNOSTIC);
    assert!(directory_is_empty(sandbox.root()));
    assert!(directory_is_empty(sandbox.home()));
}

fn directory_is_empty(path: &std::path::Path) -> bool {
    std::fs::read_dir(path)
        .expect("read sandbox directory")
        .next()
        .is_none()
}
