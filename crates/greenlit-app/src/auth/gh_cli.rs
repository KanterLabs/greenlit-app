//! `litci auth --gh`: read the current token from the `gh` CLI
//! (`gh auth token`) rather than performing any GitHub login of Greenlit's
//! own.

use std::process::{Command, Stdio};

const MAX_TOKEN_OUTPUT_BYTES: usize = 16 * 1024;
const GH_LOGIN_FIX: &str =
    "\n  fix: run `gh auth login` first, or use `litci auth` / `litci auth --pat` instead";

/// Runs `gh auth token` and returns its trimmed stdout as the token.
///
/// # Errors
/// Returns a message with a fix action when `gh` is not on `PATH`, is not
/// logged in (non-zero exit), or produced no usable output.
pub(crate) fn token_from_gh() -> Result<String, String> {
    let mut command = Command::new("gh");
    command.args(["auth", "token"]);
    command.stderr(Stdio::null());
    let output = command.output().map_err(|_| {
        "could not run `gh auth token`\n  fix: install the GitHub CLI (https://cli.github.com), or use `litci auth` / `litci auth --pat` instead"
            .to_string()
    })?;
    if !output.status.success() {
        return Err(format!("GitHub CLI authentication failed{GH_LOGIN_FIX}"));
    }
    if output.stdout.len() > MAX_TOKEN_OUTPUT_BYTES {
        return Err(format!(
            "`gh auth token` returned an invalid credential{GH_LOGIN_FIX}"
        ));
    }
    let stdout = std::str::from_utf8(&output.stdout)
        .map_err(|_| format!("`gh auth token` returned an invalid credential{GH_LOGIN_FIX}"))?;
    let token = stdout.trim();
    if token.is_empty() {
        return Err(format!(
            "`gh auth token` returned no credential{GH_LOGIN_FIX}"
        ));
    }
    if !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(format!(
            "`gh auth token` returned an invalid credential{GH_LOGIN_FIX}"
        ));
    }
    Ok(token.to_string())
}
