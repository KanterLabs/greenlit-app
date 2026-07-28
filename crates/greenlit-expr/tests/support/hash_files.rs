use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use greenlit_expr::{Context, Value, evaluate, parse};

pub(crate) fn eval_string(source: &str, context: &Context) -> String {
    let expression = parse(source).unwrap_or_else(|error| panic!("parse({source:?}): {error}"));
    match evaluate(&expression, context)
        .unwrap_or_else(|error| panic!("evaluate({source:?}): {error}"))
    {
        Value::String(value) => value,
        value => panic!("evaluate({source:?}) returned {value:?}, expected a string"),
    }
}

static NEXT_TEMP_TREE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct TempTree {
    path: PathBuf,
}

impl TempTree {
    pub(crate) fn in_base(base: &Path) -> Self {
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

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.path).expect("remove temporary test tree");
    }
}
