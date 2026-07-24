//! Local variable resolution for `${{ vars.* }}`
//! (`greenlit-v0-spec.md` "Secrets/vars", `PHASE-1-engine-core.md`
//! greenlit-app section): CLI `--var` override, then a same-named process
//! environment variable, then `.litci/vars` (a dotenv file -- repository-
//! local state per `AGENTS.md`). No authenticated/network fallback exists
//! yet; that lands in Phase 3.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsString;

use greenlit_expr::Value;
use greenlit_workflow::Span;
use greenlit_workflow::extract::StaticExtraction;

mod dotenv;
mod remote;

pub(crate) use dotenv::read_dotenv_vars;
pub(crate) use remote::VariablesClient;

const MAX_VAR_VALUE_BYTES: usize = 48 * 1024;

/// One statically-referenced `vars.NAME` that could not be resolved from any
/// local source -- the fix `litci` reports names `--var`/`.litci/vars`
/// (`PHASE-1-engine-core.md`: "If a statically referenced variable is
/// unavailable, fail with an action to supply --var or .litci/vars").
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MissingVar {
    pub(crate) name: String,
    pub(crate) first_use: Span,
}

/// A literal `vars.NAME` reference whose name GitHub could never store.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InvalidVar {
    pub(crate) name: String,
    pub(crate) first_use: Span,
    pub(crate) reason: &'static str,
}

/// A local value that GitHub could not store as one configuration variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OversizedVar {
    pub(crate) name: String,
    pub(crate) bytes: usize,
}

/// Case variants in a case-sensitive host environment that would collapse
/// to one GitHub configuration-variable name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AmbiguousProcessVar {
    pub(crate) name: String,
    pub(crate) spellings: Vec<String>,
}

/// A selected process variable whose value cannot populate GitHub's string
/// `vars` context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NonUnicodeProcessVar {
    pub(crate) name: String,
    pub(crate) spelling: String,
}

/// All local-variable resolution failures for one workflow.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VarResolutionError {
    pub(crate) invalid: Vec<InvalidVar>,
    pub(crate) oversized: Vec<OversizedVar>,
    pub(crate) ambiguous_process: Vec<AmbiguousProcessVar>,
    pub(crate) non_unicode_process: Vec<NonUnicodeProcessVar>,
    pub(crate) missing: Vec<MissingVar>,
    pub(crate) dynamic_lookup: Option<Span>,
}

/// Re-exports the shared naming rule under this module's expected name —
/// see `crate::gh_names` for the cited GitHub documentation. Kept as a
/// distinct name here (rather than callers using `crate::gh_names` directly)
/// so `crate::vars::validate_name`'s existing call sites (`crate::cli`,
/// `crate::vars::dotenv`) needed no change when the implementation moved.
pub(crate) use crate::gh_names::validate_configuration_name as validate_name;

fn canonical(name: &str) -> String {
    name.to_ascii_uppercase()
}

/// Builds a case-insensitive, last-one-wins index for one ordered local
/// source. Indexing once prevents a repository-authored dotenv file from
/// making resolution rescan every earlier assignment for every name.
fn index_last(entries: &[(String, String)]) -> HashMap<String, &str> {
    let mut index = HashMap::with_capacity(entries.len());
    for (name, value) in entries {
        index.insert(canonical(name), value.as_str());
    }
    index
}

/// Resolves one name against the CLI > process-env > `.litci/vars` chain.
enum ProcessValue {
    Missing,
    Value(String),
    Ambiguous(Vec<String>),
    NonUnicode(String),
}

