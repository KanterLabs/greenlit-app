//! Optional per-user preparation daemon.
//!
//! The daemon never owns run correctness: the client still freezes, resolves,
//! locks, and executes through the same in-process path. It keeps the content
//! catalog hot and tracks relevant repository changes so later preparation
//! work can be scheduled without making the foreground command authoritative
//! on daemon availability.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use greenlit_engine::{SourceEntry, SourceSnapshot};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::cli::DaemonArgs;

const PROTOCOL_VERSION: u32 = 1;
const MAX_MESSAGE_BYTES: u64 = 64 * 1024;
const START_WAIT: Duration = Duration::from_secs(2);
const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const WATCH_INTERVAL: Duration = Duration::from_secs(2);
const MAX_WATCH_ENTRIES: usize = 20_000;
const MAX_WATCH_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Request {
    Ping {
        protocol: u32,
        binary: String,
    },
    Prepare {
        protocol: u32,
        binary: String,
        repository: PathBuf,
    },
    Shutdown {
        protocol: u32,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct Response {
    protocol: u32,
    binary: String,
    ok: bool,
    changed: bool,
    message: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct PreparedSnapshot {
    commit: String,
    dirty: bool,
    digest: String,
    entries: Vec<SourceEntry>,
}

struct WatchedRepository {
    fingerprint: String,
    generation: u64,
    preparation: Option<tokio::task::JoinHandle<()>>,
}

/// Notify the optional daemon about one repository without making it part of
/// the execution correctness path.
pub(crate) fn prepare(repository: &Path, disabled: bool) {
    if disabled || std::env::var_os("LITCI_TEST_DISABLE_DAEMON").is_some() {
        return;
    }
    if request(&Request::Prepare {
        protocol: PROTOCOL_VERSION,
        binary: env!("CARGO_PKG_VERSION").to_string(),
        repository: repository.to_path_buf(),
    })
    .is_ok()
    {
        return;
    }
    if start().is_err() {
        return;
    }
    let deadline = std::time::Instant::now() + START_WAIT;
    while std::time::Instant::now() < deadline {
        if request(&Request::Prepare {
            protocol: PROTOCOL_VERSION,
            binary: env!("CARGO_PKG_VERSION").to_string(),
            repository: repository.to_path_buf(),
        })
        .is_ok()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Run the hidden daemon lifecycle command.
pub(crate) fn command(args: DaemonArgs) -> anyhow::Result<std::process::ExitCode> {
    if args.shutdown {
        request(&Request::Shutdown {
            protocol: PROTOCOL_VERSION,
        })?;
        return Ok(std::process::ExitCode::SUCCESS);
    }
    if args.status {
        request(&Request::Ping {
            protocol: PROTOCOL_VERSION,
            binary: env!("CARGO_PKG_VERSION").to_string(),
        })?;
        println!(
            "Greenlit daemon is ready (protocol {PROTOCOL_VERSION}, binary {}).",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(std::process::ExitCode::SUCCESS);
    }
    serve()?;
    Ok(std::process::ExitCode::SUCCESS)
}

fn serve() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| anyhow::anyhow!("could not start daemon runtime: {error}"))?;
    runtime.block_on(serve_async())
}

async fn serve_async() -> anyhow::Result<()> {
    let socket = socket_path()?;
    let parent = socket
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("daemon socket has no parent"))?;
    fs::create_dir_all(&parent)
        .map_err(|error| anyhow::anyhow!("could not create daemon directory: {error}"))?;
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))
        .map_err(|error| anyhow::anyhow!("could not secure daemon directory: {error}"))?;
    if socket.exists() {
        if request(&Request::Ping {
            protocol: PROTOCOL_VERSION,
            binary: env!("CARGO_PKG_VERSION").to_string(),
        })
        .is_ok()
        {
            return Ok(());
        }
        fs::remove_file(&socket)
            .map_err(|error| anyhow::anyhow!("could not replace stale daemon socket: {error}"))?;
    }
    let listener = UnixListener::bind(&socket)
        .map_err(|error| anyhow::anyhow!("could not bind daemon socket: {error}"))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .map_err(|error| anyhow::anyhow!("could not secure daemon socket: {error}"))?;
    let socket_metadata = fs::metadata(&socket)
        .map_err(|error| anyhow::anyhow!("could not inspect daemon socket: {error}"))?;
    let _socket_guard = SocketGuard {
        path: socket,
        device: socket_metadata.dev(),
        inode: socket_metadata.ino(),
    };
    crate::doctor_cmd::reconcile_interrupted_runs(
        parent
            .parent()
            .ok_or_else(|| anyhow::anyhow!("daemon state directory has no Greenlit root"))?,
    )?;
    reconcile_runtime_resources(
        parent
            .parent()
            .ok_or_else(|| anyhow::anyhow!("daemon state directory has no Greenlit root"))?,
    )
    .await;
    let home = parent
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("daemon directory has no user home"))?;
    let mut watched = BTreeMap::new();
    let mut interval = tokio::time::interval(WATCH_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let idle = tokio::time::sleep(IDLE_TIMEOUT);
    tokio::pin!(idle);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted
                    .map_err(|error| anyhow::anyhow!("could not accept daemon client: {error}"))?;
                if !same_user(&stream)? {
                    continue;
                }
                if handle(stream, &mut watched, &home).await? {
                    return Ok(());
                }
                idle.as_mut().reset(tokio::time::Instant::now() + IDLE_TIMEOUT);
            }
            _ = interval.tick() => {
                refresh_watched(&mut watched, &home).await;
            }
            () = &mut idle => return Ok(()),
        }
    }
}

