//! `actions/checkout` special-casing: its ref is locked before execution, then
//! it is satisfied without fetching or executing the real action source.
//!
//! `PHASE-3-actions.md`: "`actions/checkout` works tokenless for the
//! already-present local repo: detect self-checkout and satisfy it from the
//! isolated workspace instead of a network clone. Checkout of a different
//! repository performs a real clone and requires a token." This module is
//! reached the moment [`crate::executor::actions::resolve`] recognizes
//! `owner == "actions"` and `repo == "checkout"` in a step's `uses:` — the
//! real `actions/checkout` action source is never fetched or run; this
//! is a from-scratch, spec-scoped reimplementation of exactly the two
//! documented outputs (`ref`, `commit`) and the one input combination v0
//! needs (`repository`, `ref`, `token`, `path`).
//!
//! # Self vs. a different repository
//!
//! GitHub's `actions/checkout` defaults `with.repository` to
//! `${{ github.repository }}` — so "no `repository:` input" and "an
//! explicit `repository:` equal to the current repo" are the same case.
//! Either way the workspace the job container already has *is* that
//! checkout (Phase 2's overlay/copy-in isolation put it there), so
//! satisfying it costs no network and no extra work: this module only
//! computes the two outputs from already-known run metadata.
//!
//! A `repository:` naming a *different* repository requires a real,
//! authenticated `git` clone — run inside the job container (which has
//! internet egress; `AGENTS.md` "Security": "internet yes, host LAN no"),
//! mirroring the real action's own approach of persisting the credential as
//! a local `http.<url>.extraheader` git-config value (rather than embedding
//! it in a remote URL, which would leave it readable in shell history/
//! `ps` for the process's whole lifetime instead of only the fetch) and
//! removing that one config value in the post step
//! (<https://github.com/actions/checkout>, `src/git-auth-helper.ts`
//! configures/removes exactly this key) — reproduced here as
//! [`CheckoutPost::CredentialCleanup`].

use indexmap::IndexMap;

use greenlit_engine::execution::Masker;
use greenlit_engine::execution::env::RunnerEnv;

use crate::engine::{ContainerEngine, ExecOutputSink, ExecSpec, SinkNull};
use crate::error::RuntimeError;
use crate::executor::ExecError;

/// What a checkout step's post phase must do when the job ends.
#[derive(Debug, Clone)]
pub(crate) enum CheckoutPost {
    /// Self-checkout: nothing was ever written outside the workspace GitHub
    /// itself already isolated, so there is genuinely nothing to clean up.
    /// Still surfaced as a post entry (not simply omitted) so the run's step
    /// list/log shows a "Post <label>" line the way GitHub's own run does,
    /// even though its body is a documented no-op.
    Noop,
    /// A real clone persisted a credential header in the cloned repo's
    /// local git config; remove it so the token does not linger in a
    /// container-local file for the rest of the job.
    CredentialCleanup {
        /// Absolute in-container path of the cloned repository.
        repo_dir: String,
    },
}

/// The result of running `actions/checkout`'s (synthesized) main step.
pub(crate) struct CheckoutOutcome {
    /// `outputs.ref`/`outputs.commit`.
    pub outputs: IndexMap<String, String>,
    /// What this instance's post phase must do.
    pub post: CheckoutPost,
}

/// Everything one `actions/checkout` invocation needs, bundled so
/// [`execute`] takes one parameter instead of an unwieldy positional list.
pub(crate) struct CheckoutRequest<'a> {
    pub engine: &'a dyn ContainerEngine,
    pub container: &'a str,
    pub step_span: &'a greenlit_workflow::Span,
    /// This step's already-resolved `with:` map (workflow-authored `${{ }}`
    /// values, fully evaluated by the ordinary step `with:` machinery —
    /// `crate::executor::context::resolve_env_layer` — same as any other
    /// step, since these are workflow-authored, not manifest-authored,
    /// values).
    pub with: &'a IndexMap<String, String>,
    pub runner_env: &'a RunnerEnv,
    pub workspace: &'a str,
    pub github_token: Option<&'a str>,
}

