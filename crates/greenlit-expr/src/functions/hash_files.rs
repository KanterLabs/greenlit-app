//! `hashFiles(pattern…)`: glob matching plus GitHub's documented two-level
//! SHA-256 algorithm.
//!
//! Source: design memo §3.8, itself derived from
//! `src/Runner.Worker/Expressions/HashFilesFunction.cs`,
//! `src/Misc/expressionFunc/hashFiles/src/hashFiles.ts`, and
//! `@actions/toolkit` `packages/glob/src/internal-*.ts`. Filesystem access is
//! behind the [`HashFilesFs`] trait specifically so tests can supply an
//! in-memory fake while [`RealFs`] ships the real, `std::fs`-backed
//! implementation now (per the Phase 1 task list).
//!
//! Traversal order and symlink behavior follow current toolkit source. The
//! remaining documented-vs-source ambiguity is leading-`/` rooting; its
//! observed-run follow-up is recorded in `ARCHITECTURE.md`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use globset::GlobBuilder;
use sha2::{Digest, Sha256};

use crate::value::Value;

/// The kind of a directory entry returned by [`HashFilesFs::read_dir`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symbolic link (to a file or directory — not yet resolved).
    Symlink,
}

/// One directory entry: a bare file name (not a full path) plus its kind.
#[derive(Debug, Clone)]
pub struct DirEntry {
    /// The entry's bare name within its parent directory.
    pub name: String,
    /// What kind of entry this is.
    pub kind: EntryKind,
}

/// Filesystem access for `hashFiles()`, injected so evaluation never talks
/// to `std::fs` directly — the real implementation is [`RealFs`]; tests
/// substitute an in-memory fake (see `test_support` below).
pub trait HashFilesFs: std::fmt::Debug {
    /// The directory `hashFiles` patterns are rooted against by default
    /// (GitHub's `GITHUB_WORKSPACE`).
    fn workspace_root(&self) -> &Path;
    /// The directory a leading `~`/`~/…` pattern roots against. `None` if
    /// unavailable, in which case such a pattern contributes zero matches
    /// rather than erroring (real runners always have a home directory; this
    /// is a defensive default for environments that don't).
    fn home_dir(&self) -> Option<&Path> {
        None
    }
    /// Lists `path`'s direct children (bare names + kind), not recursive.
    fn read_dir(&self, path: &Path) -> std::io::Result<Vec<DirEntry>>;
    /// Reads a regular file's full contents.
    fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>>;
    /// Returns the kind of `path` without following a final symbolic link.
    ///
    /// The default preserves compatibility for injected implementations
    /// that predate this method. Implementations with metadata access should
    /// override it so symlinks can be distinguished precisely.
    fn entry_kind(&self, path: &Path) -> std::io::Result<EntryKind> {
        match self.read_dir(path) {
            Ok(_) => Ok(EntryKind::Dir),
            Err(dir_error) => match self.read_file(path) {
                Ok(_) => Ok(EntryKind::File),
                Err(_) => Err(dir_error),
            },
        }
    }
    /// Resolves symbolic links for directory-cycle detection.
    ///
    /// The identity default is suitable for virtual filesystems with no
    /// symlinks. Real filesystems should override it with canonicalization.
    fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
        Ok(path.to_path_buf())
    }
}

/// The production [`HashFilesFs`], backed directly by `std::fs`, rooted at a
/// given workspace directory.
#[derive(Debug)]
pub struct RealFs {
    root: PathBuf,
    home: Option<PathBuf>,
}

impl RealFs {
    /// Builds a real filesystem rooted at `root` (GitHub's `GITHUB_WORKSPACE`
    /// equivalent). Reads `$HOME` once at construction for `~`-rooted
    /// patterns.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        RealFs {
            root: root.into(),
            home: std::env::var_os("HOME").map(PathBuf::from),
        }
    }
}

impl HashFilesFs for RealFs {
    fn workspace_root(&self) -> &Path {
        &self.root
    }

    fn home_dir(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    fn read_dir(&self, path: &Path) -> std::io::Result<Vec<DirEntry>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let kind = if file_type.is_symlink() {
                EntryKind::Symlink
            } else if file_type.is_dir() {
                EntryKind::Dir
            } else {
                EntryKind::File
            };
            out.push(DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                kind,
            });
        }
        Ok(out)
    }

    fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn entry_kind(&self, path: &Path) -> std::io::Result<EntryKind> {
        let file_type = std::fs::symlink_metadata(path)?.file_type();
        if file_type.is_symlink() {
            Ok(EntryKind::Symlink)
        } else if file_type.is_dir() {
            Ok(EntryKind::Dir)
        } else {
            Ok(EntryKind::File)
        }
    }

    fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }
}