async fn reconcile_runtime_resources(litci_root: &Path) {
    use greenlit_runtime::{EngineState, SystemProber};

    let Some(home) = litci_root.parent() else {
        return;
    };
    let Ok(store) = greenlit_store::cas::CasStore::open(
        greenlit_store::cas::CasStore::default_path_under(home),
    ) else {
        return;
    };
    let Ok(report) = store.doctor() else {
        return;
    };
    if !report.is_consistent() || report.active_leases > 0 {
        return;
    }
    let EngineState::Available { endpoint } = greenlit_runtime::detect(&SystemProber::new()).await
    else {
        return;
    };
    let Ok(engine) = greenlit_runtime::DockerEngine::connect(&endpoint) else {
        return;
    };
    let _result = engine.reconcile_managed_resources().await;
}

async fn handle(
    stream: UnixStream,
    watched: &mut BTreeMap<PathBuf, WatchedRepository>,
    home: &Path,
) -> anyhow::Result<bool> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half).take(MAX_MESSAGE_BYTES.saturating_add(1));
    let mut bytes = Vec::new();
    reader
        .read_until(b'\n', &mut bytes)
        .await
        .map_err(|error| anyhow::anyhow!("could not read daemon request: {error}"))?;
    let response;
    let mut shutdown = false;
    if bytes.len() as u64 > MAX_MESSAGE_BYTES {
        response = error_response("daemon request exceeds 64 KiB");
    } else {
        match serde_json::from_slice::<Request>(&bytes) {
            Ok(Request::Ping { protocol, binary }) => {
                response = version_response(protocol, &binary, false);
            }
            Ok(Request::Prepare {
                protocol,
                binary,
                repository,
            }) => {
                if protocol != PROTOCOL_VERSION || binary != env!("CARGO_PKG_VERSION") {
                    response = version_response(protocol, &binary, false);
                } else {
                    match refresh_repository(watched, repository, home).await {
                        Ok(changed) => response = success_response(changed),
                        Err(message) => response = error_response(&message),
                    }
                }
            }
            Ok(Request::Shutdown { protocol }) => {
                shutdown = protocol == PROTOCOL_VERSION;
                response = success_response(false);
            }
            Err(error) => response = error_response(&format!("invalid daemon request: {error}")),
        }
    }
    let mut encoded = serde_json::to_vec(&response)
        .map_err(|error| anyhow::anyhow!("could not encode daemon response: {error}"))?;
    encoded.push(b'\n');
    write_half
        .write_all(&encoded)
        .await
        .map_err(|error| anyhow::anyhow!("could not write daemon response: {error}"))?;
    Ok(shutdown)
}

