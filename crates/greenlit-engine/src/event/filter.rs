//! GitHub webhook trigger-filter evaluation for synthetic local events.

use regex::{Regex, RegexSet};

use greenlit_workflow::Spanned;
use greenlit_workflow::model::trigger::{Trigger, WebhookFilter};

use super::{EventError, EventKind};
use crate::git::GitContext;

const MAX_FILTERED_CHANGED_PATHS: usize = 3_000;
const MAX_PATTERNS_PER_SET: usize = 128;

struct PreparedPattern {
    regex_source: String,
    negative: bool,
    span: greenlit_workflow::Span,
    source: String,
}

struct CompiledPatterns {
    chunks: Vec<CompiledPatternChunk>,
}

struct CompiledPatternChunk {
    matcher: PatternMatcher,
    negative: Vec<bool>,
}

enum PatternMatcher {
    Set(RegexSet),
    Single(Regex),
}

impl CompiledPatterns {
    fn any_match(&self, value: &str) -> bool {
        self.chunks.iter().any(|chunk| chunk.is_match(value))
    }

    fn ordered_match(&self, value: &str) -> bool {
        // Ordered filters are equivalent to the polarity of the final
        // matching pattern. Search chunks and their match indices in reverse
        // author order rather than invoking every regex individually.
        self.chunks
            .iter()
            .rev()
            .find_map(|chunk| {
                chunk
                    .last_match(value)
                    .and_then(|index| chunk.negative.get(index).map(|negative| !negative))
            })
            .unwrap_or(false)
    }
}

impl CompiledPatternChunk {
    fn is_match(&self, value: &str) -> bool {
        match &self.matcher {
            PatternMatcher::Set(patterns) => patterns.is_match(value),
            PatternMatcher::Single(pattern) => pattern.is_match(value),
        }
    }

    fn last_match(&self, value: &str) -> Option<usize> {
        match &self.matcher {
            PatternMatcher::Set(patterns) => patterns.matches(value).iter().next_back(),
            PatternMatcher::Single(pattern) => pattern.is_match(value).then_some(0),
        }
    }
}

/// Evaluates the branch, path, and activity-type filters GitHub applies
/// before creating a run. Filter axes combine with AND; ordered positive
/// lists support `!` exclusion and later re-inclusion.
/// https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#using-filters
pub(super) fn webhook_filter_matches(
    trigger: &Trigger,
    kind: EventKind,
    git: &GitContext,
) -> Result<bool, EventError> {
    let Trigger::Webhook { filter, .. } = trigger else {
        return Ok(true);
    };

    let ref_matches = match kind {
        EventKind::Push => push_branch_matches(filter, &git.branch)?,
        // GitHub's pull-request branch filter is evaluated against the base
        // branch, not the topic branch checked out at `HEAD`.
        // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#onpull_requestpull_request_targetbranchesbranches-ignore
        EventKind::PullRequest => ref_filter_matches(
            &filter.branches,
            &filter.branches_ignore,
            &git.pull_request_base_branch,
            "branches",
        )?,
        EventKind::WorkflowDispatch => true,
    };
    if !ref_matches {
        return Ok(false);
    }

    if kind == EventKind::PullRequest
        && !filter.types.is_empty()
        && !filter.types.iter().any(|kind| kind.value == "opened")
    {
        return Ok(false);
    }

    let changed_paths = match kind {
        EventKind::PullRequest => &git.pull_request_changed_paths,
        EventKind::Push | EventKind::WorkflowDispatch => &git.changed_paths,
    };
    path_filter_matches(filter, changed_paths)
}

fn push_branch_matches(filter: &WebhookFilter, branch: &str) -> Result<bool, EventError> {
    // A local synthetic push always targets a branch. GitHub suppresses
    // branch pushes when only tag filters are defined.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#onpushbranchestagsbranches-ignoretags-ignore
    if filter.branches.is_empty()
        && filter.branches_ignore.is_empty()
        && (!filter.tags.is_empty() || !filter.tags_ignore.is_empty())
    {
        return Ok(false);
    }
    ref_filter_matches(
        &filter.branches,
        &filter.branches_ignore,
        branch,
        "branches",
    )
}

fn ref_filter_matches(
    included: &[Spanned<String>],
    ignored: &[Spanned<String>],
    value: &str,
    axis: &'static str,
) -> Result<bool, EventError> {
    if !included.is_empty() {
        return ordered_match(included, value, axis);
    }
    if compile_patterns(ignored, axis, false)?.any_match(value) {
        return Ok(false);
    }
    Ok(true)
}

