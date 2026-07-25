//! Local secret resolution for `${{ secrets.* }}`
//! (`greenlit-v0-spec.md` "Secrets/vars", `PHASE-3-actions.md` Secrets):
//! repeatable `-s KEY=VAL` → same-named process environment → `.litci/secrets`
//! (dotenv, `0600`, auto-gitignored) → an interactive no-echo prompt for
//! every referenced-but-still-unresolved name, offering to persist it.
//!
//! `secrets.GITHUB_TOKEN` is deliberately excluded from every candidate set
//! this module builds: GitHub reserves that exact name (secret names "must
//! not start with the `GITHUB_` prefix" — the same rule
//! [`validate_name`]/`crate::gh_names` enforces — so a real repository can
//! never have a *user-created* secret by that name either; it is instead
//! always the platform-populated token). `crate::auth::resolve_github_token`
//! is `secrets.GITHUB_TOKEN`'s and `github.token`'s own, separate resolution
//! path (`crate::run_cmd`), which this module's ordinary chain, prompting,
//! and persistence never touch.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::IsTerminal;
use std::path::Path;

use dialoguer::Confirm;
use dialoguer::Password;
use dialoguer::theme::ColorfulTheme;
use greenlit_workflow::Span;
use greenlit_workflow::extract::StaticExtraction;

mod dotenv;

pub(crate) use dotenv::read_dotenv_secrets;

/// Re-exports the shared naming rule under this module's expected name —
/// see `crate::gh_names` for the cited GitHub documentation.
pub(crate) use crate::gh_names::validate_configuration_name as validate_name;

/// GitHub's documented per-secret value size limit, identical to
/// configuration variables.
/// <https://docs.github.com/en/actions/reference/security/secrets>
const MAX_SECRET_VALUE_BYTES: usize = 48 * 1024;

/// The reserved name this module's ordinary chain never resolves — see the
/// module doc comment.
pub(crate) const GITHUB_TOKEN_NAME: &str = "GITHUB_TOKEN";

/// A literal `secrets.NAME` reference whose name GitHub could never store.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InvalidSecret {
    pub(crate) name: String,
    pub(crate) first_use: Span,
    pub(crate) reason: &'static str,
}

/// A local value that GitHub could not store as one secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OversizedSecret {
    pub(crate) name: String,
    pub(crate) bytes: usize,
}

/// Case variants in a case-sensitive host environment that would collapse
/// to one GitHub secret name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AmbiguousProcessSecret {
    pub(crate) name: String,
    pub(crate) spellings: Vec<String>,
}

/// A selected process variable whose value cannot populate GitHub's string
/// `secrets` context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NonUnicodeProcessSecret {
    pub(crate) name: String,
    pub(crate) spelling: String,
}

/// One referenced-but-unresolved secret name that prompting was not
/// possible for (`--no-input`, or no interactive terminal).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MissingSecret {
    pub(crate) name: String,
    pub(crate) first_use: Span,
}

/// All local-secret resolution failures for one workflow that are not
/// resolved by prompting.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct SecretResolutionError {
    pub(crate) invalid: Vec<InvalidSecret>,
    pub(crate) oversized: Vec<OversizedSecret>,
    pub(crate) ambiguous_process: Vec<AmbiguousProcessSecret>,
    pub(crate) non_unicode_process: Vec<NonUnicodeProcessSecret>,
    pub(crate) missing: Vec<MissingSecret>,
}

impl SecretResolutionError {
    fn is_empty(&self) -> bool {
        self.invalid.is_empty()
            && self.oversized.is_empty()
            && self.ambiguous_process.is_empty()
            && self.non_unicode_process.is_empty()
            && self.missing.is_empty()
    }
}

/// Everything [`resolve_secrets`] can fail with.
#[derive(Debug)]
pub(crate) enum SecretsError {
    /// A structured resolution failure (invalid/oversized/ambiguous/
    /// non-Unicode local value, or unresolved names when prompting could
    /// not run).
    Resolution(Box<SecretResolutionError>),
    /// The interactive prompt or the `.litci/secrets` save itself failed at
    /// the I/O level (a broken terminal mid-prompt, an unwritable
    /// filesystem) — kept distinct from [`SecretsError::Resolution`] rather
    /// than shoehorned into one of its structured fields, since it is not a
    /// value/name problem at all.
    Prompt(String),
}

/// The flat list of secrets actually resolved, ready for the caller
/// (`crate::run_cmd`) to build the final `secrets` context object and mask
/// list from — merged with the auth-derived `GITHUB_TOKEN` value this
/// module never produces itself (see the module doc comment).
#[derive(Debug)]
pub(crate) struct SecretsOutcome {
    pub(crate) resolved: Vec<(String, String)>,
}

fn canonical(name: &str) -> String {
    name.to_ascii_uppercase()
}

