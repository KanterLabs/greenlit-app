use std::path::PathBuf;
use std::rc::Rc;

use crate::Context;
use crate::functions::hash_files::test_support::InMemoryFs;

use super::eval_string;

#[test]
fn hash_files_documented_rows_search_order_symlinks_and_deep_trees() {
    let documented = InMemoryFs::new("/workspace")
        .with_file("/workspace/package-lock.json", b"npm".to_vec())
        .with_file("/workspace/sub/Gemfile.lock", b"ruby".to_vec());
    let context = Context::new(Rc::new(documented));
    assert_eq!(
        eval_string("hashFiles('**/package-lock.json')", &context),
        "0127673cce488fddee51bab215fcfe67294cf8d28ffca9ea44018f275d9a8cd6"
    );
    assert_eq!(
        eval_string(
            "hashFiles('**/package-lock.json', '**/Gemfile.lock')",
            &context
        ),
        "8e7a1e77c9188ef5611654142ce92a14641a76bba92a9b938f0a571bd0a05167"
    );

    let rooted = Context::new(Rc::new(
        InMemoryFs::new("/workspace").with_file("/workspace/src/app.js", b"root-js".to_vec()),
    ));
    assert_eq!(
        eval_string("hashFiles('/src/*.js')", &rooted),
        "de565fb11034c2f0fbe03a74130212701ffdff881a29b21b0d71d6034aff569b"
    );

    let home_rooted = Context::new(Rc::new(
        InMemoryFs::new("/workspace")
            .with_home("/workspace/home")
            .with_file("/workspace/home/.config/tool", b"home".to_vec()),
    ));
    assert_eq!(
        eval_string("hashFiles('~/.config/tool')", &home_rooted),
        "66494229ded9a143763a170082a14503d2c6823fc733320b372fb4a46a9d0d3f"
    );

    let excluded = Context::new(Rc::new(
        InMemoryFs::new("/workspace")
            .with_file("/workspace/lib/drop.js", b"drop".to_vec())
            .with_file("/workspace/src/keep.js", b"keep".to_vec()),
    ));
    assert_eq!(
        eval_string("hashFiles('**/*.js', '!lib/**')", &excluded),
        "08a3fa15234ea9a65439f2f59a1029a82d68aa1c11955ed394a06fbcf6c47fe8"
    );

    let ordered = Context::new(Rc::new(
        InMemoryFs::new("/workspace")
            .with_file("/workspace/a/one.txt", b"a".to_vec())
            .with_file("/workspace/b/one.txt", b"b".to_vec()),
    ));
    assert_eq!(
        eval_string("hashFiles('b/**/*.txt', 'a/**/*.txt')", &ordered),
        "18d79cb747ea174c59f3a3b41768672526d56fecc58360a99d283d0f9b0a3cc0"
    );

    let symlink_file = Context::new(Rc::new(
        InMemoryFs::new("/workspace")
            .with_file("/workspace/target.txt", b"target".to_vec())
            .with_symlink("/workspace/alias.txt", "/workspace/target.txt"),
    ));
    assert_eq!(
        eval_string("hashFiles('alias.txt')", &symlink_file),
        "818002f3a8c6708c9dac7ddf5374eb2cb72b7528cf079247ddea31b2475a8d57"
    );

    let mut deep_path = PathBuf::from("/workspace/deep");
    for index in 0..205 {
        deep_path.push(format!("d{index}"));
    }
    deep_path.push("file.txt");
    let deep = Context::new(Rc::new(
        InMemoryFs::new("/workspace").with_file(deep_path, b"deep".to_vec()),
    ));
    assert_eq!(
        eval_string("hashFiles('deep/**')", &deep),
        "b0abc13643e51e6fd3e6cc0908560f235b2195433686e51add36c3d52b087cb1"
    );

    let cycle = Context::new(Rc::new(
        InMemoryFs::new("/workspace")
            .with_file("/workspace/real/a.txt", b"cycle".to_vec())
            .with_symlink("/workspace/real/loop", "/workspace/real"),
    ));
    assert_eq!(
        eval_string("hashFiles('--follow-symbolic-links', 'real/**')", &cycle),
        "59c2f0ab828e8eaf333032c8e77fc8606b2ec70417b8e008c057396cc7ca7ecc"
    );

    let directory_link = Context::new(Rc::new(
        InMemoryFs::new("/workspace")
            .with_file("/workspace/real/a.txt", b"ignored".to_vec())
            .with_symlink("/workspace/link", "/workspace/real"),
    ));
    assert_eq!(eval_string("hashFiles('link/*.txt')", &directory_link), "");
    assert_eq!(eval_string("hashFiles('no/such/**')", &directory_link), "");
}
