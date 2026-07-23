#![cfg(unix)]

use std::io::{self, Write};
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rustix::fs::{CWD, Mode, OFlags, mkdirat, openat};

use greenlit_expr::{
    Context, DirEntry, EntryKind, HashFilesError, HashFilesFs, OpenDirectory, OpenedDir, RealFs,
    Value, evaluate, parse,
};

#[test]
fn public_hash_files_boundary_enforces_containment_special_nodes_and_bounded_alias_graphs() {
    // Invariant class: exercise the public parse/evaluate boundary with RealFs
    // and prove a malicious cloned repository cannot read outside its supplied
    // workspace, block on a special node, or amplify a compact symlink DAG
    // into exponentially repeated directory traversal.
    let tree = TempTree::new();
    let workspace = tree.path().join("workspace");
    let outside = tree.path().join("outside");
    std::fs::create_dir_all(&workspace).expect("create test workspace");
    std::fs::create_dir_all(&outside).expect("create outside test directory");
    std::fs::write(outside.join("secret.txt"), b"outside secret").expect("write outside test file");

    // Root establishment is one no-symlink open. A preexisting intermediate
    // alias is rejected with the physical-path action; the old
    // canonicalize-then-open sequence would have accepted and hashed it.
    let physical_parent = tree.path().join("physical-parent");
    let physical_workspace = physical_parent.join("workspace");
    let linked_parent = tree.path().join("linked-parent");
    let linked_workspace = tree.path().join("linked-workspace");
    std::fs::create_dir_all(&physical_workspace).expect("create physical workspace");
    std::fs::write(
        physical_workspace.join("payload.txt"),
        b"aliased root payload",
    )
    .expect("write aliased-root payload");
    symlink(&physical_parent, &linked_parent).expect("create intermediate root alias");
    symlink(&physical_workspace, &linked_workspace).expect("create final root alias");
    let expression = parse("hashFiles('payload.txt')").expect("parse aliased-root expression");
    for (name, root) in [
        ("intermediate root alias", linked_parent.join("workspace")),
        ("final root alias", linked_workspace),
    ] {
        let linked_context = Context::new(Arc::new(RealFs::new(root)));
        let error = evaluate(&expression, &linked_context).expect_err(name);
        assert!(
            error
                .to_string()
                .contains("use the workspace's physical directory path"),
            "{name} error lacked its single corrective action: {error}"
        );
    }

    symlink(
        outside.join("secret.txt"),
        workspace.join("outside-file.txt"),
    )
    .expect("create outside file symlink");
    symlink(&outside, workspace.join("outside-directory"))
        .expect("create outside directory symlink");
    let _socket = UnixListener::bind(workspace.join("not-a-file.sock"))
        .expect("create non-regular filesystem node");

    let context = Context::new(Arc::new(RealFs::new(&workspace)));
    let rows = [
        (
            "final file symlink outside the workspace",
            "hashFiles('outside-file.txt')",
        ),
        (
            "followed directory symlink outside the workspace",
            "hashFiles('--follow-symbolic-links', 'outside-directory/**/*.txt')",
        ),
        (
            "home-rooted pattern cannot scan outside the workspace",
            "hashFiles('~/**')",
        ),
        (
            "socket is not treated as a readable file",
            "hashFiles('not-a-file.sock')",
        ),
    ];
    for (name, expression) in rows {
        assert_eq!(eval_string(expression, &context), "", "{name}");
    }

    // Containment is target-sensitive rather than a blanket symlink ban.
    std::fs::write(workspace.join("inside.txt"), b"inside").expect("write inside test file");
    symlink(
        workspace.join("inside.txt"),
        workspace.join("inside-link.txt"),
    )
    .expect("create inside file symlink");
    assert_eq!(
        eval_string("hashFiles('inside-link.txt')", &context),
        "09d0f5f66b683cd56297add2f1d75e2c9c83b1a7a4c3aad81ada0f71cb04e578"
    );

    // A component that changes after the root was pinned is resolved again
    // from the held parent. Replacing an in-workspace directory with an
    // outside link therefore yields no match rather than outside bytes.
    let mutable = workspace.join("mutable");
    std::fs::create_dir(&mutable).expect("create mutable directory");
    std::fs::write(mutable.join("secret.txt"), b"inside before replacement")
        .expect("write mutable file");
    std::fs::rename(&mutable, workspace.join("mutable-moved")).expect("rename mutable directory");
    symlink(&outside, &mutable).expect("replace mutable directory with outside link");
    assert_eq!(
        eval_string(
            "hashFiles('--follow-symbolic-links', 'mutable/**/*.txt')",
            &context,
        ),
        ""
    );

    // The pinned toolkit remembers only the active traversal chain. Thirty
    // levels of two aliases would produce 2^30 visits before its 120-second
    // timeout. Greenlit visits each canonical directory once, retaining the
    // first-discovered alias and hashing the payload once.
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Misc/layoutbin/hashFiles/index.js
    let dag_nodes = workspace.join("dag-nodes");
    for level in 0..=30 {
        std::fs::create_dir_all(dag_nodes.join(format!("level-{level}")))
            .expect("create symlink DAG level");
    }
    for level in 0..30 {
        let directory = dag_nodes.join(format!("level-{level}"));
        let target = format!("../level-{}", level + 1);
        symlink(&target, directory.join("left")).expect("create left DAG alias");
        symlink(&target, directory.join("right")).expect("create right DAG alias");
    }
    std::fs::write(dag_nodes.join("level-30/payload.txt"), b"bounded-dag")
        .expect("write symlink DAG payload");
    symlink(dag_nodes.join("level-0"), workspace.join("dag-entry"))
        .expect("create symlink DAG entry");
    assert_eq!(
        eval_string(
            "hashFiles('--follow-symbolic-links', 'dag-entry/**/*.txt')",
            &context,
        ),
        "446df343c576391afccb3d8986d36baecba5a4f5736f6572bfc5200a4047958b"
    );

    // The root object is pinned at construction. Replacing its original name
    // with an outside symlink must neither redirect later traversal nor change
    // the bytes hashed from the originally supplied workspace.
    let pinned_root = tree.path().join("pinned-workspace");
    let moved_root = tree.path().join("pinned-workspace-moved");
    std::fs::create_dir(&pinned_root).expect("create pinned workspace");
    std::fs::write(pinned_root.join("payload.txt"), b"pinned payload")
        .expect("write pinned payload");
    std::fs::write(outside.join("payload.txt"), b"outside replacement")
        .expect("write outside replacement payload");
    let pinned_context = Context::new(Arc::new(RealFs::new(&pinned_root)));
    let pinned_digest = eval_string("hashFiles('payload.txt')", &pinned_context);
    std::fs::rename(&pinned_root, &moved_root).expect("rename pinned workspace");
    symlink(&outside, &pinned_root).expect("replace pinned workspace name");
    assert_eq!(
        eval_string("hashFiles('payload.txt')", &pinned_context),
        pinned_digest,
        "post-construction root replacement redirected descriptor-relative access"
    );

    // Directory identity and enumeration are captured from one held object.
    // Pulling after its old name is replaced must still enumerate the object
    // whose identity was returned, not the replacement directory.
    let stream_root = tree.path().join("stream-workspace");
    let stream_dir = stream_root.join("source");
    let moved_stream_dir = stream_root.join("source-moved");
    std::fs::create_dir_all(&stream_dir).expect("create stream directory");
    std::fs::write(stream_dir.join("original.txt"), b"original")
        .expect("write original stream entry");
    let stream_fs = RealFs::new(&stream_root);
    let mut opened = stream_fs
        .open_dir(&stream_dir)
        .expect("open identified directory stream");
    std::fs::rename(&stream_dir, &moved_stream_dir).expect("rename streamed directory");
    std::fs::create_dir(&stream_dir).expect("create replacement stream directory");
    std::fs::write(stream_dir.join("replacement.txt"), b"replacement")
        .expect("write replacement stream entry");
    let mut names = Vec::new();
    while let Some(entry) = opened.next_entry().expect("pull held directory entry") {
        names.push(entry.name);
    }
    assert_eq!(names, ["original.txt"]);

    let missing_context =
        Context::new(Arc::new(RealFs::new(tree.path().join("missing-workspace"))));
    assert_eq!(eval_string("hashFiles('**')", &missing_context), "");
}

