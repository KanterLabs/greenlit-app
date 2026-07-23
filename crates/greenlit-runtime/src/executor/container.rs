//! `jobs.<id>.container` support: turning a resolved job-container request into
//! a safe [`ContainerSpec`](crate::engine::ContainerSpec), and rejecting every containment-breaking option
//! with a source-spanned fix.
//!
//! `PHASE-2-execution.md` ("Job and step execution semantics"): a job container
//! runs steps in the requested image with `env`, `ports`, named/anonymous
//! `volumes`, and *safe* `options`, and must "reject host bind mounts,
//! privileged mode, host networking, host PID/IPC namespaces, and other
//! containment-breaking options with a source-spanned fix. Private-registry
//! credentials are completed with auth in Phase 3." The security model is not
//! configurable off (`greenlit-v0-spec.md` "Security model"), so these
//! rejections are hard failures, never warnings.

use greenlit_workflow::Span;
use thiserror::Error;

use crate::engine::BindMount;

/// A resolved (all `${{ }}` finished) job-container request, with each value's
/// authored span retained for precise rejection messages.
#[derive(Debug, Clone)]
pub struct ResolvedContainer {
    /// The resolved image reference.
    pub image: String,
    /// Where `image:` was authored.
    pub image_span: Span,
    /// Whether the job authored `credentials:` (Phase 3 completes auth).
    pub has_credentials: bool,
    /// Where `credentials:` was authored, if present.
    pub credentials_span: Option<Span>,
    /// Resolved `env:` entries.
    pub env: Vec<(String, String)>,
    /// Resolved `volumes:` specs (`source:dest` or a bare `dest`).
    pub volumes: Vec<(String, Span)>,
    /// Resolved `ports:` specs.
    pub ports: Vec<(String, Span)>,
    /// Resolved raw `options:` string, if authored.
    pub options: Option<(String, Span)>,
}

/// A containment-breaking or not-yet-supported job-container request. Every
/// variant carries the authored span and, in its message, the one action that
/// resolves it (`AGENTS.md` UX invariant).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ContainerRejection {
    /// `credentials:` was supplied before Phase 3's auth landed.
    #[error(
        "{span}: private-registry `credentials:` are not available until Phase 3\n  fix: remove `credentials:` for now, or pre-pull the image and reference it by digest"
    )]
    Credentials {
        /// Where `credentials:` was authored.
        span: Span,
    },
    /// `--privileged` (or `--privileged=true`) was requested.
    #[error(
        "{span}: `--privileged` is refused — a privileged container defeats Greenlit's isolation\n  fix: remove `--privileged` from the job container `options:`"
    )]
    Privileged {
        /// Where `options:` was authored.
        span: Span,
    },
    /// Host networking (`--network host`) was requested.
    #[error(
        "{span}: host networking (`--network host`) is refused — workflow containers cannot reach the host LAN\n  fix: remove `--network host` from the job container `options:`"
    )]
    HostNetwork {
        /// Where `options:` was authored.
        span: Span,
    },
    /// The host PID namespace (`--pid host`) was requested.
    #[error(
        "{span}: sharing the host PID namespace (`--pid host`) is refused — it breaks container isolation\n  fix: remove `--pid host` from the job container `options:`"
    )]
    HostPid {
        /// Where `options:` was authored.
        span: Span,
    },
    /// The host IPC namespace (`--ipc host`) was requested.
    #[error(
        "{span}: sharing the host IPC namespace (`--ipc host`) is refused — it breaks container isolation\n  fix: remove `--ipc host` from the job container `options:`"
    )]
    HostIpc {
        /// Where `options:` was authored.
        span: Span,
    },
    /// A host bind mount (`-v /host:...`, `--mount type=bind,...`) was requested
    /// through `options:`.
    #[error(
        "{span}: host bind mounts through `options:` are refused — the host filesystem is never exposed to a workflow container\n  fix: remove the `-v`/`--mount` host bind; use a named `volumes:` entry instead"
    )]
    HostBindOption {
        /// Where `options:` was authored.
        span: Span,
    },
    /// A `volumes:` entry named a host path as its source.
    #[error(
        "{span}: the `volumes:` entry `{spec}` binds a host path — refused, the host filesystem is never exposed to a workflow container\n  fix: use a named volume (`name:{dest}`) with no host source"
    )]
    HostVolumeSource {
        /// Where the volume was authored.
        span: Span,
        /// The offending volume spec.
        spec: String,
        /// The container-side destination, for the fix suggestion.
        dest: String,
    },
    /// A `volumes:` entry's destination collides with the managed workspace
    /// path or a `/greenlit*` control path the isolation setup depends on.
    #[error(
        "{span}: the `volumes:` entry `{spec}` mounts over `{dest}`, which Greenlit's own workspace/overlay isolation uses — refused, this would let a workflow-controlled volume shadow or capture isolation state instead of the throwaway overlay\n  fix: mount the volume at a destination outside the job workspace and `/greenlit`"
    )]
    ReservedVolumeDestination {
        /// Where the volume was authored.
        span: Span,
        /// The offending volume spec.
        spec: String,
        /// The colliding destination.
        dest: String,
    },
    /// A capability/device/security escalation was requested.
    #[error(
        "{span}: `{flag}` is refused — it can escalate a workflow container out of its sandbox\n  fix: remove `{flag}` from the job container `options:`"
    )]
    Escalation {
        /// Where `options:` was authored.
        span: Span,
        /// The offending flag.
        flag: String,
    },
    /// An `options:` flag Greenlit v0 does not model. Failing loudly beats
    /// silently dropping a flag the author expected to take effect
    /// (`greenlit-v0-spec.md` "Zero config": fail immediately with a precise
    /// message rather than mysteriously).
    #[error(
        "{span}: job container option `{option}` is not supported in v0\n  fix: remove `{option}`; only the security-relevant options are interpreted in v0"
    )]
    UnsupportedOption {
        /// Where `options:` was authored.
        span: Span,
        /// The unsupported token.
        option: String,
    },
}

