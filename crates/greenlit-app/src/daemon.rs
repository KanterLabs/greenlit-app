//! Optional per-user preparation daemon.
//!
//! The daemon never owns run correctness: the client still freezes, resolves,
//! locks, and executes through the same in-process path. It keeps the content
//! catalog hot and tracks relevant repository changes so later preparation
//! work can be scheduled without making the foreground command authoritative
//! on daemon availability.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::cli::DaemonArgs;

const PROTOCOL_VERSION: u32 = 1;
const MAX_MESSAGE_BYTES: u64 = 64 * 1024;
const START_WAIT: Duration = Duration::from_secs(2);
const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
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
        .ok_or_else(|| anyhow::anyhow!("daemon socket has no parent"))?;
    fs::create_dir_all(parent)
        .map_err(|error| anyhow::anyhow!("could not create daemon directory: {error}"))?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
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
    let _socket_guard = SocketGuard(socket);
    let mut watched = BTreeMap::new();
    loop {
        let accepted = tokio::time::timeout(IDLE_TIMEOUT, listener.accept()).await;
        let Ok(accepted) = accepted else {
            return Ok(());
        };
        let (stream, _) =
            accepted.map_err(|error| anyhow::anyhow!("could not accept daemon client: {error}"))?;
        if !same_user(&stream)? {
            continue;
        }
        if handle(stream, &mut watched).await? {
            return Ok(());
        }
    }
}

async fn handle(
    stream: UnixStream,
    watched: &mut BTreeMap<PathBuf, String>,
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
                    match repository_fingerprint(&repository) {
                        Ok(fingerprint) => {
                            let changed = watched
                                .insert(repository, fingerprint.clone())
                                .is_none_or(|prior| prior != fingerprint);
                            response = success_response(changed);
                        }
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
        || path.starts_with(".git/refs")
        || (!path.starts_with(".git") && !path.starts_with("target") && !path.starts_with(".litci"))
}

fn relevant_file(path: &Path) -> bool {
    if path.starts_with(".github/workflows") || path == Path::new(".git/HEAD") {
        return true;
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name == "action.yml"
        || name == "action.yaml"
        || name.starts_with("Dockerfile")
        || name.ends_with(".lock")
        || matches!(
            name,
            "rust-toolchain.toml"
                | "rust-toolchain"
                | ".node-version"
                | ".python-version"
                | ".tool-versions"
                | "go.mod"
                | "go.sum"
                | "package.json"
        )
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

struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _result = fs::remove_file(&self.0);
    }
}
