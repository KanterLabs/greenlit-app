use std::rc::Rc;

use crate::Context;
use crate::functions::hash_files::test_support::InMemoryFs;

use super::eval_string;

#[test]
fn hash_files_documented_patterns_hashing_and_workspace_boundary() {
    // https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#hashfiles
    let documented = Context::new(Rc::new(
        InMemoryFs::new("/workspace")
            .with_file("/workspace/package-lock.json", b"npm".to_vec())
            .with_file("/workspace/sub/Gemfile.lock", b"ruby".to_vec())
            .with_file("/workspace/src/app.js", b"root-js".to_vec())
            .with_file("/workspace/src/nested/app.js", b"nested-js".to_vec())
            .with_file("/workspace/lib/a.rb", b"root-rb".to_vec())
            .with_file("/workspace/lib/deep/b.rb", b"deep-rb".to_vec())
            .with_file("/workspace/lib/foo/drop.rb", b"drop-rb".to_vec()),
    ));
    let boundary = Context::new(Rc::new(
        InMemoryFs::new("/workspace")
            .with_file("/workspace/inside.txt", b"inside".to_vec())
            .with_file("/outside/outside.txt", b"outside".to_vec()),
    ));
    let rows = [
        (
            "single package-lock pattern",
            &documented,
            "hashFiles('**/package-lock.json')",
            "0127673cce488fddee51bab215fcfe67294cf8d28ffca9ea44018f275d9a8cd6",
        ),
        (
            "root src JavaScript pattern",
            &documented,
            "hashFiles('/src/*.js')",
            "de565fb11034c2f0fbe03a74130212701ffdff881a29b21b0d71d6034aff569b",
        ),
        (
            "recursive lib Ruby pattern",
            &documented,
            "hashFiles('/lib/**/*.rb')",
            "d6e331ec561abe0247d5b342ba0fd63e3a2848f904ad8d5f682802d06f944946",
        ),
        (
            "multiple lockfile patterns",
            &documented,
            "hashFiles('**/package-lock.json', '**/Gemfile.lock')",
            "8e7a1e77c9188ef5611654142ce92a14641a76bba92a9b938f0a571bd0a05167",
        ),
        (
            "recursive lib Ruby pattern with exclusion",
            &documented,
            "hashFiles('/lib/**/*.rb', '!/lib/foo/*.rb')",
            "09c43ead9e22bca87b4b5248c5f6b22b7a21e39e7e3951895863eb2eab281284",
        ),
        ("no matches", &documented, "hashFiles('no/such/**')", ""),
        (
            "workspace boundary",
            &boundary,
            "hashFiles('**/*.txt')",
            "09d0f5f66b683cd56297add2f1d75e2c9c83b1a7a4c3aad81ada0f71cb04e578",
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
