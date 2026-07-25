use std::fs;
use std::process::Command;

use greenlit_engine::SourceSnapshot;
use tempfile::TempDir;

fn git(repo: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .expect("test Git command should start");
    assert!(status.success());
}

#[test]
fn source_snapshot_is_stable_and_excludes_ignored_content() {
    let temp = TempDir::new().expect("temp directory should be created");
    let repo = temp.path().join("repo");
    fs::create_dir(&repo).expect("repository directory should be created");
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.name", "Greenlit Test"]);
    git(&repo, &["config", "user.email", "greenlit@example.invalid"]);
    fs::write(repo.join(".gitignore"), "target/\n").expect("gitignore should be written");
    fs::write(repo.join("tracked.txt"), "current\n").expect("tracked file should be written");
    git(&repo, &["add", ".gitignore", "tracked.txt"]);
    git(&repo, &["commit", "-qm", "fixture"]);
    fs::write(repo.join("untracked.txt"), "included\n").expect("untracked file should be written");
    fs::create_dir(repo.join("target")).expect("ignored directory should be created");
    fs::write(repo.join("target/huge.bin"), vec![0_u8; 1024 * 1024])
        .expect("ignored file should be written");

    let first = SourceSnapshot::capture(&repo, &temp.path().join("first"))
        .expect("first snapshot should succeed");
    let second = SourceSnapshot::capture(&repo, &temp.path().join("second"))
        .expect("second snapshot should succeed");

    assert_eq!(first.digest, second.digest);
    assert!(first.dirty);
    assert!(first.root.join("tracked.txt").is_file());
    assert!(first.root.join("untracked.txt").is_file());
    assert!(!first.root.join("target").exists());
    assert!(first.root.join(".git").is_dir());
    assert!(
        first
            .entries
            .iter()
            .all(|entry| !entry.path.starts_with("target/"))
    );
}

#[test]
fn tracked_deletion_is_preserved_in_snapshot() {
    let temp = TempDir::new().expect("temp directory should be created");
    let repo = temp.path().join("repo");
    fs::create_dir(&repo).expect("repository directory should be created");
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.name", "Greenlit Test"]);
    git(&repo, &["config", "user.email", "greenlit@example.invalid"]);
    fs::write(repo.join("deleted.txt"), "before\n").expect("tracked file should be written");
    git(&repo, &["add", "deleted.txt"]);
    git(&repo, &["commit", "-qm", "fixture"]);
    fs::remove_file(repo.join("deleted.txt")).expect("tracked file should be deleted");

    let snapshot = SourceSnapshot::capture(&repo, &temp.path().join("snapshot"))
        .expect("snapshot should succeed");
    assert!(!snapshot.root.join("deleted.txt").exists());
    assert!(
        snapshot
            .entries
            .iter()
            .all(|entry| entry.path != "deleted.txt")
    );
}