/// The safe container additions distilled from a validated request: extra binds
/// (named volumes only) to layer onto the isolation [`ContainerSpec`](crate::engine::ContainerSpec).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerAdditions {
    /// Named-volume binds (`name:dest`). Never a host path.
    pub volume_binds: Vec<BindMount>,
    /// Resolved container env to add.
    pub env: Vec<(String, String)>,
}

/// Validate a resolved job-container request, returning the safe additions or
/// the first containment-breaking rejection.
///
/// `workspace` (the container-side `GITHUB_WORKSPACE`) and `volume_namespace`
/// (unique per `litci run` invocation, see [`crate::RunConfig::volume_namespace`])
/// let volume validation reject a destination collision with Greenlit's own
/// isolation paths and namespace named-volume sources so they can never target
/// a pre-existing daemon-global volume.
///
/// # Errors
///
/// Returns a [`ContainerRejection`] for `credentials:`, any privileged / host
/// namespace / host bind request, a host-path volume source, a volume
/// destination colliding with the workspace or `/greenlit` control paths, an
/// escalation flag, or an unsupported option.
pub fn validate_container(
    container: &ResolvedContainer,
    workspace: &str,
    volume_namespace: &str,
) -> Result<ContainerAdditions, ContainerRejection> {
    if container.has_credentials {
        return Err(ContainerRejection::Credentials {
            span: container
                .credentials_span
                .clone()
                .unwrap_or_else(|| container.image_span.clone()),
        });
    }
    if let Some((options, span)) = &container.options {
        validate_options(options, span)?;
    }
    let mut volume_binds = Vec::new();
    for (spec, span) in &container.volumes {
        if let Some(bind) = validate_volume(spec, span, workspace, volume_namespace)? {
            volume_binds.push(bind);
        }
    }
    // `ports:` on a single job container publish nothing in v0's host-LAN-free
    // model (service networking is Phase 4); they are accepted syntactically so
    // a workflow that lists them still runs, but map to no host publish.
    Ok(ContainerAdditions {
        volume_binds,
        env: container.env.clone(),
    })
}