/// Runs `actions/checkout`'s main step: self-satisfaction with no network,
/// or a real authenticated clone of a different repository.
///
/// # Errors
/// Returns [`ExecError::CheckoutRequiresAuth`] when `repository` names a
/// different repository and no token is configured, or an engine error if
/// the clone exec itself fails to dispatch (a non-zero clone exit is
/// reported as an ordinary ordinary step failure by the caller, not here).
pub(crate) async fn execute(
    request: CheckoutRequest<'_>,
    masker: &mut Masker,
) -> Result<CheckoutOutcome, ExecError> {
    let CheckoutRequest {
        engine,
        container,
        step_span,
        with,
        runner_env,
        workspace,
        github_token,
    } = request;
    let requested_repository = with.get("repository").map(String::as_str);
    let is_self = requested_repository.is_none_or(|r| r == runner_env.repository);

    let path = with
        .get("path")
        .filter(|p| !p.is_empty())
        .map_or_else(|| workspace.to_string(), |p| join_workspace(workspace, p));

    if is_self {
        // Self-checkout: the workspace the job container already has *is*
        // this checkout (Phase 2 overlay/copy-in isolation). No network, no
        // exec — matching `PHASE-3-actions.md`'s "satisfy it from the
        // isolated workspace instead of a network clone" exactly.
        let mut outputs = IndexMap::new();
        outputs.insert("ref".to_string(), runner_env.ref_full.clone());
        outputs.insert("commit".to_string(), runner_env.sha.clone());
        return Ok(CheckoutOutcome {
            outputs,
            post: CheckoutPost::Noop,
        });
    }

    let repository = requested_repository
        .unwrap_or(&runner_env.repository)
        .to_string();
    let git_ref = with
        .get("ref")
        .filter(|r| !r.is_empty())
        .cloned()
        .unwrap_or_else(|| runner_env.ref_full.clone());
    let token = with
        .get("token")
        .filter(|t| !t.is_empty())
        .map(String::from)
        .or_else(|| github_token.map(String::from));

    let Some(token) = token else {
        return Err(ExecError::CheckoutRequiresAuth {
            span: step_span.clone(),
            repository,
        });
    };
    masker.add(&token)?;

    let (sha_or_ref, is_full_sha) = (git_ref.clone(), looks_like_sha(&git_ref));
    let auth_header = format!(
        "AUTHORIZATION: basic {}",
        base64_basic(&format!("x-access-token:{token}"))
    );
    masker.add(&auth_header)?;
    let remote_url = format!("https://github.com/{repository}.git");
    let fetch_ref = if is_full_sha {
        sha_or_ref.clone()
    } else {
        strip_refs_prefix(&sha_or_ref).to_string()
    };

    let script = format!(
        "set -e\n\
         rm -rf {path}\n\
         mkdir -p {path}\n\
         git init --quiet {path}\n\
         git -C {path} remote add origin {remote_url}\n\
         git -C {path} config --local http.https://github.com/.extraheader {}\n\
         git -C {path} fetch --quiet --depth 1 origin {fetch_ref}\n\
         git -C {path} checkout --quiet FETCH_HEAD\n",
        shell_single_quote(&auth_header),
    );
    let spec = ExecSpec {
        cmd: vec!["sh".to_string(), "-c".to_string(), script],
        env: Vec::new(),
        working_dir: None,
    };
    let mut sink = SinkNull;
    let output = engine.exec(container, &spec, &mut sink).await?;
    if output.exit_code != 0 {
        return Err(ExecError::Infrastructure {
            message: format!(
                "checkout of '{repository}'@'{git_ref}' failed (git exited {})",
                output.exit_code
            ),
            fix: "verify the repository/ref exist and the token can read them".to_string(),
        });
    }

    let commit = read_commit(engine, container, &path).await?;
    let mut outputs = IndexMap::new();
    outputs.insert("ref".to_string(), git_ref);
    outputs.insert("commit".to_string(), commit);
    Ok(CheckoutOutcome {
        outputs,
        post: CheckoutPost::CredentialCleanup { repo_dir: path },
    })
}

