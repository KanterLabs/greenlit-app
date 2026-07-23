//! Shell resolution matching GitHub Actions.
//!
//! GitHub selects the program that runs a `run:` step's script from the
//! step's `shell:`, then `defaults.run.shell`, then a platform default. The
//! documented internal commands (with `{0}` standing in for the temporary
//! script file) are transcribed here.
//! <https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idstepsshell>

use thiserror::Error;

/// The concrete program and argument vector that runs a step's script. The
/// `{0}` placeholder in the resolved shell template has already been replaced
/// with the script path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellInvocation {
    /// The program to execute (`argv[0]`).
    pub program: String,
    /// The arguments following the program.
    pub args: Vec<String>,
}

/// Everything shell resolution needs beyond the authored `shell:` text: the
/// effective `defaults.run.shell`, whether the job runs in a `container:`, and
/// whether `bash` exists on the image.
#[derive(Debug, Clone, Copy)]
pub struct ShellSelection<'a> {
    /// The step's resolved `shell:` value, if authored.
    pub step_shell: Option<&'a str>,
    /// The effective `defaults.run.shell` (workflow merged with job), if any.
    pub default_shell: Option<&'a str>,
    /// Whether steps run inside a `jobs.<id>.container` image.
    pub in_container: bool,
    /// Whether `bash` is available on the runner image. Only consulted for the
    /// *default* (unspecified) shell, whose documented fallback is `sh`.
    pub bash_available: bool,
}

/// Why a `shell:` value could not be resolved on a Linux v0 host.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ShellError {
    /// A built-in shell keyword Greenlit v0 does not run faithfully (`cmd`,
    /// `powershell`, `pwsh`). `cmd`/`powershell` are Windows-only; `pwsh` uses
    /// a quoted internal command template outside v0's shell-only scope. v0
    /// hosts are Linux x86_64.
    #[error(
        "shell '{shell}' is not supported in v0; use 'bash', 'sh', 'python', or a custom command template"
    )]
    UnsupportedShell {
        /// The rejected keyword.
        shell: String,
    },
    /// A custom `shell:` string was empty.
    #[error("shell: was set to an empty string; give a shell keyword or a command template")]
    Empty,
}

/// Resolves a step's shell into the concrete program + args, substituting
/// `script_path` for the `{0}` placeholder.
///
/// Precedence follows GitHub: an explicit step `shell:` wins, then
/// `defaults.run.shell`, then the platform default. For the *default*
/// (nothing authored), an ordinary Ubuntu job uses `bash -e {0}` when `bash`
/// exists and falls back to `sh -e {0}` otherwise, while a container job
/// defaults straight to `sh -e {0}` (a container image is not guaranteed to
/// ship `bash`). Note the documented asymmetry: the *default* bash is
/// `bash -e {0}`, but an *explicit* `shell: bash` is
/// `bash --noprofile --norc -eo pipefail {0}`.
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idstepsshell>
pub fn resolve_shell(
    selection: ShellSelection<'_>,
    script_path: &str,
) -> Result<ShellInvocation, ShellError> {
    let template = match selection.step_shell.or(selection.default_shell) {
        Some(shell) => explicit_template(shell)?,
        None => default_template(selection.in_container, selection.bash_available),
    };
    Ok(substitute(&template, script_path))
}

/// The command template for the default (unspecified) shell.
fn default_template(in_container: bool, bash_available: bool) -> String {
    // GitHub: bash on non-Windows with a fallback to sh. A container image is
    // treated like a bash-absent host so that images without bash still run.
    if in_container || !bash_available {
        "sh -e {0}".to_string()
    } else {
        "bash -e {0}".to_string()
    }
}

/// The command template for an explicit `shell:` keyword, or a verbatim custom
/// command string.
fn explicit_template(shell: &str) -> Result<String, ShellError> {
    let trimmed = shell.trim();
    if trimmed.is_empty() {
        return Err(ShellError::Empty);
    }
    let template = match trimmed {
        // Built-in keywords and their documented internal commands.
        "bash" => "bash --noprofile --norc -eo pipefail {0}",
        "sh" => "sh -e {0}",
        "python" => "python {0}",
        "pwsh" | "cmd" | "powershell" => {
            return Err(ShellError::UnsupportedShell {
                shell: trimmed.to_string(),
            });
        }
        // Any other value is a custom command template.
        custom => custom,
    };
    Ok(template.to_string())
}

/// Tokenizes a shell template on whitespace and substitutes the script path
/// for `{0}`. If the template names no `{0}` placeholder, GitHub appends the
/// script path as the final argument.
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#custom-shell>
fn substitute(template: &str, script_path: &str) -> ShellInvocation {
    let mut tokens = template.split_whitespace().map(str::to_string);
    let program = tokens.next().unwrap_or_default();
    let mut args: Vec<String> = tokens.collect();
    let mut had_placeholder = program.contains("{0}");
    for arg in &mut args {
        if arg.contains("{0}") {
            had_placeholder = true;
            *arg = arg.replace("{0}", script_path);
        }
    }
    let program = program.replace("{0}", script_path);
    if !had_placeholder {
        args.push(script_path.to_string());
    }
    ShellInvocation { program, args }
}
