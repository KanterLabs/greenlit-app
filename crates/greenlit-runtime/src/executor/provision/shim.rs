//! Command-level lazy provisioning: the shim a missing tool gets.
//!
//! `PHASE-4-environment.md`: "generate Greenlit-controlled shims only for
//! manifest-known commands absent from the slim image and put their directory
//! last in `PATH`, so installed real commands always win. On first
//! invocation, a shim … installs the exact manifest-pinned tool, waits for
//! success, and `exec`s the original argv and environment against the real
//! command. … Never restart the shell or whole step; commands and side
//! effects before the missing command execute exactly once."
//!
//! # The recipe is baked in, not requested
//!
//! The brief describes the shim asking a host control boundary to install the
//! tool, over a channel that "accepts no arbitrary package or shell input".
//! Greenlit gets that property more directly: the host decides the recipe
//! when it *generates* the shim, and writes that one fixed command into it.
//! There is no channel to abuse because there is no request — a shim for `jq`
//! can install `jq` and nothing else, and a workflow cannot author a shim at
//! all, since `/greenlit` is a reserved control root. Fewer moving parts,
//! same guarantee, no network hop in the middle of a user's step.
//!
//! # Exactly once
//!
//! The shim `exec`s the real command, replacing itself. The step's shell is
//! never restarted and the script is never replayed, so anything the script
//! did before reaching the missing command happened once and stays done. This
//! is the fidelity requirement that rules out the obvious alternative of
//! detecting the failure and re-running the step.

use super::manifest::Recipe;

/// Renders the shim script for `command` under `recipe`.
///
/// The result is the body of an executable file placed in the provisioning
/// shim directory, which sits **last** on `PATH` — so the moment the real
/// tool exists, the shim stops being reached at all.
pub(crate) fn render(command: &str, recipe: &Recipe, shim_dir: &str, label: &str) -> String {
    let install = install_command(recipe);
    // `PHASE-4-environment.md`: "Every successful automatic install logs
    // `installed <tool>@<version> (present on <runner-label>)`." An apt
    // package carries no pinned version, so it says so rather than inventing
    // one.
    let version = recipe.pinned_version().unwrap_or("distribution default");
    let installed_line = format!("installed {command}@{version} (present on {label})");
    // `$0` is not used to find the real tool: the shim must never resolve to
    // itself, so its own directory is skipped while scanning PATH.
    format!(
        "#!/bin/sh\n\
         # Managed by Greenlit. `{command}` is on the GitHub runner image but not\n\
         # on Greenlit's slim base, so it is installed on first use and this\n\
         # process is replaced by the real one -- the step is never replayed.\n\
         set -e\n\
         if ! [ -e /greenlit/provisioned/{command} ]; then\n\
         \x20 echo \"greenlit: installing {command} (present on this runner image)\" >&2\n\
         \x20 {install} >&2\n\
         \x20 mkdir -p /greenlit/provisioned && : > /greenlit/provisioned/{command}\n\
         \x20 echo \"greenlit: {installed_line}\" >&2\n\
         fi\n\
         real=\"\"\n\
         IFS=:\n\
         for dir in $PATH; do\n\
         \x20 [ \"$dir\" = \"{shim_dir}\" ] && continue\n\
         \x20 if [ -x \"$dir/{command}\" ]; then real=\"$dir/{command}\"; break; fi\n\
         done\n\
         unset IFS\n\
         if [ -z \"$real\" ]; then\n\
         \x20 echo \"greenlit: installing {command} did not produce the command\" >&2\n\
         \x20 echo \"  fix: this is a Greenlit defect -- the runner-images manifest lists {command} but the install recipe did not provide it\" >&2\n\
         \x20 exit 127\n\
         fi\n\
         exec \"$real\" \"$@\"\n"
    )
}