fn request(request: &Request) -> anyhow::Result<Response> {
    let socket = socket_path()?;
    let mut stream = std::os::unix::net::UnixStream::connect(&socket)
        .map_err(|error| anyhow::anyhow!("daemon is unavailable: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .map_err(|error| anyhow::anyhow!("could not configure daemon client: {error}"))?;
    serde_json::to_writer(&mut stream, request)
        .map_err(|error| anyhow::anyhow!("could not encode daemon request: {error}"))?;
    stream
        .write_all(b"\n")
        .map_err(|error| anyhow::anyhow!("could not send daemon request: {error}"))?;
    let mut bytes = Vec::new();
    stream
        .take(MAX_MESSAGE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| anyhow::anyhow!("could not read daemon response: {error}"))?;
    if bytes.len() as u64 > MAX_MESSAGE_BYTES {
        anyhow::bail!("daemon response exceeds 64 KiB");
    }
    let response: Response = serde_json::from_slice(&bytes)
        .map_err(|error| anyhow::anyhow!("invalid daemon response: {error}"))?;
    if response.protocol != PROTOCOL_VERSION
        || response.binary != env!("CARGO_PKG_VERSION")
        || !response.ok
    {
        anyhow::bail!("{}", response.message);
    }
    Ok(response)
}

fn start() -> anyhow::Result<()> {
    let executable = std::env::current_exe()
        .map_err(|error| anyhow::anyhow!("could not locate litci: {error}"))?;
    Command::new(executable)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!("could not start preparation daemon: {error}"))
}

async fn refresh_watched(watched: &mut BTreeMap<PathBuf, WatchedRepository>, home: &Path) {
    let repositories = watched.keys().cloned().collect::<Vec<_>>();
    for repository in repositories {
        let _result = refresh_repository(watched, repository, home).await;
    }
}

async fn refresh_repository(
    watched: &mut BTreeMap<PathBuf, WatchedRepository>,
    repository: PathBuf,
    home: &Path,
) -> Result<bool, String> {
    let canonical = fs::canonicalize(&repository)
        .map_err(|error| format!("could not inspect repository: {error}"))?;
    let fingerprint = repository_fingerprint(&canonical)?;
    if let Some(state) = watched.get(&canonical)
        && state.fingerprint == fingerprint
    {
        let preparation_running = state
            .preparation
            .as_ref()
            .is_some_and(|task| !task.is_finished());
        if preparation_running || has_ready_template(home, &canonical) {
            return Ok(false);
        }
    }
    let generation = watched
        .get(&canonical)
        .map_or(1, |state| state.generation.saturating_add(1));
    if let Some(task) = watched
        .get_mut(&canonical)
        .and_then(|state| state.preparation.take())
    {
        task.abort();
    }
    let task_repository = canonical.clone();
    let task_home = home.to_path_buf();
    let task_fingerprint = fingerprint.clone();
    let preparation = tokio::spawn(async move {
        background_prepare(task_repository, task_home, task_fingerprint, generation).await;
    });
    watched.insert(
        canonical,
        WatchedRepository {
            fingerprint,
            generation,
            preparation: Some(preparation),
        },
    );
    Ok(true)
}

async fn background_prepare(
    repository: PathBuf,
    home: PathBuf,
    fingerprint: String,
    generation: u64,
) {
    let capture_repository = repository.clone();
    let capture_home = home.clone();
    let capture_fingerprint = fingerprint.clone();
    let captured = tokio::task::spawn_blocking(move || {
        capture_source_template(
            &capture_repository,
            &capture_home,
            &capture_fingerprint,
            generation,
        )
    })
    .await;
    let Ok(Ok((temporary, ready))) = captured else {
        return;
    };
    if publish_source_template(&temporary, &ready).is_err() {
        let _result = fs::remove_dir_all(&temporary);
        return;
    }
    prefetch_repository_actions(&repository).await;
}

fn capture_source_template(
    repository: &Path,
    home: &Path,
    fingerprint: &str,
    generation: u64,
) -> Result<(PathBuf, PathBuf), String> {
    let key = repository_key(repository);
    let repository_templates = template_root(home).join("repos").join(&key);
    fs::create_dir_all(&repository_templates)
        .map_err(|error| format!("could not create source-template directory: {error}"))?;
    let fingerprint = identity_component(fingerprint);
    let temporary = repository_templates.join(format!(".tmp-{}-{generation}", std::process::id()));
    if temporary.exists() {
        fs::remove_dir_all(&temporary)
            .map_err(|error| format!("could not replace stale source preparation: {error}"))?;
    }
    fs::create_dir(&temporary)
        .map_err(|error| format!("could not create source preparation: {error}"))?;
    let snapshot = match SourceSnapshot::capture(repository, &temporary.join("source")) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let _result = fs::remove_dir_all(&temporary);
            return Err(error.to_string());
        }
    };
    let prepared = PreparedSnapshot {
        commit: snapshot.commit,
        dirty: snapshot.dirty,
        digest: snapshot.digest,
        entries: snapshot.entries,
    };
    let metadata = serde_json::to_vec(&prepared)
        .map_err(|error| format!("could not encode source preparation: {error}"))?;
    let metadata_path = temporary.join("snapshot.json");
    let mut file = File::create(&metadata_path)
        .map_err(|error| format!("could not create source preparation metadata: {error}"))?;
    file.write_all(&metadata)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("could not persist source preparation metadata: {error}"))?;
    File::open(&temporary)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("could not persist source preparation: {error}"))?;
    Ok((
        temporary,
        repository_templates.join(format!("ready-{fingerprint}")),
    ))
}