/// Scans the process environment once and indexes only names that another
/// local source or the workflow made relevant. Multiple Linux spellings that
/// collapse under GitHub's case-insensitive rules remain an explicit error.
fn index_process_values(candidate_names: &HashSet<&str>) -> HashMap<String, ProcessValue> {
    let mut matches: HashMap<String, Vec<(String, OsString)>> = HashMap::new();
    for (key, value) in std::env::vars_os() {
        let Ok(spelling) = key.into_string() else {
            continue;
        };
        let name = canonical(&spelling);
        if candidate_names.contains(name.as_str()) {
            matches.entry(name).or_default().push((spelling, value));
        }
    }

    let mut index = HashMap::with_capacity(matches.len());
    for (name, mut values) in matches {
        values.sort_by(|left, right| left.0.cmp(&right.0));
        let result = if values.len() == 1 {
            let Some((spelling, value)) = values.pop() else {
                continue;
            };
            match value.into_string() {
                Ok(value) => ProcessValue::Value(value),
                Err(_) => ProcessValue::NonUnicode(spelling),
            }
        } else {
            ProcessValue::Ambiguous(values.into_iter().map(|(spelling, _)| spelling).collect())
        };
        index.insert(name, result);
    }
    index
}

fn resolve_one(
    name: &str,
    cli: &HashMap<String, &str>,
    process: &mut HashMap<String, ProcessValue>,
    dotenv: &HashMap<String, &str>,
) -> ProcessValue {
    if let Some(value) = cli.get(name) {
        return ProcessValue::Value((*value).to_string());
    }
    if let Some(value) = process.remove(name) {
        return value;
    }
    dotenv.get(name).map_or(ProcessValue::Missing, |value| {
        ProcessValue::Value((*value).to_string())
    })
}

/// Builds the `vars` context [`Value`] [`greenlit_engine::PlanOptions`]
/// needs, and validates that every statically-referenced literal
/// `vars.NAME` resolves.
///
/// The returned namespace contains literal referenced names plus keys named
/// by `--var`/`.litci/vars`. Process environment values may override only
/// those known names; Greenlit scans the ambient environment solely to find
/// case-insensitive matches and never imports unrelated keys.
/// A dynamic lookup requires an explicitly complete local map: at least one
/// `--var`, or an existing `.litci/vars` file (an empty file explicitly
/// declares an empty complete map).
pub(crate) fn resolve_vars(
    extraction: &StaticExtraction,
    cli: &[(String, String)],
    dotenv: &[(String, String)],
    dotenv_file_exists: bool,
) -> Result<Value, Box<VarResolutionError>> {
    // GitHub stores configuration-variable names uppercase and resolves
    // references without regard to case. Canonicalizing the local map makes
    // differently-cased CLI/dotenv spellings one logical last-one-wins key.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/variables#naming-conventions-for-configuration-variables
    let mut invalid = Vec::new();
    let mut extracted = HashMap::with_capacity(extraction.vars.len());
    for (name, spans) in &extraction.vars {
        match validate_name(name) {
            Ok(()) => {
                extracted
                    .entry(canonical(name))
                    .or_insert_with(|| (name.as_str(), spans.first()));
            }
            Err(reason) => {
                if let Some(first_use) = spans.first() {
                    invalid.push(InvalidVar {
                        name: name.clone(),
                        first_use: first_use.clone(),
                        reason,
                    });
                }
            }
        }
    }
    let cli = index_last(cli);
    let dotenv = index_last(dotenv);
    let mut candidate_names = BTreeSet::new();
    candidate_names.extend(extracted.keys().cloned());
    candidate_names.extend(dotenv.keys().cloned());
    candidate_names.extend(cli.keys().cloned());
    let candidate_lookup: HashSet<&str> = candidate_names.iter().map(String::as_str).collect();
    let mut process = index_process_values(&candidate_lookup);

    let mut entries = Vec::with_capacity(candidate_names.len());
    let mut oversized = Vec::new();
    let mut ambiguous_process = Vec::new();
    let mut non_unicode_process = Vec::new();
    let mut missing = Vec::new();
    for name in &candidate_names {
        match resolve_one(name, &cli, &mut process, &dotenv) {
            ProcessValue::Value(value) if value.len() > MAX_VAR_VALUE_BYTES => {
                // GitHub limits every configuration-variable value to 48
                // KiB. `String::len` is its UTF-8 byte size, matching the
                // storage boundary rather than Unicode scalar count.
                // https://docs.github.com/en/actions/reference/workflows-and-actions/variables#limits-for-configuration-variables
                oversized.push(OversizedVar {
                    name: name.clone(),
                    bytes: value.len(),
                });
            }
            ProcessValue::Value(value) => {
                entries.push((name.clone(), Value::String(value)));
            }
            ProcessValue::Ambiguous(spellings) => {
                ambiguous_process.push(AmbiguousProcessVar {
                    name: name.clone(),
                    spellings,
                });
            }
            ProcessValue::NonUnicode(spelling) => {
                non_unicode_process.push(NonUnicodeProcessVar {
                    name: name.clone(),
                    spelling,
                });
            }
            ProcessValue::Missing => {
                // A name sourced only from `.litci/vars`/`--var` always
                // resolves by construction (see `resolve_one`), so this arm
                // is only reachable for a literal-reference-only name.
                if let Some((spelling, Some(first_use))) = extracted.get(name) {
                    missing.push(MissingVar {
                        name: (*spelling).to_string(),
                        first_use: (*first_use).clone(),
                    });
                }
            }
        }
    }

    let dynamic_lookup =
        if extraction.has_dynamic_vars_lookup && cli.is_empty() && !dotenv_file_exists {
            extraction.dynamic_vars.first().cloned()
        } else {
            None
        };

    if invalid.is_empty()
        && oversized.is_empty()
        && ambiguous_process.is_empty()
        && non_unicode_process.is_empty()
        && missing.is_empty()
        && dynamic_lookup.is_none()
    {
        Ok(Value::object(entries))
    } else {
        Err(Box::new(VarResolutionError {
            invalid,
            oversized,
            ambiguous_process,
            non_unicode_process,
            missing,
            dynamic_lookup,
        }))
    }
}

