use std::sync::Arc;

use crate::Context;
use crate::functions::hash_files::test_support::InMemoryFs;

use super::eval_string;

#[test]
fn hash_files_documented_patterns_hashing_and_workspace_boundary() {
    // https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#hashfiles
    let documented = Context::new(Arc::new(
        InMemoryFs::new("/workspace")
            .with_file("/workspace/package-lock.json", b"npm".to_vec())
            .with_file("/workspace/sub/Gemfile.lock", b"ruby".to_vec())
            .with_file("/workspace/src/app.js", b"root-js".to_vec())
            .with_file("/workspace/src/nested/app.js", b"nested-js".to_vec())
            .with_file("/workspace/lib/a.rb", b"root-rb".to_vec())
            .with_file("/workspace/lib/deep/b.rb", b"deep-rb".to_vec())
            .with_file("/workspace/lib/foo/drop.rb", b"drop-rb".to_vec()),
    ));
    let boundary = Context::new(Arc::new(
        InMemoryFs::new("/workspace")
            .with_file("/workspace/inside.txt", b"inside".to_vec())
            .with_file("/outside/outside.txt", b"outside".to_vec()),
    ));
    // New defect (partial-match overreach): GitHub's toolkit globber yields
    // a non-directory only on a *full* pattern match, negatives folded
    // (`match`, not `partialMatch`) — never merely because it is the
    // literal search root a glob pattern's non-glob prefix rooted at.
    // https://github.com/actions/toolkit/blob/main/packages/glob/src/internal-globber.ts
    // https://github.com/actions/toolkit/blob/main/packages/glob/src/internal-pattern-helper.ts
    let partial_match_file = Context::new(Arc::new(InMemoryFs::new("/workspace").with_file(
        "/workspace/src",
        b"src is a plain file here, not a directory".to_vec(),
    )));
    let partial_match_dangling_symlink = Context::new(Arc::new(
        InMemoryFs::new("/workspace").with_symlink("/workspace/src", "/workspace/does-not-exist"),
    ));
    let negated_exact_match = Context::new(Arc::new(InMemoryFs::new("/workspace").with_file(
        "/workspace/a.txt",
        b"folded out by its own negation".to_vec(),
    )));
    // The same partial-match rule must hold for a non-directory reached during
    // traversal, not only at a glob's literal search root. `pkg/cfg` is a
    // dangling symlink that partially matches `*/cfg/*.yml` (its prefix could
    // still lead to a match) but is not itself a full match, so it must be
    // skipped — never followed into a `DanglingSymlink` failure. `keep.txt`
    // exists only so `pkg` enumerates as a directory during the walk.
    let partial_match_child_dangling_symlink = Context::new(Arc::new(
        InMemoryFs::new("/workspace")
            .with_file("/workspace/pkg/keep.txt", b"keeps pkg a directory".to_vec())
            .with_symlink("/workspace/pkg/cfg", "/workspace/does-not-exist"),
    ));
    let rows = [
        (
            "single package-lock pattern",
            &documented,
            "hashFiles('**/package-lock.json')",
            "0127673cce488fddee51bab215fcfe67294cf8d28ffca9ea44018f275d9a8cd6",
        ),
        (
            "documented leading slash is actually filesystem absolute",
            &documented,
            "hashFiles('/src/*.js')",
            "",
        ),
        (
            "workspace-relative src JavaScript pattern",
            &documented,
            "hashFiles('src/*.js')",
            "de565fb11034c2f0fbe03a74130212701ffdff881a29b21b0d71d6034aff569b",
        ),
        (
            "documented leading slash recursive pattern is absolute",
            &documented,
            "hashFiles('/lib/**/*.rb')",
            "",
        ),
        (
            "multiple lockfile patterns",
            &documented,
            "hashFiles('**/package-lock.json', '**/Gemfile.lock')",
            "8e7a1e77c9188ef5611654142ce92a14641a76bba92a9b938f0a571bd0a05167",
        ),
        (
            "absolute positive root stays outside workspace",
            &documented,
            "hashFiles('/lib/**/*.rb', '!/lib/foo/*.rb')",
            "",
        ),
        ("no matches", &documented, "hashFiles('no/such/**')", ""),
        (
            "workspace boundary",
            &boundary,
            "hashFiles('**/*.txt')",
            "09d0f5f66b683cd56297add2f1d75e2c9c83b1a7a4c3aad81ada0f71cb04e578",
        ),
        (
            "a plain file at a glob's literal search root is only a partial match",
            &partial_match_file,
            "hashFiles('src/*.js')",
            "",
        ),
        (
            "a dangling symlink at a glob's literal search root is only a partial \
             match — no DanglingSymlink failure",
            &partial_match_dangling_symlink,
            "hashFiles('src/*.js')",
            "",
        ),
        (
            "an exact literal pattern folded out by its own negation matches nothing",
            &negated_exact_match,
            "hashFiles('a.txt', '!a.txt')",
            "",
        ),
        (
            "a dangling symlink reached during traversal that only partially \
             matches is skipped, not a DanglingSymlink failure",
            &partial_match_child_dangling_symlink,
            "hashFiles('*/cfg/*.yml')",
            "",
        ),
    ];

    for (name, context, source, expected) in rows {
        assert_eq!(
            eval_string(source, context),
            expected,
            "hashFiles row {name}"
        );
    }
}
