//! Parsing the GitHub runner-images manifest into a command → install recipe
//! table.
//!
//! `PHASE-4-environment.md` ("Convergent images"): "build a lookup of command
//! → tool → version → install recipe" from "the GitHub `runner-images`
//! manifest matching the resolved `ubuntu-24.04` or `ubuntu-22.04` label".
//!
//! # Two sources, because one is not enough
//!
//! * **`images/ubuntu/toolsets/toolset-<version>.json`** lists the apt
//!   packages the image installs (`apt.vital_packages`,
//!   `apt.common_packages`, `apt.cmd_packages`) and the versioned tools it
//!   pins (`cmake`, `php`, `postgresql`, `pwsh`, …).
//! * **`images/ubuntu/Ubuntu<version>-Readme.md`** lists what actually ended
//!   up on the image, with versions, as `- Name X.Y.Z` bullets under `###`
//!   headings.
//!
//! The Readme is not redundant. Rust is the case that proves it: the toolset
//! JSON has no `rust` key at all — the image installs rustup from a script —
//! yet `Rustup 1.29.0` is right there in the Readme's "Rust Tools" section.
//! A workflow running `rustup show` is completely ordinary, so a manifest
//! that could not describe it would be useless for exactly the repositories
//! most likely to try Greenlit.
//!
//! # What is deliberately *not* claimed
//!
//! For apt packages the manifest gives a package set and no versions, so a
//! provisioned apt package lands at whatever the distribution currently
//! ships. That is the same thing the hosted runner gets — it installs from
//! the same Ubuntu archive — but it is weaker than the brief's "at the exact
//! versions listed", which only genuinely holds for the tools carrying an
//! explicit version. The distinction is preserved in [`Recipe`] rather than
//! papered over, so a caller can tell a pinned install from an unpinned one.

use std::collections::BTreeMap;

/// How one command gets installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Recipe {
    /// An apt package, at whatever version the archive currently offers —
    /// which is what the hosted runner gets too, from the same archive.
    Apt {
        /// The package name.
        package: String,
    },
    /// The Rust toolchain manager, at the version the image carries.
    ///
    /// Installed through `rustup.rs`, which is how the runner image installs
    /// it; the toolchain itself then comes from the repository's own
    /// `rust-toolchain.toml`, exactly as on a hosted runner.
    Rustup {
        /// The pinned rustup version.
        version: String,
    },
}

impl Recipe {
    /// The version this recipe pins, when it pins one.
    pub(crate) fn pinned_version(&self) -> Option<&str> {
        match self {
            Recipe::Apt { .. } => None,
            Recipe::Rustup { version } => Some(version),
        }
    }
}

/// The commands one runner label's image is known to carry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Manifest {
    commands: BTreeMap<String, Recipe>,
}

impl Manifest {
    /// The recipe for `command`, if the image carries it.
    ///
    /// A command with no recipe gets no shim and fails the way it does on
    /// GitHub — `PHASE-4-environment.md`: "Commands not present in the
    /// matching GitHub image have no shim and fail normally."
    pub(crate) fn recipe(&self, command: &str) -> Option<&Recipe> {
        self.commands.get(command)
    }

    /// Builds a manifest from the two upstream documents.
    ///
    /// Both are parsed leniently: an upstream shape change should cost the
    /// commands it touches, not the whole manifest, because the alternative
    /// is a Greenlit release that stops provisioning anything the day
    /// `runner-images` reorganizes a file.
    pub(crate) fn parse(toolset_json: &str, readme_markdown: &str) -> Self {
        let mut commands = BTreeMap::new();
        parse_toolset(toolset_json, &mut commands);
        parse_readme(readme_markdown, &mut commands);
        Self { commands }
    }
}

/// Reads the apt package lists out of the toolset JSON.
///
/// Command name is taken to equal package name. That holds for the great
/// majority of the list (`jq`, `make`, `zip`, `parallel`, …) and, where it
/// does not, the effect is a command that simply has no shim — the same
/// outcome as a command the image never had.
fn parse_toolset(json: &str, commands: &mut BTreeMap<String, Recipe>) {
    let Ok(document) = serde_json::from_str::<serde_json::Value>(json) else {
        return;
    };
    let Some(apt) = document.get("apt") else {
        return;
    };
    for list in ["vital_packages", "common_packages", "cmd_packages"] {
        let Some(packages) = apt.get(list).and_then(serde_json::Value::as_array) else {
            continue;
        };
        for package in packages.iter().filter_map(serde_json::Value::as_str) {
            // A `-dev`/`lib*` package provides headers, not a command; a shim
            // for it would never be invoked, so it is skipped rather than
            // cluttering the table.
            if package.starts_with("lib") || package.ends_with("-dev") {
                continue;
            }
            commands
                .entry(package.to_string())
                .or_insert_with(|| Recipe::Apt {
                    package: package.to_string(),
                });
        }
    }
}

