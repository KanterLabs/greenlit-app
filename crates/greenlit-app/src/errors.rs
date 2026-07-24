//! Renders every error `litci` can produce as `file:line:col: message`
//! (whichever library error types already embed a location render as
//! exactly that), followed by a `fix:` line naming the one action that
//! resolves it (`AGENTS.md` UX invariant: "every error maps to a state plus
//! the one action that fixes it"; `PHASE-1-engine-core.md`: "Errors render
//! with source spans: file:line:col -- message -- fix"). Kept in one module
//! so the same phrasing is used regardless of which pipeline stage failed.

use greenlit_engine::{EventError, GitError, GraphError, PlanError};
use greenlit_expr::Value;
use greenlit_metrics::MetricsError;
use greenlit_workflow::ParseError;

use crate::vars::{VarResolutionError, VarsOutcome};

fn safe_text(text: &str) -> String {
    crate::render::terminal::inline_escape(text)
}

fn safe_error(error: &impl std::fmt::Display) -> String {
    safe_text(&error.to_string())
}

pub(crate) fn parse_error(e: &ParseError) -> anyhow::Error {
    let fix = match e {
        ParseError::Io { .. } => "check the path passed to -W and its file permissions",
        ParseError::Encoding { .. } => "save the workflow file as valid UTF-8, then retry",
        ParseError::SourceLimit { .. } => "reduce the workflow file below GitHub's size limit",
        ParseError::Yaml { .. } => "fix the YAML syntax at the location above",
        ParseError::UnknownKey { .. } => {
            "remove the key, or check it for a typo against the supported workflow schema"
        }
        ParseError::MissingKey { .. } => "add the missing key",
        ParseError::Schema { .. } => "fix the value to match the shape described above",
        ParseError::YamlLimit { .. } => {
            "reduce YAML nesting or anchor/alias expansion below GitHub's parser limits"
        }
        ParseError::UnsupportedTag { .. } => {
            "remove the explicit YAML tag, or use one of !!str/!!bool/!!int/!!float/!!null"
        }
        ParseError::TagMismatch { .. } => {
            "fix the value so it matches the explicit tag, or remove the tag"
        }
        ParseError::InvalidTagStyle { .. } => {
            "remove the non-string tag, or write its value as an unquoted plain scalar"
        }
        ParseError::IntegerOverflow { .. } => "use a value that fits in a 32-bit integer",
        ParseError::DuplicateKey { .. } => "remove or rename one of the duplicate keys",
        ParseError::MultipleDocuments { .. } => {
            "keep exactly one YAML document in the file (remove the extra `---` separators)"
        }
        ParseError::EmptyDocument { .. } => "add workflow content to the file",
        ParseError::Expression { .. } => "fix the expression at the location above",
    };
    anyhow::anyhow!("{}\n  fix: {fix}", safe_error(e))
}

pub(crate) fn plan_error(e: &PlanError) -> anyhow::Error {
    let fix = match e {
        PlanError::StaticExtraction { .. } => "fix the workflow expression at the location above",
        PlanError::NotSupportedInV0 { .. } => {
            "remove or restructure the workflow to avoid this construct -- it is out of scope for Greenlit v0"
        }
        PlanError::WorkflowEval { .. } => "fix the workflow expression referenced above",
        PlanError::Graph(GraphError::UnknownNeed { .. }) => {
            "fix the `needs:` entry to name a job id defined in this workflow"
        }
        PlanError::Graph(GraphError::Cycles(_)) => {
            "break the cycle by removing one of the `needs:` edges listed above"
        }
        PlanError::Matrix { .. } => "fix the `strategy` field named in the message above",
        PlanError::Runner { .. } => {
            "use one of the supported runner labels: ubuntu-latest, ubuntu-24.04, ubuntu-22.04"
        }
        PlanError::Eval { .. } => "fix the expression referenced above",
        PlanError::JobConditionUnavailable { .. } => {
            "move this check into a step-level `if:` condition, or rewrite it using only github/needs/vars/inputs and the status functions"
        }
        PlanError::SizeLimit { .. } => {
            "reduce expanded matrix data or context-expanded field sizes, then retry"
        }
    };
    anyhow::anyhow!("{}\n  fix: {fix}", safe_error(e))
}