#[test]
fn public_hash_files_traversal_state_stays_depth_proportional_beyond_path_max() {
    // Invariant class: retained lexical state during traversal must be
    // proportional to depth, not depth squared (finding 4 of the hashFiles
    // hardening review). A per-frame full-path clone would retain roughly
    // depth * (depth * name_len) / 2 bytes; a single shared buffer retains
    // at most depth * name_len. This directly demonstrates the underlying
    // fix too: every level is created via a relative `mkdirat` from its own
    // parent fd (never a joined path string), so the chain's true lexical
    // length can — and here does — exceed Linux's `PATH_MAX` (4096 bytes)
    // by more than an order of magnitude; passing a path that long to any
    // ordinary path-based syscall would fail with `ENAMETOOLONG`.
    const DEPTH: usize = 800;
    const NAME_LEN: usize = 100;
    let tree = TempTree::new();
    let name = "d".repeat(NAME_LEN);

    let mut dir = openat(
        CWD,
        tree.path(),
        OFlags::PATH | OFlags::DIRECTORY,
        Mode::empty(),
    )
    .expect("open temporary root");
    for _ in 0..DEPTH {
        mkdirat(&dir, name.as_str(), Mode::from_raw_mode(0o700)).expect("mkdirat one level deeper");
        dir = openat(
            &dir,
            name.as_str(),
            OFlags::PATH | OFlags::DIRECTORY,
            Mode::empty(),
        )
        .expect("open the level just created");
    }
    let payload = openat(
        &dir,
        "payload.txt",
        OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC,
        Mode::from_raw_mode(0o600),
    )
    .expect("create deep payload file");
    std::fs::File::from(payload)
        .write_all(b"deep")
        .expect("write deep payload");

    let total_lexical_bytes = DEPTH * (NAME_LEN + 1);
    assert!(
        total_lexical_bytes > 16 * 4096,
        "test setup must exceed PATH_MAX by a wide margin, got {total_lexical_bytes} bytes"
    );

    let context = Context::new(Arc::new(RealFs::new(tree.path())));
    let started = Instant::now();
    let digest = eval_string("hashFiles('**')", &context);
    let elapsed = started.elapsed();

    assert_eq!(digest.len(), 64, "the deep payload file must be matched");
    assert!(
        elapsed < Duration::from_secs(10),
        "depth-proportional traversal took {elapsed:?}; a per-frame path clone would be far slower"
    );
}

