//! Throwaway repository helpers for host-containment invariants.

use std::path::{Path, PathBuf};

use crate::engine_support::unique_suffix;

/// Seed a throwaway host "repository" with a canary file and a nested file.
pub fn seed_repo(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("greenlit-repo-{tag}-{}", unique_suffix()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).expect("mk repo");
    std::fs::write(root.join("canary.txt"), b"canary").expect("write canary");
    std::fs::write(root.join("src/lib.txt"), b"nested").expect("write nested");
    root
}

/// A deterministic fingerprint of a directory tree (relative paths + file
/// bytes), for "host tree unchanged" assertions.
pub fn tree_fingerprint(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
            .expect("read_dir")
            .map(|entry| entry.expect("entry").path())
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