/// Split `options` and reject any containment-breaking or unsupported flag.
fn validate_options(options: &str, span: &Span) -> Result<(), ContainerRejection> {
    let tokens: Vec<&str> = options.split_whitespace().collect();
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        let (flag, inline_value) = match token.split_once('=') {
            Some((flag, value)) => (flag, Some(value)),
            None => (token, None),
        };
        // The value for a space-separated flag is the next token.
        let next_value = || inline_value.or_else(|| tokens.get(index + 1).copied());
        match flag {
            "--privileged" => {
                // `--privileged` or `--privileged=true`; `=false` is harmless.
                if inline_value.is_none_or(|v| !v.eq_ignore_ascii_case("false")) {
                    return Err(ContainerRejection::Privileged { span: span.clone() });
                }
            }
            "--network" | "--net" => {
                if next_value().is_some_and(is_host_value) {
                    return Err(ContainerRejection::HostNetwork { span: span.clone() });
                }
                return Err(ContainerRejection::UnsupportedOption {
                    span: span.clone(),
                    option: flag.to_string(),
                });
            }
            "--pid" => {
                if next_value().is_some_and(is_host_value) {
                    return Err(ContainerRejection::HostPid { span: span.clone() });
                }
                return Err(ContainerRejection::UnsupportedOption {
                    span: span.clone(),
                    option: flag.to_string(),
                });
            }
            "--ipc" => {
                if next_value().is_some_and(is_host_value) {
                    return Err(ContainerRejection::HostIpc { span: span.clone() });
                }
                return Err(ContainerRejection::UnsupportedOption {
                    span: span.clone(),
                    option: flag.to_string(),
                });
            }
            "-v" | "--volume" | "--mount" => {
                return Err(ContainerRejection::HostBindOption { span: span.clone() });
            }
            "--cap-add" | "--device" | "--security-opt" | "--userns" => {
                return Err(ContainerRejection::Escalation {
                    span: span.clone(),
                    flag: flag.to_string(),
                });
            }
            other => {
                return Err(ContainerRejection::UnsupportedOption {
                    span: span.clone(),
                    option: other.to_string(),
                });
            }
        }
        index += 1;
    }
    Ok(())
}

/// Whether a namespace-mode value selects the host namespace.
fn is_host_value(value: &str) -> bool {
    value.eq_ignore_ascii_case("host")
}

/// Validate one `volumes:` entry, returning a named-volume [`BindMount`] or
/// `None` for an anonymous (`dest`-only) volume, which needs no host source.
fn validate_volume(
    spec: &str,
    span: &Span,
    workspace: &str,
    volume_namespace: &str,
) -> Result<Option<BindMount>, ContainerRejection> {
    // Docker volume syntax: `source:dest[:mode]`, or a bare `dest` (anonymous).
    let parts: Vec<&str> = spec.split(':').collect();
    match parts.as_slice() {
        [dest] => {
            // Anonymous volume — a fresh writable dir at `dest`, no host source.
            // The container's own writable layer already provides one, so this
            // is accepted with no host exposure — but the destination itself
            // must still not collide with the isolation setup.
            reject_reserved_destination(spec, dest, span, workspace)?;
            Ok(None)
        }
        [source, dest, ..] => {
            reject_reserved_destination(spec, dest, span, workspace)?;
            if is_host_path(source) {
                return Err(ContainerRejection::HostVolumeSource {
                    span: span.clone(),
                    spec: spec.to_string(),
                    dest: (*dest).to_string(),
                });
            }
            Ok(Some(BindMount {
                // Namespaced so this source can never resolve to a
                // pre-existing daemon-global named volume (finding: "a named
                // source like `production_db:/loot` grants RW access to an
                // existing volume") — see `RunConfig::volume_namespace`.
                host_path: namespaced_volume_name(volume_namespace, source),
                container_path: (*dest).to_string(),
                read_only: false,
            }))
        }
        [] => Ok(None),
    }
}

/// Whether a volume source is a host filesystem path (absolute or relative)
/// rather than a named-volume identifier.
fn is_host_path(source: &str) -> bool {
    source.starts_with('/') || source.starts_with('.') || source.starts_with('~')
}

/// The Greenlit-managed control root every job container's isolation lives
/// under (`crate::isolation`'s `CONTAINER_LOWER`/`CONTAINER_UPPER_BASE`, and
/// the executor's per-step command-file/readiness paths) — a workflow-
/// declared volume must never mount over any path beneath it.
const GREENLIT_CONTROL_ROOT: &str = "/greenlit";