/// Internal test-only override for the GitHub REST API base URL (real
/// `https://api.github.com` in production) — see `crate::auth`'s identical
/// convention for its own base-URL override.
const TEST_API_BASE_URL_ENV: &str = "LITCI_TEST_GITHUB_API_BASE_URL";

fn api_base_url() -> String {
    std::env::var(TEST_API_BASE_URL_ENV)
        .unwrap_or_else(|_| remote::DEFAULT_API_BASE_URL.to_string())
}

/// The outcome of resolving `vars` with the authenticated remote leg
/// available. `PHASE-3-actions.md` Variables: local values override remote
/// values; a still-unresolved name with no authentication stops before
/// engine detection naming `litci auth`; a successful authenticated lookup
/// resolves an absent name to the empty string, matching GitHub.
pub(crate) enum VarsOutcome {
    /// Every referenced name resolved (locally, or with the remote leg).
    Resolved(Value),
    /// A local value violates GitHub's own storage rules (invalid name,
    /// oversized value, ambiguous/non-Unicode process environment) — no
    /// remote lookup could ever fix this, so it was never attempted.
    LocalError(Box<VarResolutionError>),
    /// Some names remain unresolved locally and no authentication is
    /// configured (`crate::auth::current_token` returned `None` or failed).
    AuthRequired(Box<VarResolutionError>),
    /// Authentication is configured, but the remote lookup itself failed
    /// (a permission shortfall or another API/transport error). Message
    /// already includes its one fix action.
    RemoteError(String),
}