/// A `hashFiles()` failure.
#[derive(Debug, thiserror::Error)]
pub enum HashFilesError {
    /// The first argument started with `--` but wasn't
    /// `--follow-symbolic-links`.
    #[error("invalid glob option {0:?}")]
    InvalidOption(String),
    /// A pattern contained a `.`/`..` path segment outside the recognized
    /// leading `.`/`./`/`~`/`~/` prefix forms.
    #[error("relative pathing '.' and '..' is not allowed in hashFiles pattern {pattern:?}")]
    RelativePathingNotAllowed {
        /// The offending pattern, as written.
        pattern: String,
    },
    /// A filesystem read failed (and wasn't a leniently skipped broken
    /// symlink).
    #[error("hashFiles() failed to read {path}: {source}")]
    Io {
        /// The path that failed to read.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A pattern (after rooting) failed to compile as a glob.
    #[error("hashFiles() pattern {pattern:?} is not a valid glob: {source}")]
    InvalidPattern {
        /// The fully-rooted, brace-escaped pattern that failed to compile.
        pattern: String,
        /// The underlying glob-compilation error.
        #[source]
        source: globset::Error,
    },
}

enum RootKind {
    Workspace,
    Home,
}

/// Splits a recognized leading `.`/`./`/`~`/`~/` rooting prefix off `pattern`,
/// or leaves it untouched (workspace-relative) otherwise.
///
/// Source: design memo §3.8 ("Rooting"). The leading-`/` case is a
/// deliberate, documented departure from the *implementation's* literal
/// behavior (filesystem-absolute) in favor of the *docs'* description
/// ("root level" of the repo) — see the inline comment on that arm.
fn strip_root_prefix(pattern: &str) -> (RootKind, String) {
    if pattern == "." {
        (RootKind::Workspace, String::new())
    } else if let Some(rest) = pattern.strip_prefix("./") {
        (RootKind::Workspace, rest.to_string())
    } else if pattern == "~" {
        (RootKind::Home, String::new())
    } else if let Some(rest) = pattern.strip_prefix("~/") {
        (RootKind::Home, rest.to_string())
    } else if let Some(rest) = pattern.strip_prefix('/') {
        // GitHub's docs define hashFiles paths relative to GITHUB_WORKSPACE
        // and use `/src/*.js` for a repository-root match, while current
        // @actions/glob source treats `/` as filesystem-absolute. With no
        // observed Actions run available, AGENTS.md requires documented
        // behavior to win over source inference. ARCHITECTURE.md records the
        // discrepancy and the required observed-behavior follow-up.
        // https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#hashfiles
        // https://github.com/actions/toolkit/blob/main/packages/glob/src/internal-pattern.ts
        (RootKind::Workspace, rest.to_string())
    } else {
        (RootKind::Workspace, pattern.to_string())
    }
}

/// Rejects `.`/`..` path segments anywhere in `stripped` (the pattern with
/// its recognized rooting prefix already removed) — design memo §3.8: "`.`
/// mid-pattern and `..` anywhere are errors".
fn validate_no_dot_segments(stripped: &str, original_pattern: &str) -> Result<(), HashFilesError> {
    for seg in stripped.split('/') {
        if seg == "." || seg == ".." {
            return Err(HashFilesError::RelativePathingNotAllowed {
                pattern: original_pattern.to_string(),
            });
        }
    }
    Ok(())
}

/// Escapes `{`/`}` (that aren't already backslash-escaped) so that
/// `globset` — which supports brace alternation by default — treats them
/// literally, matching `@actions/glob`'s minimatch `nobrace: true` option.
fn escape_braces(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.push(c);
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else if c == '{' || c == '}' {
            out.push('\\');
            out.push(c);
        } else {
            out.push(c);
        }
    }
    out
}

