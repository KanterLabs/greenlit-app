//! `litci clean`: reclaim the caches and images Greenlit accumulates.
//!
//! `greenlit-v0-spec.md` ("CLI"): "remove converged images, caches, warm
//! pool". Everything this removes is *derived* — a cache entry, a fetched
//! action, a downloaded runtime, a built image — so the only cost of removing
//! it is that the next run rebuilds or refetches it.
//!
//! Two things it deliberately does not touch:
//!
//! * **`~/.litci/auth.json`** and the kernel keyring. Those are credentials,
//!   not a cache; wiping them would silently sign the user out of something
//!   they never asked to lose. `litci auth` owns that lifecycle.
//! * **`~/.litci/metrics/`**. The invocation history is the user's own record
//!   of their runs and the input to `litci stats` trends — derived from
//!   nothing, and not reproducible once deleted.
//!
//! Image removal needs a reachable container engine, but the on-disk caches
//! do not. An unreachable daemon therefore degrades rather than aborting: the
//! caches are still reclaimed, and the images are reported as skipped with
//! the state that prevented it.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use greenlit_runtime::{ContainerEngine, DockerEngine, EngineState, SystemProber, detect};

use crate::cli::CleanArgs;

/// The label every Greenlit-built image carries (`images/base/Dockerfile`),
/// inherited by per-repo convergent images through `docker commit`.
const IMAGE_LABEL: &str = "greenlit.managed=1";

/// One reclaimable location under `~/.litci/`.
struct Target {
    /// Directory name under `~/.litci/`.
    name: &'static str,
    /// What a user loses by removing it, phrased as the cost of the next run.
    cost: &'static str,
}

/// Every derived directory, in the order they are reported.
const TARGETS: &[Target] = &[
    Target {
        name: "cache",
        cost: "actions/cache entries; the next run repopulates them",
    },
    Target {
        name: "artifacts",
        cost: "uploaded artifacts from previous runs",
    },
    Target {
        name: "toolcache",
        cost: "toolchains setup-* actions installed; they reinstall on demand",
    },
    Target {
        name: "actions",
        cost: "fetched action sources; they refetch on demand",
    },
    Target {
        name: "node-runtimes",
        cost: "pinned runner Node bundles; roughly 100 MiB redownloads",
    },
];

/// What a clean would remove.
struct Plan {
    /// Present directories, with their measured sizes.
    directories: Vec<(PathBuf, &'static str, u64)>,
    /// Greenlit-built images, or why they could not be enumerated.
    images: Result<Vec<(String, u64)>, String>,
}

impl Plan {
    /// Whether there is nothing this clean could remove.
    ///
    /// An image listing that *failed* counts as empty: nothing is known to be
    /// removable, so announcing a removal would be misleading. The reason is
    /// still surfaced, so a user who expected images gone learns why they are
    /// not.
    fn is_empty(&self) -> bool {
        self.directories.is_empty()
            && self
                .images
                .as_ref()
                .map_or(true, |images| images.is_empty())
    }

    fn total_bytes(&self) -> u64 {
        let directories: u64 = self.directories.iter().map(|(_, _, size)| size).sum();
        let images: u64 = self
            .images
            .as_ref()
            .map(|images| images.iter().map(|(_, size)| size).sum())
            .unwrap_or_default();
        directories + images
    }
}

/// Run `litci clean`, returning the process exit code.
pub(crate) fn run(args: CleanArgs) -> anyhow::Result<ExitCode> {
    let home = home_dir()?;
    let root = home.join(".litci");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            anyhow::anyhow!("could not start the async runtime: {error}\n  fix: retry")
        })?;

    let plan = build_plan(&root, &runtime);

    if plan.is_empty() {
        println!("Nothing to clean — no Greenlit caches or images are present.");
        if let Err(reason) = &plan.images {
            println!("Images could not be listed: {reason}");
        }
        return Ok(ExitCode::SUCCESS);
    }

    report(&plan);

    if !confirm("Remove these?", args.yes)? {
        println!("No changes made.");
        return Ok(ExitCode::SUCCESS);
    }

    let reclaimed = apply(&plan, &runtime);
    println!("Reclaimed {}.", human_bytes(reclaimed));
    Ok(ExitCode::SUCCESS)
}

/// Measure what is present without removing anything.
fn build_plan(root: &Path, runtime: &tokio::runtime::Runtime) -> Plan {
    let directories = TARGETS
        .iter()
        .filter_map(|target| {
            let path = root.join(target.name);
            path.is_dir()
                .then(|| (path.clone(), target.cost, directory_size(&path)))
        })
        .collect();

    Plan {
        directories,
        images: runtime.block_on(list_images()),
    }
}