#[test]
fn public_hash_files_matched_dangling_symlink_fails_instead_of_matching_empty() {
    // Invariant class: GitHub's pinned hash helper `fs.statSync`s every
    // matched path; a dangling symlink's target does not exist, so the
    // helper throws and the runner surfaces `hashFiles(...) failed` as an
    // expression error. The previous implementation instead swallowed this
    // as a silent no-op, producing a successful empty-string digest locally
    // where GitHub fails the whole expression (finding 8 of the hashFiles
    // hardening review).
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Misc/expressionFunc/hashFiles/src/hashFiles.ts
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Runner.Worker/Expressions/HashFilesFunction.cs
    let tree = TempTree::new();
    let workspace = tree.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("create dangling-symlink workspace");
    symlink(
        workspace.join("does-not-exist.txt"),
        workspace.join("dangling.txt"),
    )
    .expect("create dangling symlink");

    let context = Context::new(Arc::new(RealFs::new(&workspace)));
    let expression = parse("hashFiles('**')").expect("parse dangling-symlink hashFiles expression");
    let error = evaluate(&expression, &context).expect_err("a matched dangling symlink must fail");
    let greenlit_expr::EvalError::HashFiles(HashFilesError::DanglingSymlink { path }) = error
    else {
        panic!("expected a DanglingSymlink error, got {error:?}");
    };
    assert!(
        path.ends_with("dangling.txt"),
        "error must name the dangling link itself: {path}"
    );
    let rendered = HashFilesError::DanglingSymlink { path }.to_string();
    assert!(
        rendered.contains("fix or remove the dangling link"),
        "error lacked its single corrective action: {rendered}"
    );

    // A symlink that resolves fine is unaffected: this is target-sensitive,
    // not a blanket symlink penalty.
    std::fs::write(workspace.join("real.txt"), b"real").expect("write real target");
    symlink(workspace.join("real.txt"), workspace.join("healthy.txt"))
        .expect("create healthy symlink");
    let healthy_context = Context::new(Arc::new(RealFs::new(&workspace)));
    let digest = eval_string("hashFiles('healthy.txt')", &healthy_context);
    assert_eq!(digest.len(), 64);
}

/// A trait-level interposer around [`RealFs`] that renames a chosen held
/// directory to a sibling *outside* the workspace at the exact call
/// boundary the hardening re-verification flagged: after the traversal has
/// already enumerated it, but before it accesses one of its children.
/// Finding 1 of the hashFiles hardening re-verification: an earlier
/// revision resolved that child relative to the still-held (now-relocated)
/// ancestor descriptor, so the child kept resolving even though its true
/// path was now outside the workspace. `RealFs` itself cannot be asked to
/// simulate this — it always resolves fresh from its own pinned root — so
/// this wrapper is the only way to prove the fix from outside: it lets a
/// real rename land in the narrow window between a real `RealOpenDir`
/// yielding a directory it already opened and that same traversal
/// requesting one of its children.
#[derive(Debug)]
struct RenameOnAccessFs {
    inner: RealFs,
    rename_from: PathBuf,
    rename_to: PathBuf,
    renamed: Arc<AtomicBool>,
}

impl HashFilesFs for RenameOnAccessFs {
    fn workspace_root(&self) -> &Path {
        self.inner.workspace_root()
    }