fn index_last(entries: &[(String, String)]) -> HashMap<String, &str> {
    let mut index = HashMap::with_capacity(entries.len());
    for (name, value) in entries {
        index.insert(canonical(name), value.as_str());
    }
    index
}

enum ProcessValue {
    Missing,
    Value(String),
    Ambiguous(Vec<String>),
    NonUnicode(String),
}

fn index_process_values(candidate_names: &HashSet<&str>) -> HashMap<String, ProcessValue> {
    let mut matches: HashMap<String, Vec<(String, std::ffi::OsString)>> = HashMap::new();
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

/// Resolves exactly one name (`crate::auth`'s `GITHUB_TOKEN` special case)
/// against the CLI/process-env/dotenv chain only — no name validation, no
/// prompting, no persistence. `crate::run_cmd` uses this to give a
/// `-s GITHUB_TOKEN=...`/env/`.litci/secrets` override precedence over the
/// stored auth token, mirroring every other "local overrides remote" rule
/// in this tool, without folding the reserved name into the ordinary
/// candidate/prompt machinery above.
pub(crate) fn local_override_for(
    name: &str,
    cli: &[(String, String)],
    dotenv: &[(String, String)],
) -> Option<String> {
    let canon = canonical(name);
    if let Some((_, value)) = cli.iter().rev().find(|(key, _)| canonical(key) == canon) {
        return Some(value.clone());
    }
    for (key, value) in std::env::vars_os() {
        let Ok(spelling) = key.into_string() else {
            continue;
        };
        if canonical(&spelling) == canon {
            return value.into_string().ok();
        }
    }
    dotenv
        .iter()
        .rev()
        .find(|(key, _)| canonical(key) == canon)
        .map(|(_, value)| value.clone())
}

/// Whether prompting for a missing secret may run at all: mirrors
/// `crate::workflow_picker::interactive`'s convention exactly (both stdin
/// and stderr must be real terminals) combined with `--no-input`. Both a
/// non-interactive invocation (piped/redirected stdin, e.g. CI or an
/// automated test) and an explicit `--no-input` fall back to the same
/// "fails fast, lists every missing name" behavior.
pub(crate) fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Resolves every literal `secrets.NAME` reference (except the reserved
/// `GITHUB_TOKEN`, see the module doc comment) through the local chain,
/// prompting for any name that remains unresolved when `no_input` is
/// `false` and [`interactive`] is `true`; a value the user supplies at the
/// prompt is offered for persistence to `.litci/secrets`.
///
/// # Errors
/// Returns a [`SecretResolutionError`] for any invalid/oversized/ambiguous
/// local value, or (when prompting did not run) every referenced name that
/// remains unresolved.
pub(crate) fn resolve_secrets(
    extraction: &StaticExtraction,
    cli: &[(String, String)],
    dotenv: &[(String, String)],
    repo_root: &Path,
    no_input: bool,
) -> Result<SecretsOutcome, SecretsError> {
    let mut error = SecretResolutionError::default();
    let mut extracted = HashMap::with_capacity(extraction.secrets.len());
    for (name, spans) in &extraction.secrets {
        if name.eq_ignore_ascii_case(GITHUB_TOKEN_NAME) {
            continue;
        }
        match validate_name(name) {
            Ok(()) => {
                extracted
                    .entry(canonical(name))
                    .or_insert_with(|| (name.as_str(), spans.first()));
            }
            Err(reason) => {
                if let Some(first_use) = spans.first() {
                    error.invalid.push(InvalidSecret {
                        name: name.clone(),
                        first_use: first_use.clone(),
                        reason,
                    });
                }
            }
        }
    }

    let cli_index = index_last(cli);
    let dotenv_index = index_last(dotenv);
    let mut candidate_names = BTreeSet::new();
    candidate_names.extend(extracted.keys().cloned());
    candidate_names.extend(dotenv_index.keys().cloned());
    candidate_names.extend(cli_index.keys().cloned());
    let candidate_lookup: HashSet<&str> = candidate_names.iter().map(String::as_str).collect();
    let mut process = index_process_values(&candidate_lookup);

    let mut resolved = Vec::with_capacity(candidate_names.len());
    let mut still_missing: Vec<(String, Span)> = Vec::new();
    for name in &candidate_names {
        match resolve_one(name, &cli_index, &mut process, &dotenv_index) {
            ProcessValue::Value(value) if value.len() > MAX_SECRET_VALUE_BYTES => {
                error.oversized.push(OversizedSecret {
                    name: name.clone(),
                    bytes: value.len(),
                });
            }
            ProcessValue::Value(value) => resolved.push((name.clone(), value)),
            ProcessValue::Ambiguous(spellings) => {
                error.ambiguous_process.push(AmbiguousProcessSecret {
                    name: name.clone(),
                    spellings,
                });
            }
            ProcessValue::NonUnicode(spelling) => {
                error.non_unicode_process.push(NonUnicodeProcessSecret {
                    name: name.clone(),
                    spelling,
                });
            }
            ProcessValue::Missing => {
                if let Some((spelling, Some(first_use))) = extracted.get(name) {
                    still_missing.push(((*spelling).to_string(), (*first_use).clone()));
                }
            }
        }
    }

    if !error.is_empty() {
        return Err(SecretsError::Resolution(Box::new(error)));
    }

    if !still_missing.is_empty() {
        if no_input || !interactive() {
            error.missing = still_missing
                .into_iter()
                .map(|(name, first_use)| MissingSecret { name, first_use })
                .collect();
            return Err(SecretsError::Resolution(Box::new(error)));
        }
        for (name, _first_use) in still_missing {
            let value = prompt_and_offer_to_save(repo_root, &name).map_err(SecretsError::Prompt)?;
            resolved.push((name, value));
        }
    }

    Ok(SecretsOutcome { resolved })
}

/// Prompts (no echo) for `name`'s value, then offers to persist it to the
/// encrypted repository-local vault for future runs.
fn prompt_and_offer_to_save(repo_root: &Path, name: &str) -> Result<String, String> {
    let value = Password::with_theme(&ColorfulTheme::default())
        .with_prompt(format!("Enter a value for secret '{name}'"))
        .interact()
        .map_err(|error| format!("could not read a value for secret '{name}': {error}"))?;
    let save = Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt(format!(
            "Save '{name}' to the encrypted .litci/secrets.vault for future runs?"
        ))
        .default(false)
        .interact()
        .unwrap_or(false);
    if save {
        dotenv::append_secret(repo_root, name, &value)
            .map_err(|message| format!("could not save '{name}' to the secret vault: {message}"))?;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use greenlit_workflow::Location;
    use greenlit_workflow::extract::StaticExtraction;

    /// A placeholder span for tests that need one but do not exercise any
    /// span-rendering behavior themselves.
    fn span() -> Span {
        Span::new(
            std::sync::Arc::from("<test>"),
            Location::new(1, 1),
            Location::new(1, 1),
        )
    }

    fn extraction_with_secret(name: &str) -> StaticExtraction {
        let mut extraction = StaticExtraction::default();
        extraction.secrets.insert(name.to_string(), vec![span()]);
        extraction
    }

    #[test]
    fn github_token_is_never_a_candidate() {
        let extraction = extraction_with_secret("GITHUB_TOKEN");
        let outcome = resolve_secrets(&extraction, &[], &[], Path::new("."), true)
            .expect("GITHUB_TOKEN is excluded, not missing");
        assert!(outcome.resolved.is_empty());
    }

    #[test]
    fn cli_override_resolves_a_referenced_secret() {
        let extraction = extraction_with_secret("API_TOKEN");
        let cli = vec![("API_TOKEN".to_string(), "s3cr3t".to_string())];
        let outcome =
            resolve_secrets(&extraction, &cli, &[], Path::new("."), true).expect("resolves");
        assert_eq!(
            outcome.resolved,
            vec![("API_TOKEN".to_string(), "s3cr3t".to_string())]
        );
    }

    fn expect_resolution(result: Result<SecretsOutcome, SecretsError>) -> SecretResolutionError {
        match result {
            Ok(outcome) => panic!("expected a resolution error, got {outcome:?}"),
            Err(SecretsError::Resolution(failure)) => *failure,
            Err(SecretsError::Prompt(message)) => {
                panic!("expected a resolution error, got a prompt error: {message}")
            }
        }
    }

    #[test]
    fn no_input_fails_fast_listing_every_missing_name() {
        let mut extraction = StaticExtraction::default();
        extraction.secrets.insert("FIRST".to_string(), vec![span()]);
        extraction
            .secrets
            .insert("SECOND".to_string(), vec![span()]);
        let error = expect_resolution(resolve_secrets(&extraction, &[], &[], Path::new("."), true));
        let names: Vec<&str> = error.missing.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["FIRST", "SECOND"]);
    }

    #[test]
    fn an_oversized_local_value_is_rejected() {
        let extraction = extraction_with_secret("BIG");
        let cli = vec![("BIG".to_string(), "x".repeat(48 * 1024 + 1))];
        let error = expect_resolution(resolve_secrets(
            &extraction,
            &cli,
            &[],
            Path::new("."),
            true,
        ));
        assert_eq!(error.oversized[0].name, "BIG");
    }

    #[test]
    fn an_invalid_referenced_name_is_rejected() {
        let extraction = extraction_with_secret("1BAD");
        let error = expect_resolution(resolve_secrets(&extraction, &[], &[], Path::new("."), true));
        assert_eq!(error.invalid[0].name, "1BAD");
    }
}