fn compile_one(escaped_pattern: &str) -> Result<globset::GlobMatcher, HashFilesError> {
    let mut builder = GlobBuilder::new(escaped_pattern);
    // `*`/`?` never cross a path separator (minimatch's default, unlike
    // globset's own default); case-sensitive always (GitHub only
    // case-folds on Windows, and v0 hosts are Linux x86_64 only per
    // AGENTS.md); `\` escapes (minimatch's default on Linux/macOS).
    builder
        .literal_separator(true)
        .case_insensitive(false)
        .backslash_escape(true);
    builder
        .build()
        .map(|g| g.compile_matcher())
        .map_err(|source| HashFilesError::InvalidPattern {
            pattern: escaped_pattern.to_string(),
            source,
        })
}

/// Compiles the matcher(s) for one already-rooted pattern string: the
/// pattern itself, plus (unless it already ends in `**`) an implicit
/// "descendants" variant with `/**` appended — design memo §3.8: "any
/// pattern whose last segment isn't `**` (or that has a trailing `/`)
/// implicitly also matches its descendants". A file only ever needs to
/// satisfy one variant, so a trailing-slash (directory-only) pattern's
/// *base* variant simply never matches anything in our files-only
/// candidate list, giving the documented "contributes nothing to
/// hashFiles" behavior for free.
fn compile_matchers(rooted_pattern: &str) -> Result<Vec<globset::GlobMatcher>, HashFilesError> {
    let escaped = escape_braces(rooted_pattern);
    let mut variants = vec![compile_one(&escaped)?];
    let ends_with_doublestar = escaped.rsplit('/').next() == Some("**");
    if !ends_with_doublestar {
        let base = escaped.trim_end_matches('/');
        variants.push(compile_one(&format!("{base}/**"))?);
    }
    Ok(variants)
}

struct CompiledPattern {
    negated: bool,
    matchers: Vec<globset::GlobMatcher>,
    search_root: Option<PathBuf>,
}

/// Derives the toolkit `Pattern.searchPath`: literal path segments before
/// the first unescaped glob metacharacter. Escaped metacharacters remain
/// literal path characters.
fn derive_search_root(root: &Path, pattern: &str) -> PathBuf {
    let mut search_root = root.to_path_buf();
    for segment in pattern.split('/').filter(|segment| !segment.is_empty()) {
        let Some(literal) = literal_segment(segment) else {
            break;
        };
        search_root.push(literal);
    }
    search_root
}

