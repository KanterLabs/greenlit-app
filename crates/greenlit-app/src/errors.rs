//! Renders every error `litci` can produce as `file:line:col: message`
//! (whichever library error types already embed a location render as
//! exactly that), followed by a `fix:` line naming the one action that
//! resolves it (`AGENTS.md` UX invariant: "every error maps to a state plus
//! the one action that fixes it"; `PHASE-1-engine-core.md`: "Errors render
//! with source spans: file:line:col -- message -- fix"). Kept in one module
//! so the same phrasing is used regardless of which pipeline stage failed.

use greenlit_engine::{EventError, GitError, GraphError, PlanError};
use greenlit_workflow::ParseError;

use crate::vars::MissingVar;

pub(crate) fn parse_error(e: &ParseError) -> anyhow::Error {
    let fix = match e {
        ParseError::Io { .. } => "check the path passed to -W and its file permissions",
        ParseError::Yaml { .. } => "fix the YAML syntax at the location above",
        ParseError::UnknownKey { .. } => {
            "remove the key, or check it for a typo against the supported workflow schema"
        }
        ParseError::MissingKey { .. } => "add the missing key",
        ParseError::Schema { .. } => "fix the value to match the shape described above",
        ParseError::UnsupportedTag { .. } => {
            "remove the explicit YAML tag, or use one of !!str/!!bool/!!int/!!float/!!null"
        }
        ParseError::TagMismatch { .. } => {
            "fix the value so it matches the explicit tag, or remove the tag"
        }
        ParseError::IntegerOverflow { .. } => "use a value that fits in a 32-bit integer",
        ParseError::DuplicateKey { .. } => "remove or rename one of the duplicate keys",
        ParseError::MultipleDocuments { .. } => {
            "keep exactly one YAML document in the file (remove the extra `---` separators)"
        }
        ParseError::EmptyDocument { .. } => "add workflow content to the file",
    };
    anyhow::anyhow!("{e}\n  fix: {fix}")
}

pub(crate) fn plan_error(e: &PlanError) -> anyhow::Error {
    let fix = match e {
        PlanError::NotSupportedInV0 { .. } => {
            "remove or restructure the workflow to avoid this construct -- it is out of scope for Greenlit v0"
        }
        PlanError::Graph(GraphError::UnknownNeed { .. }) => {
            "fix the `needs:` entry to name a job id defined in this workflow"
        }
        PlanError::Graph(GraphError::Cycles(_)) => {
            "break the cycle by removing one of the `needs:` edges listed above"
        }
        PlanError::Matrix { .. } => {
            "fix the `strategy.matrix`/`include`/`exclude` entries per the message above"
        }
        PlanError::Runner { .. } => {
            "use one of the supported runner labels: ubuntu-latest, ubuntu-24.04, ubuntu-22.04"
        }
        PlanError::Eval { .. } => "fix the expression referenced above",
        PlanError::JobConditionUnavailable { .. } => {
            "move this check into a step-level `if:` condition, or rewrite it using only github/needs/vars/inputs and the status functions"
        }
        PlanError::NeedsReferenceNotDeclared { .. } => {
            "add the referenced job to this job's `needs:` list, or remove the reference"
        }
    };
    anyhow::anyhow!("{e}\n  fix: {fix}")
}

pub(crate) fn event_error(e: &EventError) -> anyhow::Error {
    let EventError::Git(git_err) = e;
    let fix = match git_err {
        GitError::NotARepository { .. } => "run litci inside a git repository",
        GitError::NoCommits { .. } => "make an initial commit so HEAD exists",
        GitError::DetachedHead { .. } => "check out a branch before planning a push event",
        GitError::CommandFailed { .. } => "ensure the git binary is installed and on PATH",
    };
    anyhow::anyhow!("{e}\n  fix: {fix}")
}

pub(crate) fn missing_vars(missing: &[MissingVar]) -> anyhow::Error {
    let mut message = String::new();
    for m in missing {
        message.push_str(&format!(
            "{}: variable 'vars.{}' is not set\n  fix: pass --var {}=<value>, set ${} in the environment, or add {}=<value> to .litci/vars\n",
            m.first_use, m.name, m.name, m.name, m.name
        ));
    }
    anyhow::anyhow!(message.trim_end().to_string())
}