fn path_filter_matches(
    filter: &WebhookFilter,
    changed_paths: &[String],
) -> Result<bool, EventError> {
    // GitHub does not run a path-filtered workflow when the comparison has
    // no changed files. A positive list runs if at least one path remains
    // included; paths-ignore skips only when every path is ignored.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#onpushpull_requestpull_request_targetpathspaths-ignore
    // GitHub generates path-filter comparisons from at most the first 3,000
    // changed files; a later matching file therefore cannot start a run.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#git-diff-comparisons
    let filtered_paths = changed_paths.iter().take(MAX_FILTERED_CHANGED_PATHS);
    if !filter.paths.is_empty() {
        let patterns = compile_patterns(&filter.paths, "paths", true)?;
        for path in filtered_paths {
            if patterns.ordered_match(path) {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    if !filter.paths_ignore.is_empty() {
        if changed_paths.is_empty() {
            return Ok(false);
        }
        let patterns = compile_patterns(&filter.paths_ignore, "paths-ignore", false)?;
        for path in filtered_paths {
            if !patterns.any_match(path) {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    Ok(true)
}

fn ordered_match(
    patterns: &[Spanned<String>],
    value: &str,
    axis: &'static str,
) -> Result<bool, EventError> {
    let patterns = compile_patterns(patterns, axis, true)?;
    Ok(patterns.ordered_match(value))
}

fn compile_patterns(
    patterns: &[Spanned<String>],
    axis: &'static str,
    allow_negative: bool,
) -> Result<CompiledPatterns, EventError> {
    let prepared = patterns
        .iter()
        .map(|pattern| {
            let negative = allow_negative && pattern.value.starts_with('!');
            let source = if negative {
                pattern.value.strip_prefix('!').unwrap_or(&pattern.value)
            } else {
                &pattern.value
            };
            let regex_source =
                glob_to_regex(source).map_err(|message| EventError::InvalidFilterPattern {
                    span: pattern.span.clone(),
                    axis,
                    pattern: source.to_string(),
                    message,
                })?;
            Ok::<PreparedPattern, EventError>(PreparedPattern {
                regex_source,
                negative,
                span: pattern.span.clone(),
                source: source.to_string(),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut chunks = Vec::new();
    for patterns in prepared.chunks(MAX_PATTERNS_PER_SET) {
        compile_pattern_chunk(patterns, axis, &mut chunks)?;
    }
    Ok(CompiledPatterns { chunks })
}

fn compile_pattern_chunk(
    patterns: &[PreparedPattern],
    axis: &'static str,
    compiled: &mut Vec<CompiledPatternChunk>,
) -> Result<(), EventError> {
    if patterns.len() > 1 {
        let sources = patterns.iter().map(|pattern| pattern.regex_source.as_str());
        if let Ok(matcher) = RegexSet::new(sources) {
            compiled.push(CompiledPatternChunk {
                matcher: PatternMatcher::Set(matcher),
                negative: patterns.iter().map(|pattern| pattern.negative).collect(),
            });
            return Ok(());
        }

        // A set can exceed the regex crate's compiled-size budget even when
        // each authored pattern is valid. Split until it compiles, retaining
        // the same accepted pattern language and declaration order.
        let midpoint = patterns.len() / 2;
        compile_pattern_chunk(&patterns[..midpoint], axis, compiled)?;
        compile_pattern_chunk(&patterns[midpoint..], axis, compiled)?;
        return Ok(());
    }

    let Some(pattern) = patterns.first() else {
        return Ok(());
    };
    let matcher =
        Regex::new(&pattern.regex_source).map_err(|error| EventError::InvalidFilterPattern {
            span: pattern.span.clone(),
            axis,
            pattern: pattern.source.clone(),
            message: error.to_string(),
        })?;
    compiled.push(CompiledPatternChunk {
        matcher: PatternMatcher::Single(matcher),
        negative: vec![pattern.negative],
    });
    Ok(())
}

fn glob_to_regex(pattern: &str) -> Result<String, String> {
    let mut chars = pattern.chars().peekable();
    let mut out = String::from("^");
    let mut has_atom = false;
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                let escaped = chars.next().ok_or_else(|| {
                    "a trailing backslash does not escape a character".to_string()
                })?;
                out.push_str(&regex::escape(&escaped.to_string()));
                has_atom = true;
            }
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                // GitHub's filter grammar makes `**/` span zero or more
                // complete directory segments: `**/README.md` includes the
                // root file, and `docs/**/*.md` includes direct children.
                // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#filter-pattern-cheat-sheet
                if chars.peek() == Some(&'/') {
                    chars.next();
                    out.push_str("(?:.*/)?");
                } else {
                    out.push_str(".*");
                }
                has_atom = true;
            }
            '*' => {
                out.push_str("[^/]*");
                has_atom = true;
            }
            '?' | '+' => {
                if !has_atom {
                    return Err(format!("'{ch}' must follow a character or character class"));
                }
                out.push(ch);
                has_atom = false;
            }
            '[' => {
                let mut class = String::new();
                let mut closed = false;
                for item in chars.by_ref() {
                    if item == ']' {
                        closed = true;
                        break;
                    }
                    class.push(item);
                }
                if !closed
                    || class.is_empty()
                    || !class
                        .chars()
                        .all(|item| item.is_ascii_alphanumeric() || item == '-')
                {
                    return Err(
                        "character classes may contain only ASCII letters, digits, and ranges"
                            .to_string(),
                    );
                }
                out.push('[');
                out.push_str(&class);
                out.push(']');
                has_atom = true;
            }
            literal => {
                out.push_str(&regex::escape(&literal.to_string()));
                has_atom = true;
            }
        }
    }
    out.push('$');
    Ok(out)
}
