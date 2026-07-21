//! Local variable resolution for `${{ vars.* }}`
//! (`greenlit-v0-spec.md` "Secrets/vars", `PHASE-1-engine-core.md`
//! greenlit-app section): CLI `--var` override, then a same-named process
//! environment variable, then `.litci/vars` (a dotenv file -- repository-
//! local state per `AGENTS.md`). No authenticated/network fallback exists
//! yet; that lands in Phase 3.

use std::path::{Path, PathBuf};

use greenlit_expr::Value;
use greenlit_workflow::Span;
use greenlit_workflow::extract::StaticExtraction;

/// Where `.litci/vars` lives, relative to the current working directory --
/// repository-local state (`AGENTS.md`: "Repository-local state | `.litci/`").
pub(crate) fn default_vars_file(cwd: &Path) -> PathBuf {
    cwd.join(".litci").join("vars")
}

/// One statically-referenced `vars.NAME` that could not be resolved from any
/// local source -- the fix `litci` reports names `--var`/`.litci/vars`
/// (`PHASE-1-engine-core.md`: "If a statically referenced variable is
/// unavailable, fail with an action to supply --var or .litci/vars").
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MissingVar {
    pub(crate) name: String,
    pub(crate) first_use: Span,
}

/// Reads `.litci/vars` as a flat `KEY=VALUE` dotenv file, if present.
///
/// A missing file is empty history, not an error -- `.litci/vars` is
/// optional local configuration (mirrors
/// `greenlit_metrics::MetricsStore::read_all`'s "file absent => empty"
/// convention). Uses `dotenvy::from_path_iter`, which only *yields* parsed
/// pairs rather than mutating the real process environment -- required so
/// this crate can keep ".litci/vars" and "process environment" as two
/// distinct sources in the precedence chain.
pub(crate) fn read_dotenv_vars(path: &Path) -> Result<Vec<(String, String)>, String> {
    let iter = match dotenvy::from_path_iter(path) {
        Ok(iter) => iter,
        Err(e) if e.not_found() => return Ok(Vec::new()),
        Err(e) => {
            return Err(format!(
                "{}: could not read local variables file: {e}\n  fix: fix the KEY=VALUE syntax in {}",
                path.display(),
                path.display()
            ));
        }
    };
    let mut entries = Vec::new();
    for item in iter {
        let (key, value) = item.map_err(|e| {
            format!(
                "{}: could not parse local variables file: {e}\n  fix: fix the KEY=VALUE syntax in {}",
                path.display(),
                path.display()
            )
        })?;
        entries.push((key, value));
    }
    Ok(entries)
}

/// The last entry named `name` in `entries` (a repeated `--var`/dotenv key
/// is last-one-wins, matching how `--var` flags and typical dotenv readers
/// behave).
fn find_last<'a>(entries: &'a [(String, String)], name: &str) -> Option<&'a str> {
    entries
        .iter()
        .rev()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// Resolves one name against the CLI > process-env > `.litci/vars` chain.
fn resolve_one(
    name: &str,
    cli: &[(String, String)],
    dotenv: &[(String, String)],
) -> Option<String> {
    find_last(cli, name)
        .map(str::to_string)
        .or_else(|| std::env::var(name).ok())
        .or_else(|| find_last(dotenv, name).map(str::to_string))
}

/// Builds the `vars` context [`Value`] [`greenlit_engine::PlanOptions`]
/// needs, and validates that every statically-referenced literal
/// `vars.NAME` resolves.
///
/// The returned map covers every name this crate can discover locally:
/// every literal `vars.*` reference the workflow makes, plus every
/// `--var`/`.litci/vars` entry the user supplied even if the workflow never
/// names it. That superset is what makes a *dynamic* `vars[...]` lookup
/// resolve to anything at all in this no-auth phase
/// (`PHASE-1-engine-core.md`: "Dynamic vars[...] requires a complete
/// locally supplied map in this phase") -- Greenlit cannot know which name a
/// dynamic lookup will probe at plan time, so the best it can do is hand the
/// evaluator everything the user locally supplied; a name outside that set
/// falls through the evaluator's own documented unknown-key rule (resolves
/// to an empty string) rather than erroring.
pub(crate) fn resolve_vars(
    extraction: &StaticExtraction,
    cli: &[(String, String)],
    dotenv: &[(String, String)],
) -> Result<Value, Vec<MissingVar>> {
    let mut candidate_names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    candidate_names.extend(extraction.vars.keys().cloned());
    candidate_names.extend(dotenv.iter().map(|(k, _)| k.clone()));
    candidate_names.extend(cli.iter().map(|(k, _)| k.clone()));

    let mut entries = Vec::with_capacity(candidate_names.len());
    let mut missing = Vec::new();
    for name in &candidate_names {
        match resolve_one(name, cli, dotenv) {
            Some(value) => entries.push((name.clone(), Value::String(value))),
            None => {
                // A name sourced only from `.litci/vars`/`--var` always
                // resolves by construction (see `resolve_one`), so this arm
                // is only reachable for a literal-reference-only name.
                if let Some(first_use) = extraction.vars.get(name).and_then(|spans| spans.first()) {
                    missing.push(MissingVar {
                        name: name.clone(),
                        first_use: first_use.clone(),
                    });
                }
            }
        }
    }

    if missing.is_empty() {
        Ok(Value::object(entries))
    } else {
        Err(missing)
    }
}