pub(crate) fn event_error(e: &EventError) -> anyhow::Error {
    let fix = match e {
        EventError::Git(git_err) => match git_err {
            GitError::NotARepository { .. } => "run litci inside a git repository",
            GitError::NoCommits { .. } => "make an initial commit so HEAD exists",
            GitError::DetachedHead { .. } => "check out a branch before planning a push event",
            GitError::CommandFailed { .. } => "ensure the git binary is installed and on PATH",
            GitError::CommandUnsuccessful { .. } => {
                "repair the local repository's refs and objects, then retry"
            }
            GitError::MissingObjects { .. } => {
                "fetch the missing Git objects explicitly, then retry; Greenlit never lazy-fetches while planning"
            }
            GitError::OutputLimit { .. } => {
                "shorten the oversized local Git metadata value reported above, then retry"
            }
            GitError::ChangedPathLimit { .. } => {
                "rename the changed path below the listed byte limit, commit the rename, then retry"
            }
            GitError::CommandTimedOut { .. } => {
                "run the listed git command directly, repair the local repository if it stalls, then retry"
            }
        },
        EventError::InputsForNonDispatch { .. } => {
            "select -e workflow_dispatch, or remove the --input arguments"
        }
        EventError::UnknownInput { .. } => {
            "use a workflow_dispatch input name declared by this workflow"
        }
        EventError::MissingRequiredInput { .. } => {
            "pass --input NAME=VALUE for the required workflow_dispatch input"
        }
        EventError::InvalidInputValue { .. } => {
            "pass --input NAME=VALUE using the declared input type/options"
        }
        EventError::EventNotDeclared { .. } => {
            "select an event declared under `on:`, or add this event to the workflow"
        }
        EventError::EventFilteredOut { .. } => {
            "check out a matching branch or choose an event that satisfies the workflow trigger filters"
        }
        EventError::InvalidFilterPattern { .. } => {
            "fix the trigger filter pattern at the location above"
        }
    };
    anyhow::anyhow!("{}\n  fix: {fix}", safe_error(e))
}