    fn open_dir(&self, path: &Path) -> io::Result<OpenedDir<'_>> {
        let opened = self.inner.open_dir(path)?;
        if path != self.rename_from {
            return Ok(opened);
        }
        Ok(OpenedDir::new(
            opened.identity.clone(),
            Box::new(RenameHookDir {
                inner: opened,
                rename_from: self.rename_from.clone(),
                rename_to: self.rename_to.clone(),
                renamed: Arc::clone(&self.renamed),
            }),
        ))
    }

    fn entry_kind(&self, path: &Path) -> io::Result<EntryKind> {
        self.inner.entry_kind(path)
    }

    fn hash_file_sha256(
        &self,
        path: &Path,
        check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]> {
        self.inner.hash_file_sha256(path, check_timeout)
    }
}

struct RenameHookDir<'a> {
    inner: OpenedDir<'a>,
    rename_from: PathBuf,
    rename_to: PathBuf,
    renamed: Arc<AtomicBool>,
}

impl<'a> RenameHookDir<'a> {
    /// Performs the real, on-disk rename exactly once, the first time this
    /// held directory's caller tries to reach one of its children.
    fn rename_held_directory_away(&self) {
        if !self.renamed.swap(true, Ordering::AcqRel) {
            std::fs::rename(&self.rename_from, &self.rename_to)
                .expect("rename the held directory to outside the workspace");
        }
    }
}

impl<'a> OpenDirectory<'a> for RenameHookDir<'a> {
    fn next_entry(&mut self) -> io::Result<Option<DirEntry>> {
        self.inner.next_entry()
    }

    fn open_child_dir(
        &self,
        name: &str,
        lexical_path: &Path,
        follow: bool,
    ) -> io::Result<OpenedDir<'a>> {
        self.rename_held_directory_away();
        self.inner.open_child_dir(name, lexical_path, follow)
    }

    fn hash_child_file(
        &self,
        name: &str,
        lexical_path: &Path,
        follow: bool,
        check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]> {
        self.rename_held_directory_away();
        self.inner
            .hash_child_file(name, lexical_path, follow, check_timeout)
    }
}

#[test]
fn public_hash_files_contains_a_held_directory_renamed_outside_the_workspace_mid_traversal() {
    // Finding 1 (the critical one) of the hashFiles hardening
    // re-verification: the previous fd-chain design resolved a child
    // relative to whatever descriptor its parent directory happened to
    // still be, not relative to a fresh, root-anchored lookup — so once
    // `workspace/a` was renamed to a directory outside the workspace, its
    // already-held descriptor kept serving children from that now-outside
    // location. The fix makes every access a single `openat2` call over the
    // full path from the one pinned root, so the rename instead surfaces as
    // `ENOENT` (the entry vanished from the workspace's perspective) —
    // never outside content.
    let tree = TempTree::new();
    let workspace = tree.path().join("workspace");
    let held_directory = workspace.join("a");
    std::fs::create_dir_all(&held_directory).expect("create held traversal directory");
    std::fs::write(held_directory.join("b.txt"), b"must never be hashed")
        .expect("write held directory's child file");
    let outside = tree.path().join("outside-a");

    let renamed = Arc::new(AtomicBool::new(false));
    let fs = RenameOnAccessFs {
        inner: RealFs::new(&workspace),
        rename_from: held_directory,
        rename_to: outside.clone(),
        renamed: Arc::clone(&renamed),
    };
    let context = Context::new(Arc::new(fs));
    let digest = eval_string("hashFiles('a/*.txt')", &context);

    assert!(
        renamed.load(Ordering::Acquire),
        "test setup bug: the interposer never fired, so it proved nothing"
    );
    assert!(
        outside.join("b.txt").exists(),
        "test setup bug: the held directory was not actually relocated outside the workspace"
    );
    assert_eq!(
        digest, "",
        "a directory renamed outside the workspace after being held must never contribute a \
         match — the outside file's content must never be hashed"
    );
}

fn eval_string(source: &str, context: &Context) -> String {
    let expression = parse(source).unwrap_or_else(|error| panic!("parse({source:?}): {error}"));
    match evaluate(&expression, context)
        .unwrap_or_else(|error| panic!("evaluate({source:?}): {error}"))
    {
        Value::String(value) => value,
        value => panic!("evaluate({source:?}) returned {value:?}, expected a string"),
    }
}

static NEXT_TEMP_TREE: AtomicU64 = AtomicU64::new(0);

struct TempTree {
    path: PathBuf,
}

impl TempTree {
    fn new() -> Self {
        let base = std::env::temp_dir();
        loop {
            let serial = NEXT_TEMP_TREE.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!(
                "greenlit-expr-hash-files-{}-{serial}",
                std::process::id()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self { path },
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("create temporary test tree: {error}"),
            }
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.path).expect("remove temporary test tree");
    }
}