fn literal_segment(segment: &str) -> Option<String> {
    let mut literal = String::with_capacity(segment.len());
    let mut escaped = false;
    for ch in segment.chars() {
        if escaped {
            literal.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if matches!(ch, '*' | '?' | '[') {
            return None;
        } else {
            literal.push(ch);
        }
    }
    if escaped {
        literal.push('\\');
    }
    Some(literal)
}

/// Mirrors `getSearchPaths`: negative patterns do not add roots, duplicates
/// and roots beneath any other positive root are removed, and surviving
/// roots retain pattern order.
/// https://github.com/actions/toolkit/blob/main/packages/glob/src/internal-pattern-helper.ts
fn merged_search_roots(patterns: &[CompiledPattern]) -> Vec<PathBuf> {
    let candidates: HashSet<PathBuf> = patterns
        .iter()
        .filter(|pattern| !pattern.negated)
        .filter_map(|pattern| pattern.search_root.clone())
        .collect();
    let mut included = HashSet::new();
    let mut roots = Vec::new();
    for root in patterns
        .iter()
        .filter(|pattern| !pattern.negated)
        .filter_map(|pattern| pattern.search_root.as_ref())
    {
        if included.contains(root) {
            continue;
        }
        let has_ancestor = root
            .ancestors()
            .skip(1)
            .any(|ancestor| candidates.contains(ancestor));
        if !has_ancestor {
            roots.push(root.clone());
            included.insert(root.clone());
        }
    }
    roots
}

enum WalkItem {
    Visit(PathBuf, EntryKind),
    LeaveDirectory(PathBuf),
}

/// Traverses one toolkit-derived search root in its native directory-read
/// order. Symbolic links use lstat-like entry kinds: with following off, a
/// link to a file is still yielded while a link to a directory is not
/// traversed. With following on, canonical directories in the active DFS
/// chain detect cycles without imposing an arbitrary depth ceiling.
/// https://github.com/actions/toolkit/blob/main/packages/glob/src/internal-globber.ts
fn walk_search_root(
    fs: &dyn HashFilesFs,
    root: &Path,
    follow_symlinks: bool,
    out: &mut Vec<PathBuf>,
    symlink_sourced: &mut HashSet<PathBuf>,
) -> Result<(), HashFilesError> {
    let root_kind = match fs.entry_kind(root) {
        Ok(kind) => kind,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(HashFilesError::Io {
                path: root.display().to_string(),
                source,
            });
        }
    };
    let mut stack = vec![WalkItem::Visit(root.to_path_buf(), root_kind)];
    let mut active_directories = HashSet::new();
    while let Some(item) = stack.pop() {
        match item {
            WalkItem::LeaveDirectory(canonical) => {
                active_directories.remove(&canonical);
            }
            WalkItem::Visit(path, EntryKind::File) => out.push(path),
            WalkItem::Visit(path, EntryKind::Dir) => {
                let canonical = fs
                    .canonicalize(&path)
                    .map_err(|source| HashFilesError::Io {
                        path: path.display().to_string(),
                        source,
                    })?;
                if !active_directories.insert(canonical.clone()) {
                    continue;
                }
                let entries = fs.read_dir(&path).map_err(|source| HashFilesError::Io {
                    path: path.display().to_string(),
                    source,
                })?;
                stack.push(WalkItem::LeaveDirectory(canonical));
                for entry in entries.into_iter().rev() {
                    stack.push(WalkItem::Visit(path.join(entry.name), entry.kind));
                }
            }
            WalkItem::Visit(path, EntryKind::Symlink) => match fs.read_dir(&path) {
                Ok(_) if follow_symlinks => {
                    stack.push(WalkItem::Visit(path, EntryKind::Dir));
                }
                Ok(_) => {}
                Err(_) => {
                    // lstat exposes the link itself as a non-directory
                    // match. The hash script's later file read follows a
                    // file link; a broken link is omitted there.
                    symlink_sourced.insert(path.clone());
                    out.push(path);
                }
            },
        }
    }
    Ok(())
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// Evaluates `hashFiles(args…)` against `fs`. `args` are the already
/// `ToString`-converted function arguments (see the design memo §3.8:
/// "1-255 string arguments (each evaluated then ToString)" — there is no
/// documented laziness for `hashFiles`, unlike `format`/`join`).
pub(crate) fn hash_files(args: &[String], fs: &dyn HashFilesFs) -> Result<Value, HashFilesError> {
    let mut follow_symlinks = false;
    let mut pattern_args = args;
    if let Some(first) = args.first()
        && first.starts_with("--")
    {
        if first.eq_ignore_ascii_case("--follow-symbolic-links") {
            follow_symlinks = true;
            pattern_args = &args[1..];
        } else {
            return Err(HashFilesError::InvalidOption(first.clone()));
        }
    }

    let joined = pattern_args.join("\n");
    let mut compiled = Vec::new();
    for raw_line in joined.lines() {
        let line = raw_line.trim();
        // Pattern list: blank lines and `#`-prefixed comment lines are
        // skipped (design memo §3.8).
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Leading `!` negates; multiple leading `!` toggle.
        let negated_count = line.chars().take_while(|c| *c == '!').count();
        let negated = negated_count % 2 == 1;
        let rest = &line[negated_count..];
        let (root_kind, stripped) = strip_root_prefix(rest);
        validate_no_dot_segments(&stripped, line)?;
        let root_path = match root_kind {
            RootKind::Workspace => Some(fs.workspace_root()),
            RootKind::Home => fs.home_dir(),
        };
        let Some(root_path) = root_path else {
            // A home-rooted pattern cannot match when no home directory is
            // available; it must not accidentally fall back to workspace.
            continue;
        };
        let rooted = if stripped.is_empty() {
            root_path.to_path_buf()
        } else {
            root_path.join(&stripped)
        };
        let matchers = compile_matchers(&rooted.to_string_lossy())?;
        compiled.push(CompiledPattern {
            negated,
            matchers,
            search_root: (!negated).then(|| derive_search_root(root_path, &stripped)),
        });
    }

    if compiled.is_empty() {
        return Ok(Value::String(String::new()));
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut symlink_sourced: HashSet<PathBuf> = HashSet::new();
    // The toolkit derives literal roots from positive patterns and visits
    // those roots in pattern order. This order is observable because the
    // outer hash consumes per-file digests in generator order.
    // https://github.com/actions/toolkit/blob/main/packages/glob/src/internal-pattern-helper.ts
    // https://github.com/actions/toolkit/blob/main/packages/glob/src/internal-globber.ts
    for root in merged_search_roots(&compiled) {
        walk_search_root(
            fs,
            &root,
            follow_symlinks,
            &mut candidates,
            &mut symlink_sourced,
        )?;
    }
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();
    candidates.retain(|p| seen_paths.insert(p.clone()));

    // "each `match` folds `result |= p.match` / `result &= ~p.match` in
    // pattern order" — non-negated patterns OR their matches in; each
    // negated pattern removes matches from the running set so far.
    let is_included = |path: &Path| {
        let path_str = path.to_string_lossy();
        let mut included = false;
        for pattern in &compiled {
            if pattern
                .matchers
                .iter()
                .any(|matcher| matcher.is_match(path_str.as_ref()))
            {
                included = !pattern.negated;
            }
        }
        included
    };

    let workspace_root = fs.workspace_root().to_path_buf();
    let ordered_paths: Vec<PathBuf> = candidates
        .into_iter()
        .filter(|path| is_included(path) && path.starts_with(&workspace_root))
        .collect();

    let mut valid_file_bytes: Vec<Vec<u8>> = Vec::with_capacity(ordered_paths.len());
    for path in &ordered_paths {
        match fs.read_file(path) {
            Ok(bytes) => valid_file_bytes.push(bytes),
            // "Broken symlinks are omitted" — but only for paths reached by
            // following a symlink; a genuine file that fails to read is a
            // real error.
            Err(_) if symlink_sourced.contains(path) => continue,
            Err(source) => {
                return Err(HashFilesError::Io {
                    path: path.display().to_string(),
                    source,
                });
            }
        }
    }

    // "Zero matches -> empty string (not an error; docs confirm)." This also
    // covers the case where every match turned out to be a broken symlink.
    if valid_file_bytes.is_empty() {
        return Ok(Value::String(String::new()));
    }

    // "SHA-256 of the file's raw bytes ... write the raw 32-byte digest into
    // an outer running SHA-256 ... final result = lowercase-hex digest of
    // the outer hash." (design memo §3.8 "Hashing").
    let mut outer = Sha256::new();
    for bytes in &valid_file_bytes {
        let inner = Sha256::digest(bytes);
        outer.update(inner.as_slice());
    }
    Ok(Value::String(to_hex(outer.finalize().as_slice())))
}

// In-memory filesystem fakes exercise the public parse/evaluate path at the
// true filesystem boundary, as permitted by TESTING.md.
#[cfg(test)]
pub(crate) mod test_support {
    use super::{DirEntry, EntryKind, HashFilesFs};
    use std::collections::{BTreeMap, HashSet};
    use std::io;
    use std::path::{Path, PathBuf};

    /// A fake with no files anywhere — used by tests that need a
    /// [`HashFilesFs`] to build a [`crate::context::Context`] but never
    /// call `hashFiles()`.
    #[derive(Debug)]
    pub struct NoFiles {
        root: PathBuf,
    }

    impl NoFiles {
        /// Builds a fake rooted at `root` with no files anywhere.
        pub fn new(root: impl Into<PathBuf>) -> Self {
            NoFiles { root: root.into() }
        }
    }

    impl HashFilesFs for NoFiles {
        fn workspace_root(&self) -> &Path {
            &self.root
        }
        fn read_dir(&self, _path: &Path) -> io::Result<Vec<DirEntry>> {
            Ok(Vec::new())
        }
        fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no such file: {}", path.display()),
            ))
        }
    }

    /// An in-memory tree: `files` maps a full path (as it would be joined
    /// from the root) to its byte content; directories are inferred from
    /// path prefixes. Preserves insertion order per directory, so tests can
    /// pin traversal-order-sensitive behavior deterministically.
    #[derive(Debug, Default)]
    pub struct InMemoryFs {
        root: PathBuf,
        home: Option<PathBuf>,
        files: Vec<(PathBuf, Vec<u8>)>,
        symlinks: BTreeMap<PathBuf, PathBuf>, // symlink path -> target path (target must be a file in `files`, or absent = broken)
    }

    impl InMemoryFs {
        /// Builds an empty in-memory tree rooted at `root`.
        pub fn new(root: impl Into<PathBuf>) -> Self {
            InMemoryFs {
                root: root.into(),
                home: None,
                files: Vec::new(),
                symlinks: BTreeMap::new(),
            }
        }

        /// Adds a file at `path` (relative to nothing in particular — tests
        /// pass full paths already joined under the configured root/home) in
        /// insertion order.
        pub fn with_file(mut self, path: impl Into<PathBuf>, content: impl Into<Vec<u8>>) -> Self {
            self.files.push((path.into(), content.into()));
            self
        }

        fn direct_children(&self, dir: &Path) -> Vec<DirEntry> {
            let mut seen_dirs = std::collections::BTreeSet::new();
            let mut out: Vec<DirEntry> = Vec::new();
            let push_unique = |name: String, kind: EntryKind, out: &mut Vec<DirEntry>| {
                if !out.iter().any(|e| e.name == name) {
                    out.push(DirEntry { name, kind });
                }
            };
            for (path, _) in &self.files {
                if let Ok(rest) = path.strip_prefix(dir) {
                    let mut components = rest.components();
                    if let Some(first) = components.next() {
                        let name = first.as_os_str().to_string_lossy().into_owned();
                        if components.next().is_some() {
                            if seen_dirs.insert(name.clone()) {
                                push_unique(name, EntryKind::Dir, &mut out);
                            }
                        } else {
                            push_unique(name, EntryKind::File, &mut out);
                        }
                    }
                }
            }
            for path in self.symlinks.keys() {
                if let Ok(rest) = path.strip_prefix(dir) {
                    let mut components = rest.components();
                    if let Some(first) = components.next()
                        && components.next().is_none()
                    {
                        let name = first.as_os_str().to_string_lossy().into_owned();
                        push_unique(name, EntryKind::Symlink, &mut out);
                    }
                }
            }
            out
        }

        fn is_directory(&self, path: &Path) -> bool {
            path == self.root
                || self.home.as_deref() == Some(path)
                || self.files.iter().any(|(candidate, _)| {
                    candidate
                        .strip_prefix(path)
                        .is_ok_and(|rest| !rest.as_os_str().is_empty())
                })
                || self.symlinks.keys().any(|candidate| {
                    candidate
                        .strip_prefix(path)
                        .is_ok_and(|rest| !rest.as_os_str().is_empty())
                })
        }

        /// Resolves *any* symlink prefix of `path` (not only an exact
        /// match), so e.g. a file discovered as `/work/link/a.txt` (where
        /// `/work/link` is a symlink to `/work/real`) reads back as
        /// `/work/real/a.txt`'s content — mirroring how a real filesystem
        /// resolves a symlink appearing anywhere in a path, not just as the
        /// final component. Repeated resolved paths detect cycles without a
        /// fixed iteration cutoff.
        fn resolve(&self, path: &Path) -> io::Result<PathBuf> {
            let mut current = path.to_path_buf();
            let mut seen = HashSet::new();
            loop {
                if !seen.insert(current.clone()) {
                    return Err(io::Error::other(format!(
                        "symbolic-link cycle at {}",
                        path.display()
                    )));
                }
                let mut next = None;
                for (link, target) in &self.symlinks {
                    if let Ok(rest) = current.strip_prefix(link) {
                        next = Some(if rest.as_os_str().is_empty() {
                            target.clone()
                        } else {
                            target.join(rest)
                        });
                        break;
                    }
                }
                match next {
                    Some(n) => current = n,
                    None => return Ok(current),
                }
            }
        }
    }

    impl HashFilesFs for InMemoryFs {
        fn workspace_root(&self) -> &Path {
            &self.root
        }
        fn home_dir(&self) -> Option<&Path> {
            self.home.as_deref()
        }
        fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
            let resolved = self.resolve(path)?;
            if !self.is_directory(&resolved) {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!("not a directory: {}", path.display()),
                ));
            }
            Ok(self.direct_children(&resolved))
        }
        fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
            let resolved = self.resolve(path)?;
            self.files
                .iter()
                .find(|(p, _)| *p == resolved)
                .map(|(_, content)| content.clone())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("no such file: {}", path.display()),
                    )
                })
        }
        fn entry_kind(&self, path: &Path) -> io::Result<EntryKind> {
            if self.symlinks.contains_key(path) {
                return Ok(EntryKind::Symlink);
            }
            let resolved = self.resolve(path)?;
            if self
                .files
                .iter()
                .any(|(candidate, _)| *candidate == resolved)
            {
                Ok(EntryKind::File)
            } else if self.is_directory(&resolved) {
                Ok(EntryKind::Dir)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no such path: {}", path.display()),
                ))
            }
        }
        fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
            self.resolve(path)
        }
    }
}