/// Rejects a volume destination that collides with the managed workspace path
/// or the `/greenlit` control root — as an exact match, an ancestor (which
/// would shadow the reserved path entirely), or a descendant (which would let
/// the workflow write into isolation-internal state).
fn reject_reserved_destination(
    spec: &str,
    dest: &str,
    span: &Span,
    workspace: &str,
) -> Result<(), ContainerRejection> {
    let dest = trim_trailing_slash(dest);
    let reserved = [GREENLIT_CONTROL_ROOT, workspace];
    if reserved
        .into_iter()
        .any(|path| paths_collide(dest, trim_trailing_slash(path)))
    {
        return Err(ContainerRejection::ReservedVolumeDestination {
            span: span.clone(),
            spec: spec.to_string(),
            dest: dest.to_string(),
        });
    }
    Ok(())
}

/// Strips one trailing `/` (never the root path itself).
fn trim_trailing_slash(path: &str) -> &str {
    if path.len() > 1 {
        path.trim_end_matches('/')
    } else {
        path
    }
}

/// Whether two absolute paths are the same path, or one is an ancestor
/// directory of the other.
fn paths_collide(a: &str, b: &str) -> bool {
    a == b || is_ancestor(a, b) || is_ancestor(b, a)
}

/// Whether `ancestor` is a proper ancestor directory of `other` (component-
/// boundary aware, so `/greenlitfoo` is not mistaken for a descendant of
/// `/greenlit`).
fn is_ancestor(ancestor: &str, other: &str) -> bool {
    other.len() > ancestor.len()
        && other.starts_with(ancestor)
        && (ancestor == "/" || other.as_bytes()[ancestor.len()] == b'/')
}