/// Renders a variable-resolution failure. `suggest_auth` adds `litci auth`
/// as the fix for every still-unresolved literal name/dynamic lookup
/// (`PHASE-3-actions.md` Variables: "stop before engine detection with
/// `litci auth` as the single fix action") — set only for
/// [`crate::vars::VarsOutcome::AuthRequired`], never for a hard local error
/// (invalid/oversized/ambiguous/non-Unicode), which no amount of
/// authentication could fix.
pub(crate) fn vars_resolution(failure: &VarResolutionError, suggest_auth: bool) -> anyhow::Error {
    let mut message = String::new();
    for invalid in &failure.invalid {
        let span = safe_text(&invalid.first_use.to_string());
        let name = safe_text(&invalid.name);
        message.push_str(&format!(
            "{}: 'vars.{}' is not a valid GitHub configuration variable name: {}\n  fix: rename the reference to use only letters, digits, and underscores, starting with a letter or underscore and not GITHUB_\n",
            span, name, invalid.reason
        ));
    }
    for oversized in &failure.oversized {
        let name = safe_text(&oversized.name);
        message.push_str(&format!(
            "configuration variable 'vars.{}' has a {}-byte value, exceeding GitHub's 48 KiB per-variable limit\n  fix: shorten the value supplied through --var, the process environment, or .litci/vars\n",
            name, oversized.bytes
        ));
    }
    for ambiguous in &failure.ambiguous_process {
        let name = safe_text(&ambiguous.name);
        let spellings = safe_text(&ambiguous.spellings.join(", "));
        message.push_str(&format!(
            "process environment defines case-insensitive configuration variable 'vars.{}' more than once ({})\n  fix: remove the duplicate case variants, or pass --var {}=<value> to choose explicitly\n",
            name, spellings, name
        ));
    }
    for non_unicode in &failure.non_unicode_process {
        let spelling = safe_text(&non_unicode.spelling);
        let name = safe_text(&non_unicode.name);
        message.push_str(&format!(
            "process environment variable '{}' for 'vars.{}' is not valid UTF-8\n  fix: remove it, or pass --var {}=<value> with a UTF-8 value\n",
            spelling, name, name
        ));
    }
    for m in &failure.missing {
        let span = safe_text(&m.first_use.to_string());
        let name = safe_text(&m.name);
        let auth_fix = if suggest_auth {
            " or run `litci auth` to look it up from GitHub"
        } else {
            ""
        };
        message.push_str(&format!(
            "{span}: variable 'vars.{name}' is not set\n  fix: pass --var {name}=<value>, set ${name} in the environment, or add {name}=<value> to .litci/vars{auth_fix}\n"
        ));
    }
    if let Some(span) = &failure.dynamic_lookup {
        let span = safe_text(&span.to_string());
        let auth_fix = if suggest_auth {
            ", or run `litci auth` to fetch the complete repository/organization map from GitHub"
        } else {
            ""
        };
        message.push_str(&format!(
            "{span}: dynamic `vars[...]` lookup requires a complete local variable map\n  fix: pass at least one --var KEY=VALUE, or create .litci/vars (an empty file declares an empty map){auth_fix}\n"
        ));
    }
    anyhow::anyhow!(message.trim_end().to_string())
}

/// Resolves a [`VarsOutcome`] into the `vars` context `Value` or the one
/// `anyhow::Error` to report — the single call site both `crate::plan_cmd`
/// and `crate::run_cmd` share so the local/auth-required/remote-error
/// renderings stay identical between the two commands
/// (`PHASE-3-actions.md`: "keep plan and run behavior consistent").
pub(crate) fn vars_outcome(outcome: VarsOutcome) -> anyhow::Result<Value> {
    match outcome {
        VarsOutcome::Resolved(value) => Ok(value),
        VarsOutcome::LocalError(failure) => Err(vars_resolution(&failure, false)),
        VarsOutcome::AuthRequired(failure) => Err(vars_resolution(&failure, true)),
        VarsOutcome::RemoteError(message) => Err(anyhow::anyhow!(safe_text(&message))),
    }
}

pub(crate) fn metrics_error(error: &MetricsError) -> anyhow::Error {
    let fix = match error {
        MetricsError::HomeDirUnavailable | MetricsError::InvalidHomeDir => {
            "set HOME to your absolute user home directory"
        }
        MetricsError::CreateDir { .. }
        | MetricsError::OpenForWrite { .. }
        | MetricsError::LockFile { .. }
        | MetricsError::WriteRecord { .. }
        | MetricsError::ReadFile { .. } => {
            "fix the listed path's ownership and permissions, then retry"
        }
        MetricsError::InvalidPathComponent { .. } => {
            "replace the listed symbolic link or special file with a real directory or regular file, then retry"
        }
        MetricsError::Serialize { .. } => "update litci and retry the command",
        MetricsError::RecordWriteLimit { .. } => {
            "reduce the workflow's number or length of named stages and steps, then retry"
        }
        MetricsError::RecordReadLimit { .. } => {
            "move the listed runs.ndjson file aside, then rerun litci"
        }
        MetricsError::CorruptRecord { .. } | MetricsError::CorruptRecentRecord { .. } => {
            "move the listed runs.ndjson file aside, then rerun litci"
        }
        MetricsError::UnsupportedSchema { .. } | MetricsError::UnsupportedRecentSchema { .. } => {
            "update litci to a version that supports this metrics schema"
        }
    };
    anyhow::anyhow!("{}\n  fix: {fix}", safe_error(error))
}