fn publish_source_template(temporary: &Path, ready: &Path) -> Result<(), String> {
    match fs::rename(temporary, ready) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            fs::remove_dir_all(temporary)
                .map_err(|remove| format!("could not discard duplicate preparation: {remove}"))?;
        }
        Err(error) => return Err(format!("could not publish source preparation: {error}")),
    }
    if let Some(parent) = ready.parent() {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                format!("could not persist source preparation publication: {error}")
            })?;
    }
    Ok(())
}

async fn prefetch_repository_actions(repository: &Path) {
    let token = crate::auth::current_token().ok().flatten();
    let Ok((config, _pinned)) = crate::run_cmd::build_action_runtime_config(token, false) else {
        return;
    };
    let workflows = repository.join(".github").join("workflows");
    let Ok(entries) = fs::read_dir(workflows) else {
        return;
    };
    let mut references = std::collections::BTreeSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file()
            || !matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("yml" | "yaml")
            )
        {
            continue;
        }
        let Some(source_name) = path.strip_prefix(repository).ok().and_then(Path::to_str) else {
            continue;
        };
        let Ok(workflow) = greenlit_workflow::parse_workflow_file_with_name(&path, source_name)
        else {
            continue;
        };
        let Ok(extraction) = greenlit_workflow::extract_static(&workflow) else {
            continue;
        };
        references.extend(extraction.uses.into_iter().map(|uses| uses.value));
    }
    for reference in references {
        let Ok(greenlit_actions::ActionRef::Repository(action)) =
            greenlit_actions::ActionRef::parse(&reference)
        else {
            continue;
        };
        let Ok(commit) = greenlit_actions::resolve::resolve_ref(
            config.resolver.as_ref(),
            &action.owner,
            &action.repo,
            &action.git_ref,
        )
        .await
        else {
            continue;
        };
        let _result = config
            .store
            .ensure_fetched(
                &action.owner,
                &action.repo,
                &commit,
                config.fetcher.as_ref(),
            )
            .await;
    }
}

/// Atomically claims one daemon-prepared source snapshot and verifies it
/// against the current repository before returning it to run evidence.
pub(crate) fn take_source_template(
    repository: &Path,
    destination: &Path,
) -> Option<Result<SourceSnapshot, greenlit_engine::SourceSnapshotError>> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    if !home.is_absolute() {
        return None;
    }
    let canonical = fs::canonicalize(repository).ok()?;
    let source = template_root(&home)
        .join("repos")
        .join(repository_key(&canonical));
    let mut candidates = fs::read_dir(&source)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("ready-"))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| right.cmp(left));
    let claims = template_root(&home).join("claims");
    if fs::create_dir_all(&claims).is_err() {
        return None;
    }
    for candidate in candidates {
        let claim = claims.join(format!(
            "{}-{}-{}",
            repository_key(&canonical),
            std::process::id(),
            monotonic_token()
        ));
        if fs::rename(&candidate, &claim).is_err() {
            continue;
        }
        let prepared = fs::read(claim.join("snapshot.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<PreparedSnapshot>(&bytes).ok());
        let Some(prepared) = prepared else {
            let _result = fs::remove_dir_all(&claim);
            continue;
        };
        let snapshot = SourceSnapshot {
            commit: prepared.commit,
            dirty: prepared.dirty,
            digest: prepared.digest,
            entries: prepared.entries,
            root: claim.join("source"),
        };
        match snapshot.verify_and_adopt(&canonical, destination) {
            Ok(snapshot) => {
                let _result = fs::remove_dir_all(&claim);
                return Some(Ok(snapshot));
            }
            Err(greenlit_engine::SourceSnapshotError::ChangedDuringCapture) => {
                let _result = fs::remove_dir_all(&claim);
            }
            Err(error) => {
                let _result = fs::remove_dir_all(&claim);
                return Some(Err(error));
            }
        }
    }
    None
}

fn template_root(home: &Path) -> PathBuf {
    home.join(".litci").join("daemon").join("templates")
}

fn has_ready_template(home: &Path, repository: &Path) -> bool {
    fs::read_dir(
        template_root(home)
            .join("repos")
            .join(repository_key(repository)),
    )
    .is_ok_and(|mut entries| {
        entries.any(|entry| {
            entry
                .ok()
                .and_then(|entry| entry.file_name().into_string().ok())
                .is_some_and(|name| name.starts_with("ready-"))
        })
    })
}

fn repository_key(repository: &Path) -> String {
    identity_component(&greenlit_engine::opaque_revision(
        repository.as_os_str().as_encoded_bytes(),
    ))
}

fn identity_component(identity: &str) -> String {
    identity
        .strip_prefix("sha256:")
        .unwrap_or(identity)
        .to_string()
}

fn monotonic_token() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos())
}