/// Resolves `vars` through the complete v0 chain: `resolve_vars`'s local
/// chain first, then (only when the *only* local problem is an unresolved
/// literal name or a dynamic lookup — never for a hard local error) the
/// authenticated GitHub configuration-variables API, with repository
/// values overriding organization values.
///
/// The remote leg is layered in as an additional local source ranked
/// beneath `.litci/vars` (never above it — "local values override remote
/// values") by re-running the exact same local resolution with the
/// remote-fetched entries appended to the dotenv list; this is what lets a
/// name GitHub reports as absent resolve to the empty string on the second
/// pass rather than remaining "missing", and what lets a dynamic lookup's
/// fetched map satisfy `resolve_vars`'s "explicitly complete local map"
/// requirement without a second, parallel resolution implementation.
pub(crate) fn resolve_vars_with_remote(
    extraction: &StaticExtraction,
    cli: &[(String, String)],
    dotenv: &[(String, String)],
    dotenv_file_exists: bool,
    repo_owner: &str,
    repo_name: &str,
) -> VarsOutcome {
    let local_error = match resolve_vars(extraction, cli, dotenv, dotenv_file_exists) {
        Ok(value) => return VarsOutcome::Resolved(value),
        Err(error) => error,
    };
    let remote_could_help = local_error.invalid.is_empty()
        && local_error.oversized.is_empty()
        && local_error.ambiguous_process.is_empty()
        && local_error.non_unicode_process.is_empty()
        && (!local_error.missing.is_empty() || local_error.dynamic_lookup.is_some());
    if !remote_could_help {
        return VarsOutcome::LocalError(local_error);
    }
    let token = match crate::auth::current_token() {
        Ok(Some(token)) => token,
        Ok(None) | Err(_) => return VarsOutcome::AuthRequired(local_error),
    };

    let client = VariablesClient::with_base_url(token, api_base_url());
    let remote_entries = if local_error.dynamic_lookup.is_some() {
        fetch_complete_map(&client, repo_owner, repo_name)
    } else {
        fetch_named(&client, repo_owner, repo_name, &local_error.missing)
    };
    let remote_entries = match remote_entries {
        Ok(entries) => entries,
        Err(message) => return VarsOutcome::RemoteError(message),
    };

    let mut combined_dotenv = dotenv.to_vec();
    combined_dotenv.extend(remote_entries);
    match resolve_vars(extraction, cli, &combined_dotenv, true) {
        Ok(value) => VarsOutcome::Resolved(value),
        // Every name that could have been fixed by the remote leg was
        // either given a real value or an explicit empty-string entry
        // above, so a second failure here can only be a hard local error
        // (or, in principle, a name the remote leg does not cover at all —
        // neither happens by construction) and is reported unchanged.
        Err(error) => VarsOutcome::LocalError(error),
    }
}

/// Fetches the complete repository ∪ organization variable map for a
/// dynamic `vars[...]` lookup, repository values overriding organization
/// values (`PHASE-3-actions.md` Variables: "repository values override
/// organization values").
fn fetch_complete_map(
    client: &VariablesClient,
    owner: &str,
    repo: &str,
) -> Result<Vec<(String, String)>, String> {
    let mut merged = client
        .list_org_variables(owner)
        .map_err(|error| error.to_message_with_fix())?;
    let repo_vars = client
        .list_repo_variables(owner, repo)
        .map_err(|error| error.to_message_with_fix())?;
    merged.extend(repo_vars);
    Ok(merged.into_iter().collect())
}

/// Fetches each literal missing name individually: repository value if
/// present, else organization value, else an explicit empty string (GitHub
/// parity: "a name absent from GitHub resolves to the empty string").
fn fetch_named(
    client: &VariablesClient,
    owner: &str,
    repo: &str,
    missing: &[MissingVar],
) -> Result<Vec<(String, String)>, String> {
    let mut entries = Vec::with_capacity(missing.len());
    for var in missing {
        let repo_value = client
            .get_repo_variable(owner, repo, &var.name)
            .map_err(|error| error.to_message_with_fix())?;
        let value = match repo_value {
            Some(value) => value,
            None => client
                .get_org_variable(owner, &var.name)
                .map_err(|error| error.to_message_with_fix())?
                .unwrap_or_default(),
        };
        entries.push((var.name.clone(), value));
    }
    Ok(entries)
}
