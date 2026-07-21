//! Pattern rooting, validation, compilation, and search-root selection.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use globset::GlobBuilder;

use super::{HashFilesError, HashFilesFs};

enum RootKind {
    Workspace,
    Home,
}

/// One pattern after rooting and glob compilation.
pub(super) struct CompiledPattern {
    negated: bool,
    matchers: Vec<globset::GlobMatcher>,
    search_root: Option<PathBuf>,
}

/// Parses and compiles the already string-converted function arguments.
pub(super) fn compile_patterns(
    args: &[String],
    fs: &dyn HashFilesFs,
) -> Result<Vec<CompiledPattern>, HashFilesError> {
    let joined = args.join("\n");
    let mut compiled = Vec::new();
    for raw_line in joined.lines() {
        let line = raw_line.trim();
        // Pattern list: blank lines and `#`-prefixed comment lines are
        // skipped, matching `@actions/glob` pattern parsing.
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Leading `!` negates; multiple leading `!` toggle.
        let negated_count = line
            .chars()
            .take_while(|character| *character == '!')
            .count();
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
        compiled.push(CompiledPattern {
            negated,
            matchers: compile_matchers(&rooted.to_string_lossy())?,
            search_root: (!negated).then(|| derive_search_root(root_path, &stripped)),
        });
    }
    Ok(compiled)
}

/// Splits a recognized leading `.`/`./`/`~`/`~/` rooting prefix off `pattern`,
/// or leaves it untouched (workspace-relative) otherwise.
///
/// The leading-`/` case is a deliberate, documented departure from the
/// toolkit implementation's literal
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
        // @actions/glob source treats `/` as filesystem-absolute. GitHub's
        // public workflow contract is authoritative for user-visible
        // behavior, and the expression oracle pins this workspace-rooted
        // interpretation.
        // https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#hashfiles
        // https://github.com/actions/toolkit/blob/main/packages/glob/src/internal-pattern.ts
        (RootKind::Workspace, rest.to_string())
    } else {
        (RootKind::Workspace, pattern.to_string())
    }
}

/// Rejects `.`/`..` path segments anywhere in `stripped` (the pattern with
/// its recognized rooting prefix already removed), matching the runner's
/// `hashFiles` validation in
/// <https://github.com/actions/runner/blob/main/src/Runner.Worker/Expressions/HashFilesFunction.cs>.
fn validate_no_dot_segments(stripped: &str, original: &str) -> Result<(), HashFilesError> {
    for segment in stripped.split('/') {
        if segment == "." || segment == ".." {
            return Err(HashFilesError::RelativePathingNotAllowed {
                pattern: original.to_string(),
            });
        }
    }
    Ok(())
}

/// Escapes `{`/`}` (that aren't already backslash-escaped) so that
/// `globset` — which supports brace alternation by default — treats them
/// literally, matching `@actions/glob`'s minimatch `nobrace: true` option.
fn escape_braces(pattern: &str) -> String {
    let mut escaped_pattern = String::with_capacity(pattern.len());
    let mut characters = pattern.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\\' {
            escaped_pattern.push(character);
            if let Some(next) = characters.next() {
                escaped_pattern.push(next);
            }
        } else if character == '{' || character == '}' {
            escaped_pattern.push('\\');
            escaped_pattern.push(character);
        } else {
            escaped_pattern.push(character);
        }
    }
    escaped_pattern
}

fn compile_one(pattern: &str) -> Result<globset::GlobMatcher, HashFilesError> {
    let mut builder = GlobBuilder::new(pattern);
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
        .map(|glob| glob.compile_matcher())
        .map_err(|source| HashFilesError::InvalidPattern {
            pattern: pattern.to_string(),
            source,
        })
}

/// Compiles one already-rooted pattern plus, unless it already ends in `**`,
/// the toolkit's implicit `/**` descendants variant. A trailing-slash
/// directory-only base pattern cannot match a file candidate, which gives
/// the documented "contributes nothing" behavior without a special case.
fn compile_matchers(pattern: &str) -> Result<Vec<globset::GlobMatcher>, HashFilesError> {
    let escaped = escape_braces(pattern);
    let mut variants = vec![compile_one(&escaped)?];
    let ends_with_doublestar = escaped.rsplit('/').next() == Some("**");
    if !ends_with_doublestar {
        let base = escaped.trim_end_matches('/');
        variants.push(compile_one(&format!("{base}/**"))?);
    }
    Ok(variants)
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
    for character in segment.chars() {
        if escaped {
            literal.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if matches!(character, '*' | '?' | '[') {
            return None;
        } else {
            literal.push(character);
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
pub(super) fn merged_search_roots(patterns: &[CompiledPattern]) -> Vec<PathBuf> {
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

/// Folds `result |= positive_match` and `result &= !negative_match` in
/// declaration order for one path.
pub(super) fn is_included(patterns: &[CompiledPattern], path: &Path) -> bool {
    let path = path.to_string_lossy();
    let mut included = false;
    for pattern in patterns {
        if pattern
            .matchers
            .iter()
            .any(|matcher| matcher.is_match(path.as_ref()))
        {
            included = !pattern.negated;
        }
    }
    included
}