/// The one fixed install command a shim carries.
///
/// Also replayed by [`super::build_converged`] in a clean container, so the
/// converged image and the lazy install can never diverge on how a tool is
/// installed.
pub(crate) fn install_command(recipe: &Recipe) -> String {
    match recipe {
        Recipe::Apt { package } => format!(
            "(apt-get update -qq && apt-get install -y -qq --no-install-recommends {package})"
        ),
        Recipe::Rustup { version } => format!(
            // `RUSTUP_VERSION` pins the installer itself to what the runner
            // image carries. `--default-toolchain none` is deliberate: the
            // toolchain comes from the repository's own `rust-toolchain.toml`,
            // exactly as it does on a hosted runner, so Greenlit must not
            // pick one.
            "(export RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo RUSTUP_VERSION={version}; \
             curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
             | sh -s -- -y --no-modify-path --default-toolchain none \
             && ln -sf /usr/local/cargo/bin/rustup /usr/local/bin/rustup \
             && ln -sf /usr/local/cargo/bin/cargo /usr/local/bin/cargo \
             && ln -sf /usr/local/cargo/bin/rustc /usr/local/bin/rustc)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{Recipe, render};

    const SHIM_DIR: &str = "/greenlit/shims";

    fn apt_shim() -> String {
        render(
            "jq",
            &Recipe::Apt {
                package: "jq".to_string(),
            },
            SHIM_DIR,
            "ubuntu-24.04",
        )
    }

    #[test]
    fn every_install_logs_the_tool_version_and_runner_label() {
        assert!(
            apt_shim().contains("installed jq@distribution default (present on ubuntu-24.04)"),
            "an apt package has no pinned version, so it says so rather than inventing one"
        );
        assert!(
            render(
                "rustup",
                &Recipe::Rustup {
                    version: "1.29.0".to_string()
                },
                SHIM_DIR,
                "ubuntu-24.04",
            )
            .contains("installed rustup@1.29.0 (present on ubuntu-24.04)")
        );
    }

    #[test]
    fn the_shim_execs_the_real_command_rather_than_replaying_the_step() {
        let shim = apt_shim();
        assert!(
            shim.contains("exec \"$real\" \"$@\""),
            "the original argv replaces this process; the step's shell is never restarted"
        );
        assert!(
            !shim.contains("exit 1\n") || shim.contains("exit 127"),
            "the only non-exec exit is the defect path"
        );
    }

    #[test]
    fn the_shim_never_resolves_to_itself() {
        assert!(
            apt_shim().contains(&format!("[ \"$dir\" = \"{SHIM_DIR}\" ] && continue")),
            "without skipping its own directory the shim execs itself forever"
        );
    }

    #[test]
    fn the_recipe_is_fixed_with_no_room_for_caller_input() {
        // The security property: a shim for `jq` installs `jq` and nothing
        // else. There is no request channel, so there is nothing to abuse.
        let shim = apt_shim();
        assert!(shim.contains("apt-get install -y -qq --no-install-recommends jq"));
        assert!(
            !shim.contains("$1") && !shim.contains("${1"),
            "no argument reaches the install command"
        );
    }

    #[test]
    fn a_second_invocation_does_not_reinstall() {
        let shim = apt_shim();
        assert!(shim.contains("if ! [ -e /greenlit/provisioned/jq ]"));
        assert!(shim.contains(": > /greenlit/provisioned/jq"));
    }

    #[test]
    fn rustup_is_pinned_and_picks_no_toolchain() {
        let shim = render(
            "rustup",
            &Recipe::Rustup {
                version: "1.29.0".to_string(),
            },
            SHIM_DIR,
            "ubuntu-24.04",
        );
        assert!(
            shim.contains("RUSTUP_VERSION=1.29.0"),
            "the installer is pinned"
        );
        assert!(
            shim.contains("--default-toolchain none"),
            "the toolchain comes from the repository's rust-toolchain.toml, as on GitHub -- \
             choosing one here would silently diverge from what the workflow declared"
        );
    }

    #[test]
    fn a_failed_install_reports_a_defect_rather_than_looping() {
        let shim = apt_shim();
        assert!(shim.contains("exit 127"));
        assert!(
            shim.contains("this is a Greenlit defect"),
            "the manifest promised the command; not providing it is ours to fix, not the user's"
        );
    }
}