/// Reads versioned tools out of the Readme's `- Name X.Y.Z` bullets.
///
/// Only entries Greenlit knows how to install are taken; the rest of the
/// Readme is left alone. Today that is the Rust toolchain manager, which the
/// toolset JSON does not describe at all.
fn parse_readme(markdown: &str, commands: &mut BTreeMap<String, Recipe>) {
    for line in markdown.lines() {
        let Some(entry) = line.trim().strip_prefix("- ") else {
            continue;
        };
        let Some((name, version)) = entry.rsplit_once(' ') else {
            continue;
        };
        let version = version.trim();
        // A version is digits and dots; anything else is prose that happens
        // to sit in a bullet.
        if version.is_empty()
            || !version
                .chars()
                .all(|character| character.is_ascii_digit() || character == '.')
        {
            continue;
        }
        match name.trim().to_ascii_lowercase().as_str() {
            "rustup" => {
                commands.insert(
                    "rustup".to_string(),
                    Recipe::Rustup {
                        version: version.to_string(),
                    },
                );
            }
            // The runner image carries an interpreter the toolset JSON does
            // not list, because it arrives through the toolcache rather than
            // apt. Greenlit's slim base has none -- `ubuntu:24.04` ships no
            // `python3` -- so without this a perfectly ordinary
            // `run: python3 script.py` fails, as this repository's own
            // `python3 tools/check-stubs` step did.
            "python" => {
                commands
                    .entry("python3".to_string())
                    .or_insert_with(|| Recipe::Apt {
                        package: "python3".to_string(),
                    });
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Manifest, Recipe};

    /// Trimmed to the shapes that matter, verbatim from
    /// `images/ubuntu/toolsets/toolset-2404.json`.
    const TOOLSET: &str = r#"{
        "apt": {
            "vital_packages": ["curl", "jq", "make", "libssl-dev"],
            "common_packages": ["dpkg", "gnupg2"],
            "cmd_packages": ["parallel", "zip", "libnss3-tools"]
        },
        "node": { "default": "22" },
        "postgresql": { "version": "16" }
    }"#;

    /// Verbatim shape from `images/ubuntu/Ubuntu2404-Readme.md`.
    const README: &str = "### Rust Tools\n\
         - Cargo 1.97.0\n\
         - Rust 1.97.0\n\
         - Rustup 1.29.0\n\
         \n\
         ### Package Management\n\
         - Homebrew 4.4.0\n\
         - Miniconda is not a version\n";

    fn manifest() -> Manifest {
        Manifest::parse(TOOLSET, README)
    }

    #[test]
    fn every_apt_list_contributes_commands() {
        let manifest = manifest();
        for command in ["curl", "jq", "make", "dpkg", "gnupg2", "parallel", "zip"] {
            assert_eq!(
                manifest.recipe(command),
                Some(&Recipe::Apt {
                    package: command.to_string()
                }),
                "{command} is on the runner image and must be provisionable"
            );
        }
    }

    #[test]
    fn header_only_packages_get_no_shim() {
        // A shim for these would never be invoked -- nothing calls `libssl-dev`.
        let manifest = manifest();
        assert!(manifest.recipe("libssl-dev").is_none());
        assert!(manifest.recipe("libnss3-tools").is_none());
    }

    #[test]
    fn rustup_comes_from_the_readme_because_the_toolset_omits_it() {
        // The case that justifies parsing two documents: `rust` appears
        // nowhere in the toolset JSON, yet `rustup show` is an entirely
        // ordinary thing for a workflow to run.
        assert!(!TOOLSET.to_lowercase().contains("rust"));
        assert_eq!(
            manifest().recipe("rustup"),
            Some(&Recipe::Rustup {
                version: "1.29.0".to_string()
            })
        );
    }

    #[test]
    fn an_interpreter_the_toolset_omits_still_gets_a_recipe() {
        // Python reaches the runner image through the toolcache, not apt, and
        // Greenlit's slim base has none -- so `run: python3 …` needs a recipe
        // or it fails on an image that plainly has Python.
        let manifest = Manifest::parse(TOOLSET, "### Python\n- Python 3.12.3\n");
        assert_eq!(
            manifest.recipe("python3"),
            Some(&Recipe::Apt {
                package: "python3".to_string()
            })
        );
    }

    #[test]
    fn a_command_the_image_does_not_carry_has_no_recipe() {
        // `PHASE-4-environment.md`: such a command "fails normally".
        assert!(manifest().recipe("definitely-not-installed").is_none());
        assert!(manifest().recipe("kubectl").is_none());
    }

    #[test]
    fn prose_bullets_are_not_mistaken_for_versions() {
        assert!(manifest().recipe("Miniconda").is_none());
    }

    #[test]
    fn a_pinned_recipe_is_distinguishable_from_an_unpinned_one() {
        // apt gives a package set with no versions, so the honest answer for
        // an apt command is "no pinned version" rather than a version this
        // code invented.
        let manifest = manifest();
        assert_eq!(
            manifest.recipe("rustup").and_then(Recipe::pinned_version),
            Some("1.29.0")
        );
        assert_eq!(manifest.recipe("jq").and_then(Recipe::pinned_version), None);
    }

    #[test]
    fn a_broken_upstream_document_costs_only_what_it_describes() {
        // An upstream reorganization must not take the whole manifest with it.
        let json_only = Manifest::parse(TOOLSET, "not markdown at all");
        assert!(json_only.recipe("jq").is_some());
        assert!(json_only.recipe("rustup").is_none());

        let readme_only = Manifest::parse("{ this is not json", README);
        assert!(readme_only.recipe("jq").is_none());
        assert!(readme_only.recipe("rustup").is_some());
    }
}
