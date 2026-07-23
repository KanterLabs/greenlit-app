//! Repository dotenv parsing, bounds, literal values, and path safety.

use std::os::unix::fs::symlink;

use super::common::*;
use super::support;
use super::support::Sandbox;

#[test]
fn dotenv_file_resolution_distinguishes_values_from_io_failures() {
    let sandbox = Sandbox::new();
    sandbox.write("wf.yml", LITERAL_VAR_WORKFLOW);
    sandbox.write(".litci/vars", "MODE=ci\n");
    sandbox.init_git();

    let output = sandbox.run(&["plan", "-W", "wf.yml"]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    let stdout = support::stdout_text(&output);
    assert!(condition_line(&stdout).contains("static(true)"));

    // `.litci/vars` is repository-controlled input. Greenlit accepts exactly
    // one MiB, indexes many assignments without repeated source scans, and
    // rejects the first byte over the boundary without echoing file content.
    const MAX_DOTENV_FILE_BYTES: usize = 1024 * 1024;
    const FILE_SENTINEL: &str = "dotenv-file-content-must-not-reach-diagnostics-7391";
    let bounded = Sandbox::new();
    bounded.write("wf.yml", LITERAL_VAR_WORKFLOW);
    bounded.init_git();
    let mut exact = format!("MODE=ci\n# {FILE_SENTINEL}\n");
    exact.push('#');
    exact.push_str(&"x".repeat(MAX_DOTENV_FILE_BYTES - exact.len()));
    assert_eq!(exact.len(), MAX_DOTENV_FILE_BYTES);
    bounded.write(".litci/vars", &exact);

    let output = bounded.run(&["plan", "-W", "wf.yml"]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    assert!(condition_line(&support::stdout_text(&output)).contains("static(true)"));

    exact.push('x');
    assert_eq!(exact.len(), MAX_DOTENV_FILE_BYTES + 1);
    bounded.write(".litci/vars", &exact);
    let output = bounded.run(&["plan", "-W", "wf.yml"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(
        stderr.contains("exceeds Greenlit's 1 MiB safety limit"),
        "{stderr}"
    );
    assert!(stderr.contains(".litci/vars"), "{stderr}");
    assert!(stderr.contains("fix:"), "{stderr}");
    assert!(!stderr.contains(FILE_SENTINEL), "{stderr}");
    let metrics = std::fs::read_to_string(bounded.metrics_file()).expect("read bounded metrics");
    assert!(!metrics.contains(FILE_SENTINEL), "{metrics}");

    const MAX_DOTENV_ASSIGNMENTS: usize = 2_000;
    const ENTRY_SENTINEL: &str = "dotenv-entry-must-not-reach-diagnostics-7391";
    let mut assignments = String::from("MODE=ci\n");
    for index in 1..MAX_DOTENV_ASSIGNMENTS {
        assignments.push_str(&format!("LOCAL_{index}=x\n"));
    }
    bounded.write(".litci/vars", &assignments);
    let output = bounded.run(&["plan", "-W", "wf.yml"]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));

    assignments.push_str(&format!("OVER_LIMIT={ENTRY_SENTINEL}\n"));
    bounded.write(".litci/vars", &assignments);
    let output = bounded.run(&["plan", "-W", "wf.yml"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(
        stderr.contains("exceeds Greenlit's 2,000-assignment safety limit"),
        "{stderr}"
    );
    assert!(stderr.contains("fix:"), "{stderr}");
    assert!(!stderr.contains(ENTRY_SENTINEL), "{stderr}");

    let invalid_path = Sandbox::new();
    invalid_path.write("wf.yml", LITERAL_VAR_WORKFLOW);
    invalid_path.write(".litci/vars/placeholder", "");
    invalid_path.init_git();

    let output = invalid_path.run(&["plan", "-W", "wf.yml"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(
        stderr.contains("could not read local variables file"),
        "{stderr}"
    );
    assert!(stderr.contains("readable regular file"), "{stderr}");
    assert!(!stderr.contains("fix the KEY=VALUE syntax"), "{stderr}");

    let invalid_encoding = Sandbox::new();
    invalid_encoding.write("wf.yml", LITERAL_VAR_WORKFLOW);
    invalid_encoding.write_bytes(".litci/vars", b"MODE=\xff\n");
    invalid_encoding.init_git();

    let output = invalid_encoding.run(&["plan", "-W", "wf.yml"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("not valid UTF-8"), "{stderr}");
    assert!(stderr.contains("save "), "{stderr}");
    assert!(stderr.contains(" as UTF-8"), "{stderr}");
}

#[test]
fn dotenv_values_are_literal_and_repository_symlinks_cannot_read_host_files() {
    const HOST_ENV: &str = "GREENLIT_HOST_SECRET_AUDIT_7391";
    const SENTINEL: &str = "host-secret-must-never-escape-7391";

    let literal = Sandbox::new();
    literal.write("wf.yml", DISPLAY_VAR_WORKFLOW);
    literal.write(".litci/vars", &format!("LEAK=${HOST_ENV}\n"));
    literal.init_git();

    let output = literal.run_with_env(&["plan", "-W", "wf.yml"], &[(HOST_ENV, SENTINEL)]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    let stdout = support::stdout_text(&output);
    let stderr = support::stderr_text(&output);
    assert!(stdout.contains(&format!("${HOST_ENV}")), "{stdout}");
    assert!(!stdout.contains(SENTINEL), "{stdout}");
    assert!(!stderr.contains(SENTINEL), "{stderr}");
    let metrics = std::fs::read_to_string(literal.metrics_file()).expect("read literal metrics");
    assert!(!metrics.contains(SENTINEL), "{metrics}");

    let compatible = Sandbox::new();
    compatible.write("wf.yml", DISPLAY_VAR_WORKFLOW);
    compatible.write(
        ".litci/vars",
        "  # local values\nexport LEAK = \"quoted value\" # trailing comment\n",
    );
    compatible.init_git();

    let output = compatible.run(&["plan", "-W", "wf.yml"]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    let stdout = support::stdout_text(&output);
    assert!(stdout.contains("quoted value"), "{stdout}");
    assert!(!stdout.contains("trailing comment"), "{stdout}");

    let malformed = Sandbox::new();
    malformed.write("wf.yml", DISPLAY_VAR_WORKFLOW);
    malformed.write(
        ".litci/vars",
        &format!("LEAK=local\nBROKEN LINE {SENTINEL}\n"),
    );
    malformed.init_git();

    let output = malformed.run(&["plan", "-W", "wf.yml"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("KEY=VALUE"), "{stderr}");
    assert!(!stderr.contains(SENTINEL), "{stderr}");
    assert!(!support::stdout_text(&output).contains(SENTINEL));
    let metrics =
        std::fs::read_to_string(malformed.metrics_file()).expect("read malformed metrics");
    assert!(!metrics.contains(SENTINEL), "{metrics}");

    let hostile_name = Sandbox::new();
    hostile_name.write("wf.yml", DISPLAY_VAR_WORKFLOW);
    hostile_name.write(".litci/vars", "BAD\t\u{202e}=redacted-value\n");
    hostile_name.init_git();
    let output = hostile_name.run(&["plan", "-W", "wf.yml"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains(r"BAD\t\u{202e}"), "{stderr}");
    assert!(!stderr.contains('\t'));
    assert!(!stderr.contains('\u{202e}'));
    assert!(!stderr.contains("redacted-value"));

    let external = tempfile::tempdir().expect("external host directory");
    let external_vars = external.path().join("host-vars");
    std::fs::write(&external_vars, format!("LEAK={SENTINEL}\n")).expect("write host vars");
    let linked_file = Sandbox::new();
    let workflow_path = linked_file.write("wf.yml", DISPLAY_VAR_WORKFLOW);
    let litci_dir = workflow_path
        .parent()
        .expect("workflow parent")
        .join(".litci");
    std::fs::create_dir(&litci_dir).expect("create .litci directory");
    symlink(&external_vars, litci_dir.join("vars")).expect("link vars to host file");
    linked_file.init_git();

    let output = linked_file.run(&["plan", "-W", "wf.yml"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("symbolic link"), "{stderr}");
    assert!(stderr.contains("fix:"), "{stderr}");
    assert!(!stderr.contains(SENTINEL), "{stderr}");
    assert!(!support::stdout_text(&output).contains(SENTINEL));

    let external_litci = tempfile::tempdir().expect("external .litci directory");
    std::fs::write(
        external_litci.path().join("vars"),
        format!("LEAK={SENTINEL}\n"),
    )
    .expect("write external vars");
    let linked_directory = Sandbox::new();
    let workflow_path = linked_directory.write("wf.yml", DISPLAY_VAR_WORKFLOW);
    let repo_root = workflow_path.parent().expect("workflow parent");
    symlink(external_litci.path(), repo_root.join(".litci")).expect("link .litci to host dir");
    linked_directory.init_git();

    let output = linked_directory.run(&["plan", "-W", "wf.yml"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("symbolic link"), "{stderr}");
    assert!(stderr.contains("fix:"), "{stderr}");
    assert!(!stderr.contains(SENTINEL), "{stderr}");
    assert!(!support::stdout_text(&output).contains(SENTINEL));
}