/// Runs a checkout instance's post phase.
///
/// # Errors
/// Returns an engine error if the cleanup exec itself could not be
/// dispatched; a nonexistent config key is not an error (`git config
/// --unset-all` on an absent key is treated as success, matching the real
/// action's own tolerant cleanup).
pub(crate) async fn run_post(
    engine: &dyn ContainerEngine,
    container: &str,
    post: &CheckoutPost,
) -> Result<(), RuntimeError> {
    let CheckoutPost::CredentialCleanup { repo_dir } = post else {
        return Ok(());
    };
    let script = format!(
        "git -C {repo_dir} config --local --unset-all http.https://github.com/.extraheader 2>/dev/null || true"
    );
    let spec = ExecSpec {
        cmd: vec!["sh".to_string(), "-c".to_string(), script],
        env: Vec::new(),
        working_dir: None,
    };
    engine.exec(container, &spec, &mut SinkNull).await?;
    Ok(())
}

/// Reads the checked-out commit SHA back out with `git rev-parse HEAD`,
/// rather than trusting `git_ref` verbatim (a branch/tag name is not a
/// commit SHA, and `outputs.commit` must be one).
async fn read_commit(
    engine: &dyn ContainerEngine,
    container: &str,
    path: &str,
) -> Result<String, ExecError> {
    let spec = ExecSpec {
        cmd: vec![
            "git".to_string(),
            "-C".to_string(),
            path.to_string(),
            "rev-parse".to_string(),
            "HEAD".to_string(),
        ],
        env: Vec::new(),
        working_dir: None,
    };
    let mut sink = CaptureSink::default();
    let output = engine.exec(container, &spec, &mut sink).await?;
    if output.exit_code != 0 {
        return Err(ExecError::Infrastructure {
            message: "checkout succeeded but the resulting commit SHA could not be read"
                .to_string(),
            fix: "retry the run".to_string(),
        });
    }
    Ok(sink.text.trim().to_string())
}

#[derive(Default)]
struct CaptureSink {
    text: String,
}

impl ExecOutputSink for CaptureSink {
    fn on_stdout(&mut self, chunk: &[u8]) {
        self.text.push_str(&String::from_utf8_lossy(chunk));
    }
    fn on_stderr(&mut self, _chunk: &[u8]) {}
}

/// Joins a `with.path` input onto the workspace root the same way GitHub
/// does: relative to `GITHUB_WORKSPACE`, an absolute `path:` used verbatim.
fn join_workspace(workspace: &str, path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{}/{path}", workspace.trim_end_matches('/'))
    }
}

/// Whether `value` is already a full 40-hex-character commit SHA.
fn looks_like_sha(value: &str) -> bool {
    value.len() == 40 && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// Strips a `refs/heads/`/`refs/tags/` prefix so the remainder is a bare
/// name `git fetch` accepts directly; a value with no such prefix (e.g. a
/// short branch name) passes through unchanged.
fn strip_refs_prefix(value: &str) -> &str {
    value
        .strip_prefix("refs/heads/")
        .or_else(|| value.strip_prefix("refs/tags/"))
        .unwrap_or(value)
}

/// Single-quotes `value` for embedding as one `sh` argument, escaping any
/// embedded single quote with the standard `'\''` trick (close the quoted
/// string, emit an escaped literal quote, reopen).
fn shell_single_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Minimal base64 (standard alphabet, padded) encoder — avoids adding a
/// dependency for one short, fixed-shape credential string
/// (`x-access-token:<token>`), the same "basic auth" header value `git`'s
/// own HTTP transport expects for `http.<url>.extraheader`.
fn base64_basic(input: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
        out.push(if let Some(b1) = b1 {
            ALPHABET[(((b1 & 0x0f) << 2) | (b2.unwrap_or(0) >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if let Some(b2) = b2 {
            ALPHABET[(b2 & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}
