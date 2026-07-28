//! `litci auth --pat`: paste a fine-grained personal access token.
//!
//! Interactive terminals get `dialoguer`'s no-echo `Password` prompt,
//! matching `crate::workflow_picker`'s TTY-gated convention. A non-terminal
//! stdin (a script, CI, or an automated test) instead reads one trimmed
//! line directly — `dialoguer::Password::interact` itself refuses to run at
//! all without a real terminal, and a plain piped line (`echo "$PAT" | litci
//! auth --pat`) is the standard scriptable equivalent other credential-paste
//! CLIs (e.g. `docker login --password-stdin`) already use.

use std::io::{BufRead, IsTerminal, Write};

use dialoguer::Password;
use dialoguer::theme::ColorfulTheme;

/// Prompts for (or reads) a pasted PAT, returning it trimmed of surrounding
/// whitespace/newline.
pub(crate) fn prompt_for_pat() -> Result<String, String> {
    let token = if std::io::stdin().is_terminal() {
        Password::with_theme(&ColorfulTheme::default())
            .with_prompt("Paste your fine-grained personal access token")
            .interact()
            .map_err(|error| format!("could not read the pasted token: {error}"))?
    } else {
        read_stdin_line()?
    };
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err(
            "no token was provided\n  fix: paste a fine-grained personal access token, or pipe one on stdin"
                .to_string(),
        );
    }
    Ok(token)
}

fn read_stdin_line() -> Result<String, String> {
    let stdin = std::io::stdin();
    let mut line = String::new();
    stdin
        .lock()
        .read_line(&mut line)
        .map_err(|error| format!("could not read the token from stdin: {error}"))?;
    Ok(line)
}

/// Guidance printed alongside every `--pat` prompt, naming the exact
/// fine-grained permissions the resulting token needs for the endpoints
/// this crate calls (`crate::vars::remote`): read-only repository
/// "Contents" (action/checkout-adjacent lookups) and "Variables"
/// permissions, plus organization-level "Variables" read access when the
/// workflow references organization configuration variables.
/// <https://docs.github.com/en/rest/actions/variables>
pub(crate) fn print_permission_guidance(out: &mut impl Write) {
    writeln!(
        out,
        "Create a fine-grained PAT (https://github.com/settings/personal-access-tokens/new) with:\n  - Repository permissions: 'Contents' (read), 'Variables' (read)\n  - Organization permissions (only if the workflow reads org-level configuration variables): 'Variables' (read)\nGreenlit never requests write access on your behalf."
    )
    .ok();
}