/// Greenlit's own images, or a description of why they are unreachable.
async fn list_images() -> Result<Vec<(String, u64)>, String> {
    let endpoint = match detect(&SystemProber::new()).await {
        EngineState::Available { endpoint } => endpoint,
        // Not an error: the on-disk caches are still reclaimable, so report
        // the state and carry on rather than failing the whole command.
        EngineState::DaemonStopped(fix)
        | EngineState::NotInstalled(fix)
        | EngineState::UnsupportedDockerHost(fix) => return Err(fix.message),
    };
    let engine = DockerEngine::connect(&endpoint)
        .map_err(|error| format!("could not connect to the container engine: {error}"))?;
    let images = engine
        .list_images(IMAGE_LABEL)
        .await
        .map_err(|error| format!("could not list images: {error}"))?;
    Ok(images
        .into_iter()
        .map(|image| {
            // An untagged image (a converged layer whose tag was replaced)
            // is still removable by id.
            let reference = image.tags.first().cloned().unwrap_or(image.id);
            (reference, image.size_bytes)
        })
        .collect())
}

/// Print what would be removed, before asking.
fn report(plan: &Plan) {
    println!("This will remove:");
    for (path, cost, size) in &plan.directories {
        println!("  {:>9}  {}", human_bytes(*size), path.display());
        println!("             {cost}");
    }
    match &plan.images {
        Ok(images) => {
            for (reference, size) in images {
                println!("  {:>9}  image {reference}", human_bytes(*size));
            }
        }
        Err(reason) => {
            println!("  images could not be listed: {reason}");
            println!("             caches below are still reclaimable");
        }
    }
    println!();
    println!("Total: {}", human_bytes(plan.total_bytes()));
    println!("Credentials and run history are not touched.");
}

/// Remove everything in `plan`, returning the bytes actually reclaimed.
///
/// A removal that fails is reported and skipped: partial reclamation is more
/// useful than aborting, and nothing here is load-bearing.
fn apply(plan: &Plan, runtime: &tokio::runtime::Runtime) -> u64 {
    let mut reclaimed = 0;
    for (path, _, size) in &plan.directories {
        match std::fs::remove_dir_all(path) {
            Ok(()) => reclaimed += size,
            Err(error) => {
                eprintln!("could not remove {}: {error}", path.display());
            }
        }
    }

    if let Ok(images) = &plan.images
        && !images.is_empty()
    {
        reclaimed += runtime.block_on(remove_images(images));
    }
    reclaimed
}

/// Remove each image, returning the bytes reclaimed.
async fn remove_images(images: &[(String, u64)]) -> u64 {
    let endpoint = match detect(&SystemProber::new()).await {
        EngineState::Available { endpoint } => endpoint,
        _ => return 0,
    };
    let Ok(engine) = DockerEngine::connect(&endpoint) else {
        return 0;
    };
    let mut reclaimed = 0;
    for (reference, size) in images {
        match engine.remove_image(reference).await {
            Ok(()) => reclaimed += size,
            Err(error) => eprintln!("could not remove image {reference}: {error}"),
        }
    }
    reclaimed
}

/// Total size of everything under `path`.
///
/// Traversal failures are counted as zero rather than aborting: the number is
/// a reporting aid, and an unreadable subtree must not stop a clean.
fn directory_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            // Symlinks are not followed: a link's target may live outside the
            // store entirely, and its bytes are not ours to count or reclaim.
            Ok(kind) if kind.is_dir() => directory_size(&entry.path()),
            Ok(kind) if kind.is_file() => entry.metadata().map(|meta| meta.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

/// The user's home directory, which every store roots under.
fn home_dir() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| {
        anyhow::anyhow!(
            "the HOME environment variable is not set\n  fix: set HOME to your home directory"
        )
    })?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        anyhow::bail!(
            "the HOME environment variable is not an absolute path\n  fix: set HOME to an absolute path"
        );
    }
    Ok(home)
}

/// Ask once before removing anything. Mirrors `setup_cmd`'s prompt so both
/// destructive commands read the same.
fn confirm(question: &str, pre_confirmed: bool) -> anyhow::Result<bool> {
    if pre_confirmed {
        return Ok(true);
    }
    print!("{question} [y/N] ");
    io::stdout().flush().ok();
    let mut answer = String::new();
    let stdin = io::stdin();
    if stdin.lock().read_line(&mut answer).is_err() {
        // An unreadable prompt is "no" — nothing is removed unasked.
        return Ok(false);
    }
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Render a byte count for a human, in the largest unit that keeps it small.
fn human_bytes(bytes: u64) -> String {
    // Integer arithmetic throughout: a float here would only introduce
    // rounding to display one decimal place.
    const UNITS: [(&str, u64); 4] = [
        ("GiB", 1024 * 1024 * 1024),
        ("MiB", 1024 * 1024),
        ("KiB", 1024),
        ("B", 1),
    ];
    for (suffix, scale) in UNITS {
        if bytes >= scale {
            if scale == 1 {
                return format!("{bytes} B");
            }
            let whole = bytes / scale;
            let tenths = (bytes % scale) * 10 / scale;
            return format!("{whole}.{tenths} {suffix}");
        }
    }
    "0 B".to_string()
}

#[cfg(test)]
mod tests {
    use super::human_bytes;

    #[test]
    fn byte_counts_render_in_the_largest_small_unit() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }
}