fn same_user(stream: &UnixStream) -> anyhow::Result<bool> {
    let credentials = stream
        .peer_cred()
        .map_err(|error| anyhow::anyhow!("could not authenticate daemon client: {error}"))?;
    Ok(credentials.uid() == rustix::process::getuid().as_raw())
}

fn socket_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    if !home.is_absolute() {
        anyhow::bail!("HOME is not absolute");
    }
    Ok(home.join(".litci").join("daemon").join("v1.sock"))
}

fn repository_fingerprint(repository: &Path) -> Result<String, String> {
    let canonical = fs::canonicalize(repository)
        .map_err(|error| format!("could not inspect repository: {error}"))?;
    let mut paths = Vec::new();
    collect_relevant(&canonical, &canonical, &mut paths)?;
    paths.sort();
    let mut material = Vec::new();
    for path in paths {
        let relative = path.strip_prefix(&canonical).map_err(|_| {
            "repository watcher encountered a path outside its canonical root".to_string()
        })?;
        let bytes = fs::read(&path)
            .map_err(|error| format!("could not inspect {}: {error}", relative.display()))?;
        if material
            .len()
            .saturating_add(relative.as_os_str().as_encoded_bytes().len())
            .saturating_add(bytes.len())
            > MAX_WATCH_BYTES
        {
            return Err("repository preparation inputs exceed 64 MiB".to_string());
        }
        material.extend_from_slice(relative.as_os_str().as_encoded_bytes());
        material.push(0);
        material.extend_from_slice(&bytes);
        material.push(0xff);
    }
    Ok(greenlit_engine::opaque_revision(&material))
}

fn collect_relevant(root: &Path, directory: &Path, paths: &mut Vec<PathBuf>) -> Result<(), String> {
    if paths.len() >= MAX_WATCH_ENTRIES {
        return Err("repository preparation inputs exceed 20,000 files".to_string());
    }
    for entry in fs::read_dir(directory)
        .map_err(|error| format!("could not watch {}: {error}", directory.display()))?
    {
        let entry = entry.map_err(|error| format!("could not read watched entry: {error}"))?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| "watched path escaped repository".to_string())?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("could not inspect {}: {error}", relative.display()))?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if relevant_directory(relative) {
                collect_relevant(root, &path, paths)?;
            }
        } else if file_type.is_file() && relevant_file(relative) {
            paths.push(path);
        }
    }
    Ok(())
}

fn relevant_directory(path: &Path) -> bool {
    path.starts_with(".github")
        || path == Path::new(".git")
        || path.starts_with(".git/refs")
        || (!path.starts_with(".git") && !path.starts_with("target") && !path.starts_with(".litci"))
}

fn relevant_file(path: &Path) -> bool {
    path == Path::new(".git/HEAD")
        || path.starts_with(".git/refs")
        || (!path.starts_with(".git") && !path.starts_with("target") && !path.starts_with(".litci"))
}

fn success_response(changed: bool) -> Response {
    Response {
        protocol: PROTOCOL_VERSION,
        binary: env!("CARGO_PKG_VERSION").to_string(),
        ok: true,
        changed,
        message: String::new(),
    }
}

fn error_response(message: &str) -> Response {
    Response {
        protocol: PROTOCOL_VERSION,
        binary: env!("CARGO_PKG_VERSION").to_string(),
        ok: false,
        changed: false,
        message: message.to_string(),
    }
}

fn version_response(protocol: u32, binary: &str, changed: bool) -> Response {
    let compatible = protocol == PROTOCOL_VERSION && binary == env!("CARGO_PKG_VERSION");
    Response {
        protocol: PROTOCOL_VERSION,
        binary: env!("CARGO_PKG_VERSION").to_string(),
        ok: compatible,
        changed,
        message: if compatible {
            String::new()
        } else {
            "daemon protocol or binary version differs; restart the daemon".to_string()
        },
    }
}

struct SocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let owns_path = fs::metadata(&self.path)
            .is_ok_and(|metadata| metadata.dev() == self.device && metadata.ino() == self.inode);
        if owns_path {
            let _result = fs::remove_file(&self.path);
        }
    }
}