/// Namespaces a `volumes:` named-volume source so it can only ever resolve to
/// a volume created by (and scoped to) this one `litci run` invocation.
fn namespaced_volume_name(volume_namespace: &str, source: &str) -> String {
    format!("greenlit-run-{volume_namespace}-{source}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORKSPACE: &str = "/home/runner/work/repo/repo";
    const NAMESPACE: &str = "test-run";

    /// `validate_container` with fixed workspace/namespace values, for tests
    /// that don't exercise those parameters directly.
    fn validate(container: &ResolvedContainer) -> Result<ContainerAdditions, ContainerRejection> {
        validate_container(container, WORKSPACE, NAMESPACE)
    }

    fn span() -> Span {
        use greenlit_workflow::Location;
        Span::new(
            std::sync::Arc::from(".github/workflows/ci.yml"),
            Location::new(1, 1),
            Location::new(1, 2),
        )
    }

    fn base() -> ResolvedContainer {
        ResolvedContainer {
            image: "node:20".to_string(),
            image_span: span(),
            has_credentials: false,
            credentials_span: None,
            env: vec![("A".to_string(), "b".to_string())],
            volumes: Vec::new(),
            ports: Vec::new(),
            options: None,
        }
    }

    #[test]
    fn credentials_are_rejected_with_a_phase_3_fix() {
        let mut c = base();
        c.has_credentials = true;
        c.credentials_span = Some(span());
        let err = validate(&c).unwrap_err();
        assert!(matches!(err, ContainerRejection::Credentials { .. }));
        assert!(err.to_string().contains("Phase 3"));
    }

    #[test]
    fn privileged_and_host_namespaces_are_refused() {
        for (opt, want_privileged) in [("--privileged", true), ("--privileged=true", true)] {
            let mut c = base();
            c.options = Some((opt.to_string(), span()));
            let err = validate(&c).unwrap_err();
            assert_eq!(
                matches!(err, ContainerRejection::Privileged { .. }),
                want_privileged
            );
        }
        for opt in ["--network host", "--net=host"] {
            let mut c = base();
            c.options = Some((opt.to_string(), span()));
            assert!(matches!(
                validate(&c).unwrap_err(),
                ContainerRejection::HostNetwork { .. }
            ));
        }
        let mut c = base();
        c.options = Some(("--pid host".to_string(), span()));
        assert!(matches!(
            validate(&c).unwrap_err(),
            ContainerRejection::HostPid { .. }
        ));
        let mut c = base();
        c.options = Some(("--ipc=host".to_string(), span()));
        assert!(matches!(
            validate(&c).unwrap_err(),
            ContainerRejection::HostIpc { .. }
        ));
    }

    #[test]
    fn host_bind_options_and_escalations_are_refused() {
        for opt in [
            "-v /etc:/etc",
            "--volume /root:/root",
            "--mount type=bind,src=/x",
        ] {
            let mut c = base();
            c.options = Some((opt.to_string(), span()));
            assert!(matches!(
                validate(&c).unwrap_err(),
                ContainerRejection::HostBindOption { .. }
            ));
        }
        for opt in [
            "--cap-add SYS_ADMIN",
            "--device /dev/kmsg",
            "--security-opt seccomp=unconfined",
        ] {
            let mut c = base();
            c.options = Some((opt.to_string(), span()));
            assert!(matches!(
                validate(&c).unwrap_err(),
                ContainerRejection::Escalation { .. }
            ));
        }
    }

    #[test]
    fn unknown_options_fail_loudly() {
        let mut c = base();
        c.options = Some(("--cpus 2".to_string(), span()));
        assert!(matches!(
            validate(&c).unwrap_err(),
            ContainerRejection::UnsupportedOption { .. }
        ));
    }

    #[test]
    fn named_volumes_pass_but_host_volume_sources_are_refused() {
        let mut c = base();
        c.volumes = vec![("cache:/data".to_string(), span())];
        let additions = validate(&c).expect("named volume ok");
        // The source is namespaced to this run, never the bare authored name
        // (finding #2: a bare name could otherwise resolve to a pre-existing
        // daemon-global volume).
        assert_eq!(
            additions.volume_binds,
            vec![BindMount {
                host_path: format!("greenlit-run-{NAMESPACE}-cache"),
                container_path: "/data".to_string(),
                read_only: false,
            }]
        );

        let mut c = base();
        c.volumes = vec![("/host/dir:/data".to_string(), span())];
        assert!(matches!(
            validate(&c).unwrap_err(),
            ContainerRejection::HostVolumeSource { .. }
        ));

        // An anonymous volume needs no host source and is accepted.
        let mut c = base();
        c.volumes = vec![("/data".to_string(), span())];
        assert!(validate(&c).expect("anon ok").volume_binds.is_empty());
    }

    #[test]
    fn named_volume_source_is_namespaced_per_run_so_it_cannot_target_an_existing_volume() {
        // Two different runs (different namespaces) requesting the exact same
        // authored volume name must resolve to two different, non-colliding
        // Docker volume names — neither of which is the bare "production_db"
        // an attacker-controlled workflow might target on purpose.
        let mut c = base();
        c.volumes = vec![("production_db:/data".to_string(), span())];
        let a = validate_container(&c, WORKSPACE, "run-a")
            .expect("named volume ok")
            .volume_binds;
        let b = validate_container(&c, WORKSPACE, "run-b")
            .expect("named volume ok")
            .volume_binds;
        assert_ne!(a, b);
        for binds in [&a, &b] {
            assert_ne!(binds[0].host_path, "production_db");
            assert!(binds[0].host_path.contains("production_db"));
        }
    }

    #[test]
    fn volume_destinations_colliding_with_the_workspace_or_greenlit_root_are_refused() {
        for dest in [
            WORKSPACE,
            &format!("{WORKSPACE}/subdir"),
            "/", // an ancestor of both reserved roots
            "/greenlit",
            "/greenlit/upper/upper",
            "/greenlit/cmdfiles/step-0/output",
        ] {
            let mut c = base();
            c.volumes = vec![(format!("cache:{dest}"), span())];
            assert!(
                matches!(
                    validate(&c).unwrap_err(),
                    ContainerRejection::ReservedVolumeDestination { .. }
                ),
                "expected {dest} to collide"
            );

            // An anonymous volume at the same destination is refused too — the
            // collision is about the destination, not the source.
            let mut c = base();
            c.volumes = vec![(dest.to_string(), span())];
            assert!(matches!(
                validate(&c).unwrap_err(),
                ContainerRejection::ReservedVolumeDestination { .. }
            ));
        }

        // A sibling path that merely shares a prefix textually must not
        // false-positive.
        let mut c = base();
        c.volumes = vec![("cache:/greenlitfoo".to_string(), span())];
        assert!(validate(&c).is_ok());
    }
}
