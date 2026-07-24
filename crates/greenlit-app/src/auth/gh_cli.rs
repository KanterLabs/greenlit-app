//! `litci auth --gh`: read the current token from the `gh` CLI
//! (`gh auth token`) rather than performing any GitHub login of Greenlit's
//! own.

use std::process::Command;

/// Runs `gh auth token` and returns its trimmed stdout as the token.
///
/// # Errors
/// Returns a message with a fix action when `gh` is not on `PATH`, is not
/// logged in (non-zero exit), or produced no usable output.
pub(crate) fn token_from_gh() -> Result<String, String> {
    run_gh_auth_token(None)
}

/// The real implementation, taking an optional `PATH` override so tests can
/// point `Command::new("gh")` at a controlled fake executable via
/// `Command::env` (which only affects the spawned child, unlike mutating
/// this process's own environment, which `#![forbid(unsafe_code)]` and
/// Rust edition 2024 both make unavailable here) rather than the developer
/// machine's real `gh` (`TESTING.md`: "Mock only true externals" — an
/// external CLI tool is exactly that boundary).
fn run_gh_auth_token(path_override: Option<&str>) -> Result<String, String> {
    let mut command = Command::new("gh");
    command.args(["auth", "token"]);
    if let Some(path) = path_override {
        command.env("PATH", path);
    }
    let output = command.output().map_err(|error| {
        format!(
            "could not run `gh auth token`: {error}\n  fix: install the GitHub CLI (https://cli.github.com), or use `litci auth` / `litci auth --pat` instead"
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "`gh auth token` failed: {}\n  fix: run `gh auth login` first, or use `litci auth` / `litci auth --pat` instead",
            stderr.trim()
        ));
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        return Err(
            "`gh auth token` printed no token\n  fix: run `gh auth login` first, or use `litci auth` / `litci auth --pat` instead"
                .to_string(),
        );
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    /// Writes a fake `gh` executable to `dir/gh` running `script` and
    /// returns a `PATH` value with `dir` prefixed onto the real `PATH`
    /// (which the fake still needs, for `/bin/sh` itself to resolve).
    fn fake_gh(dir: &std::path::Path, script: &str) -> String {
        let path = dir.join("gh");
        let mut file = std::fs::File::create(&path).expect("create fake gh");
        file.write_all(script.as_bytes()).expect("write fake gh");
        let mut perms = file.metadata().unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&path, perms).expect("chmod fake gh");
        let real_path = std::env::var("PATH").unwrap_or_default();
        format!("{}:{real_path}", dir.display())
    }

    #[test]
    fn returns_the_trimmed_token_on_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = fake_gh(dir.path(), "#!/bin/sh\necho ' gho_faketoken \n'\nexit 0\n");
        let token = run_gh_auth_token(Some(&path)).expect("token");
        assert_eq!(token, "gho_faketoken");
    }

    #[test]
    fn a_nonzero_exit_is_reported_with_a_fix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = fake_gh(dir.path(), "#!/bin/sh\necho 'not logged in' 1>&2\nexit 1\n");
        let error = run_gh_auth_token(Some(&path)).unwrap_err();
        assert!(error.contains("not logged in"), "{error}");
        assert!(error.contains("gh auth login"), "{error}");
    }

    #[test]
    fn a_missing_gh_executable_is_reported_with_a_fix() {
        let dir = tempfile::tempdir().expect("empty tempdir, no gh in it");
        let error = run_gh_auth_token(Some(&dir.path().display().to_string())).unwrap_err();
        assert!(error.contains("could not run"), "{error}");
        assert!(error.contains("litci auth"), "{error}");
    }
}
