#![cfg(unix)]

#[path = "support/hash_files.rs"]
mod support;

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use greenlit_expr::{Context, RealFs};
use rustix::fs::{CWD, Mode, OFlags, mkdirat, openat};

use support::{TempTree, eval_string};

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
    // The repository root may be overlayfs, which reconstructs full paths
    // internally and rejects descriptor-relative chains at PATH_MAX. tmpfs
    // supports the kernel behavior this invariant owns. Absence, insufficient
    // capacity, or NAMETOOLONG is a hard prerequisite failure, never a skip.
    let tree = TempTree::in_base(Path::new("/dev/shm"));
    let name = "d".repeat(NAME_LEN);

    let mut dir = openat(
        CWD,
        tree.path(),
        OFlags::PATH | OFlags::DIRECTORY,
        Mode::empty(),
    )
    .expect("open temporary root");
    for depth in 0..DEPTH {
        mkdirat(&dir, name.as_str(), Mode::from_raw_mode(0o700)).unwrap_or_else(|error| {
            panic!(
                "the required /dev/shm beyond-PATH_MAX fixture failed at depth {depth}: {error:?}"
            )
        });
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
