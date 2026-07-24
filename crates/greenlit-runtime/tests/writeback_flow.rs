//! The `--write-back` orchestration end to end, driven through a fake engine —
//! no Docker daemon.
//!
//! `PHASE-2-execution.md` exit criterion 3: "`--write-back` lists and applies
//! only the confirmed overlay diff; cancellation leaves the host unchanged."
//! The engine is the true external here (its `export_path` is the overlay-diff
//! boundary), so it is faked per `TESTING.md`; the confirmation gate is the
//! other injected boundary. Everything the host process does with the diff runs
//! real.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tar::{Builder, EntryType, Header};

use greenlit_runtime::engine::{
    BuildSpec, CommitSpec, ContainerEngine, ContainerSpec, ContainerState, ExecOutput,
    ExecOutputSink, ExecSpec, HealthState, ImageSummary, NetworkInfo, RegistryAuth,
};
use greenlit_runtime::error::RuntimeError;
use greenlit_runtime::progress::ProgressSink;
use greenlit_runtime::writeback::{Change, ChangeKind, Confirm, WriteBackOutcome, run_write_back};

/// A fake engine whose `export_path` replays a fixed overlay-upper tar.
struct DiffEngine {
    upper_tar: Vec<u8>,
}

#[async_trait]
impl ContainerEngine for DiffEngine {
    async fn pull_image(
        &self,
        _image: &str,
        _auth: Option<&RegistryAuth>,
        _progress: &mut (dyn ProgressSink + Send),
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn image_exists(&self, _image: &str) -> Result<bool, RuntimeError> {
        Ok(true)
    }
    async fn build_image(
        &self,
        _spec: &BuildSpec,
        _progress: &mut (dyn ProgressSink + Send),
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn commit_container(&self, _spec: &CommitSpec) -> Result<String, RuntimeError> {
        Ok(String::new())
    }
    async fn create_container(&self, _spec: &ContainerSpec) -> Result<String, RuntimeError> {
        Ok("c".to_string())
    }
    async fn start_container(&self, _id: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn stop_container(&self, _id: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn remove_container(&self, _id: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn exec(
        &self,
        _container: &str,
        _spec: &ExecSpec,
        _sink: &mut (dyn ExecOutputSink + Send),
    ) -> Result<ExecOutput, RuntimeError> {
        Ok(ExecOutput { exit_code: 0 })
    }
    async fn run_container(
        &self,
        _id: &str,
        _sink: &mut (dyn ExecOutputSink + Send),
    ) -> Result<ExecOutput, RuntimeError> {
        Ok(ExecOutput { exit_code: 0 })
    }
    async fn export_path(&self, _container: &str, _path: &str) -> Result<Vec<u8>, RuntimeError> {
        Ok(self.upper_tar.clone())
    }
    async fn create_network(&self, _name: &str) -> Result<String, RuntimeError> {
        Ok(String::new())
    }
    async fn remove_network(&self, _name: &str) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn inspect_network(&self, _name: &str) -> Result<NetworkInfo, RuntimeError> {
        Ok(NetworkInfo {
            gateway: Some("10.0.0.1".to_string()),
        })
    }
    // Write-back never inspects health, touches volumes, or manages images;
    // these exist so the fake still implements the whole port.
    async fn inspect_container(&self, _id: &str) -> Result<ContainerState, RuntimeError> {
        Ok(ContainerState {
            running: true,
            exit_code: None,
            health: HealthState::None,
        })
    }
    async fn create_volume(&self, _name: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn remove_volume(&self, _name: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn list_images(&self, _label: &str) -> Result<Vec<ImageSummary>, RuntimeError> {
        Ok(Vec::new())
    }
    async fn remove_image(&self, _image: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
}

/// A scripted confirmation gate.
struct FixedConfirm {
    answer: bool,
    seen: Vec<Change>,
}

impl Confirm for FixedConfirm {
    fn confirm(&mut self, changes: &[Change]) -> bool {
        self.seen = changes.to_vec();
        self.answer
    }
}

/// Build an overlay-upper tar (rooted at `upper/`) with one added file and one
/// whiteout deletion — the shape Docker returns for the exported upper.
fn upper_with_add_and_delete() -> Vec<u8> {
    let mut builder = Builder::new(Vec::new());

    let mut file = Header::new_gnu();
    file.set_entry_type(EntryType::Regular);
    file.set_mode(0o644);
    file.set_size(5);
    file.set_cksum();
    builder
        .append_data(&mut file, "upper/added.txt", &b"fresh"[..])
        .expect("append add");

    let mut whiteout = Header::new_gnu();
    whiteout.set_entry_type(EntryType::Char);
    whiteout.set_mode(0o644);
    whiteout.set_size(0);
    whiteout.set_cksum();
    builder
        .append_data(&mut whiteout, "upper/removed.txt", std::io::empty())
        .expect("append whiteout");

    builder.into_inner().expect("finish tar")
}

/// A unique throwaway working tree seeded with `removed.txt`.
fn seeded_worktree(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "greenlit-wb-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("mk worktree");
    std::fs::write(root.join("removed.txt"), b"present").expect("seed removed");
    root
}

#[tokio::test]
async fn confirmed_write_back_applies_only_the_listed_diff() {
    let engine = DiffEngine {
        upper_tar: upper_with_add_and_delete(),
    };
    let root = seeded_worktree("apply");
    let mut confirm = FixedConfirm {
        answer: true,
        seen: Vec::new(),
    };

    let outcome = run_write_back(&engine, "c", &root, &mut confirm)
        .await
        .expect("write-back");

    // The listing the user confirmed names exactly the add and the delete.
    assert!(confirm.seen.contains(&Change {
        path: PathBuf::from("added.txt"),
        kind: ChangeKind::AddedOrModified,
    }));
    assert!(confirm.seen.contains(&Change {
        path: PathBuf::from("removed.txt"),
        kind: ChangeKind::Deleted,
    }));

    assert!(matches!(outcome, WriteBackOutcome::Applied(_)));
    assert_eq!(
        std::fs::read(root.join("added.txt")).expect("added applied"),
        b"fresh"
    );
    assert!(
        !root.join("removed.txt").exists(),
        "the whiteout deleted the seeded file"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn cancelled_write_back_leaves_the_host_untouched() {
    let engine = DiffEngine {
        upper_tar: upper_with_add_and_delete(),
    };
    let root = seeded_worktree("cancel");
    let before = tree_fingerprint(&root);
    let mut confirm = FixedConfirm {
        answer: false,
        seen: Vec::new(),
    };

    let outcome = run_write_back(&engine, "c", &root, &mut confirm)
        .await
        .expect("write-back");

    assert_eq!(outcome, WriteBackOutcome::Cancelled);
    // Nothing added, nothing deleted: the tree is byte-for-byte what it was.
    assert!(!root.join("added.txt").exists(), "cancel wrote nothing");
    assert!(root.join("removed.txt").exists(), "cancel deleted nothing");
    assert_eq!(before, tree_fingerprint(&root), "host tree unchanged");

    let _ = std::fs::remove_dir_all(&root);
}

/// Finding: write-back symlink traversal. The repository (the write-back
/// host root) already contains a symlink pointing *outside* it; the
/// workflow's diff replaces that same workspace path with a regular file.
/// Applying it must never open through the pre-existing symlink — the
/// outside target must be untouched, and the workspace path must end up as
/// the new regular file, not silently still a symlink.
#[tokio::test]
async fn write_back_replacing_a_pre_existing_symlink_never_writes_outside_the_repo() {
    let mut builder = Builder::new(Vec::new());
    let mut file = Header::new_gnu();
    file.set_entry_type(EntryType::Regular);
    file.set_mode(0o644);
    file.set_size(11);
    file.set_cksum();
    builder
        .append_data(&mut file, "upper/victim", &b"replacement"[..])
        .expect("append replacement");
    let engine = DiffEngine {
        upper_tar: builder.into_inner().expect("finish tar"),
    };

    let root = seeded_worktree("symlink-escape");
    let outside = std::env::temp_dir().join(format!(
        "greenlit-wb-outside-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&outside);
    std::fs::create_dir_all(&outside).expect("mk outside dir");
    let outside_target = outside.join("secret.txt");
    std::fs::write(&outside_target, b"do not touch").expect("seed outside target");
    std::os::unix::fs::symlink(&outside_target, root.join("victim")).expect("seed victim symlink");

    let mut confirm = FixedConfirm {
        answer: true,
        seen: Vec::new(),
    };
    let outcome = run_write_back(&engine, "c", &root, &mut confirm)
        .await
        .expect("write-back");

    assert!(matches!(outcome, WriteBackOutcome::Applied(_)));
    // The outside file is exactly what it was — never opened, never
    // truncated, through the symlink.
    assert_eq!(
        std::fs::read(&outside_target).expect("outside target still readable"),
        b"do not touch"
    );
    // The workspace path is now the plain replacement file, not a symlink.
    let meta = std::fs::symlink_metadata(root.join("victim")).expect("stat victim");
    assert!(
        !meta.file_type().is_symlink(),
        "victim is no longer a symlink"
    );
    assert_eq!(
        std::fs::read(root.join("victim")).expect("read victim"),
        b"replacement"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

#[tokio::test]
async fn empty_overlay_reports_no_changes() {
    let engine = DiffEngine {
        // A bare root directory entry with nothing beneath it.
        upper_tar: {
            let mut builder = Builder::new(Vec::new());
            let mut dir = Header::new_gnu();
            dir.set_entry_type(EntryType::Directory);
            dir.set_mode(0o755);
            dir.set_size(0);
            dir.set_cksum();
            builder
                .append_data(&mut dir, "upper/", std::io::empty())
                .expect("append root");
            builder.into_inner().expect("finish")
        },
    };
    let root = seeded_worktree("empty");
    let mut confirm = FixedConfirm {
        answer: true,
        seen: Vec::new(),
    };

    let outcome = run_write_back(&engine, "c", &root, &mut confirm)
        .await
        .expect("write-back");
    assert_eq!(outcome, WriteBackOutcome::NoChanges);
    assert!(confirm.seen.is_empty(), "an empty diff prompts for nothing");

    let _ = std::fs::remove_dir_all(&root);
}

/// A cheap deterministic fingerprint of a directory tree (relative paths + file
/// bytes), for host-unchanged assertions.
fn tree_fingerprint(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .expect("read_dir")
            .map(|e| e.expect("entry").path())
            .collect();
        entries.sort();
        for path in entries {
            let rel = path
                .strip_prefix(root)
                .expect("under root")
                .to_string_lossy()
                .into_owned();
            if path.is_dir() {
                out.push((format!("{rel}/"), Vec::new()));
                stack.push(path);
            } else {
                out.push((rel, std::fs::read(&path).expect("read file")));
            }
        }
    }
    out.sort();
    out
}
