//! Workflow discovery and bounded, non-fetching local Git integration.

use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Command;

use super::common::*;
use super::support;
use super::support::Sandbox;

fn git_command(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn test git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_command_with_input(cwd: &Path, args: &[&str], input: &[u8]) -> Vec<u8> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn test git");
    child
        .stdin
        .take()
        .expect("test git stdin")
        .write_all(input)
        .expect("write test git stdin");
    let output = child.wait_with_output().expect("wait for test git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn trimmed_git_output(cwd: &Path, args: &[&str], input: &[u8]) -> String {
    String::from_utf8(git_command_with_input(cwd, args, input))
        .expect("test git output is UTF-8")
        .trim()
        .to_string()
}

#[test]
fn workflow_is_discovered_when_exactly_one_exists_under_github_workflows() {
    let sandbox = Sandbox::new();
    let workflow = workflow_with_trigger("  push:\n");
    sandbox.write(".github/workflows/ci.yml", &workflow);
    sandbox.init_git();

    let output = sandbox.run(&["plan"]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    assert!(support::stdout_text(&output).contains("event: push"));

    let external = tempfile::tempdir().expect("external workflow directory");
    let external_workflow = external.path().join("outside.yml");
    std::fs::write(&external_workflow, &workflow).expect("write outside workflow");
    let linked = Sandbox::new();
    let placeholder = linked.write(".github/workflows/.keep", "");
    std::fs::remove_file(&placeholder).expect("remove workflow placeholder");
    symlink(
        &external_workflow,
        placeholder
            .parent()
            .expect("workflows directory")
            .join("ci.yml"),
    )
    .expect("link outside workflow");
    linked.init_git();

    let output = linked.run(&["plan"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("resolves outside repository"), "{stderr}");
    assert!(stderr.contains("fix:"), "{stderr}");
}

#[test]
fn planning_a_partial_clone_never_lazy_fetches_missing_git_objects() {
    let source = tempfile::tempdir().expect("source repository");
    git_command(source.path(), &["init", "-q", "-b", "main"]);
    git_command(
        source.path(),
        &["config", "user.email", "litci-tests@example.com"],
    );
    git_command(source.path(), &["config", "user.name", "litci tests"]);
    std::fs::write(source.path().join("README.md"), "first\n").expect("write first commit");
    git_command(source.path(), &["add", "."]);
    git_command(source.path(), &["commit", "-q", "-m", "first"]);
    std::fs::create_dir_all(source.path().join(".github/workflows"))
        .expect("create workflow directory");
    std::fs::write(
        source.path().join(".github/workflows/ci.yml"),
        "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
    )
    .expect("write workflow");
    git_command(source.path(), &["add", "."]);
    git_command(source.path(), &["commit", "-q", "-m", "workflow"]);

    let origin = tempfile::tempdir().expect("bare origin parent");
    let origin_path = origin.path().join("origin.git");
    let origin_text = origin_path.to_string_lossy().into_owned();
    git_command(
        origin.path(),
        &["init", "-q", "--bare", "-b", "main", &origin_text],
    );
    git_command(source.path(), &["remote", "add", "origin", &origin_text]);
    git_command(source.path(), &["push", "-q", "origin", "main"]);
    git_command(&origin_path, &["config", "uploadpack.allowFilter", "true"]);

    let clone = Sandbox::new();
    git_command(
        clone.root(),
        &[
            "clone",
            "-q",
            "--filter=tree:0",
            "--no-local",
            &origin_text,
            ".",
        ],
    );
    let missing_parent_tree = Command::new("git")
        .args(["cat-file", "-e", "HEAD~1^{tree}"])
        .current_dir(clone.root())
        .env("GIT_NO_LAZY_FETCH", "1")
        .output()
        .expect("probe missing parent tree");
    assert!(
        !missing_parent_tree.status.success(),
        "filtered clone unexpectedly contains the parent tree"
    );

    let upload_pack = origin.path().join("upload-pack-sentinel.sh");
    let marker = origin.path().join("upload-pack-sentinel.sh.invoked");
    std::fs::write(&upload_pack, "#!/bin/sh\ntouch \"$0.invoked\"\nexit 97\n")
        .expect("write upload-pack sentinel");
    let mut permissions = std::fs::metadata(&upload_pack)
        .expect("sentinel metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&upload_pack, permissions).expect("make sentinel executable");
    let upload_pack_text = upload_pack.to_string_lossy().into_owned();
    clone.git(&["config", "remote.origin.uploadpack", &upload_pack_text]);

    let output = clone.run(&["plan", "-W", ".github/workflows/ci.yml"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("missing Git objects"), "{stderr}");
    assert!(
        stderr.contains("fetch the missing Git objects explicitly"),
        "{stderr}"
    );
    assert!(!marker.exists(), "planning contacted the promisor remote");
}

#[test]
fn repository_git_metadata_stdout_is_bounded() {
    let sandbox = Sandbox::new();
    sandbox.write("wf.yml", PATH_FILTER_WORKFLOW);
    sandbox.init_git();
    let oversized_actor = "a".repeat(70 * 1024);
    sandbox.git(&["config", "user.name", &oversized_actor]);

    let output = sandbox.run(&["plan", "-W", "wf.yml"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("Git stdout"), "{stderr}");
    assert!(stderr.contains("65536-byte safety limit"), "{stderr}");
    assert!(
        stderr.contains("shorten the oversized local Git metadata"),
        "{stderr}"
    );
}

#[test]
fn repository_changed_path_records_are_bounded_before_materialization() {
    let sandbox = Sandbox::new();
    sandbox.write("wf.yml", PATH_FILTER_WORKFLOW);
    sandbox.init_git();

    let mut tree = git_command_with_input(sandbox.root(), &["ls-tree", "-z", "HEAD"], &[]);
    let blob = trimmed_git_output(sandbox.root(), &["hash-object", "-w", "--stdin"], b"x");
    tree.extend_from_slice(format!("100644 blob {blob}\t").as_bytes());
    tree.extend(std::iter::repeat_n(b'x', 70 * 1024));
    tree.push(0);
    let tree_id = trimmed_git_output(sandbox.root(), &["mktree", "-z"], &tree);
    let parent = trimmed_git_output(sandbox.root(), &["rev-parse", "HEAD"], &[]);
    let commit = trimmed_git_output(
        sandbox.root(),
        &["commit-tree", &tree_id, "-p", &parent],
        b"overlong path\n",
    );
    git_command(sandbox.root(), &["update-ref", "HEAD", &commit]);

    let output = sandbox.run(&["plan", "-W", "wf.yml"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("a changed path"), "{stderr}");
    assert!(stderr.contains("65536-byte safety limit"), "{stderr}");
    assert!(stderr.contains("rename the changed path"), "{stderr}");
}

#[test]
fn repository_git_processes_have_a_deterministic_deadline() {
    let sandbox = Sandbox::new();
    sandbox.write("wf.yml", PATH_FILTER_WORKFLOW);
    sandbox.init_git();
    let head = sandbox.root().join(".git/HEAD");
    std::fs::remove_file(&head).expect("remove ordinary HEAD file");
    let fifo = Command::new("mkfifo")
        .arg(&head)
        .status()
        .expect("spawn mkfifo");
    assert!(fifo.success(), "mkfifo failed");

    let started = std::time::Instant::now();
    let output = sandbox.run(&["plan", "-W", "wf.yml"]);
    let elapsed = started.elapsed();
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(
        stderr.contains("5-second local command deadline"),
        "{stderr}"
    );
    assert!(stderr.contains("was stopped"), "{stderr}");
    assert!(stderr.contains("repair the local repository"), "{stderr}");
    assert!(
        elapsed < std::time::Duration::from_secs(15),
        "deadline took {elapsed:?}"
    );
}
