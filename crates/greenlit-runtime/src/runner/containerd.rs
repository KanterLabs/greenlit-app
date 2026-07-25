//! Direct containerd/stargz control-plane client.
//!
//! Only the protobuf messages Greenlit sends or reads are defined here. Proto
//! unknown-field compatibility means this remains interoperable with newer
//! containerd servers without adding a build-time `protoc` dependency.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use hyper_util::rt::TokioIo;
use prost::Message;
use tonic::client::Grpc;
use tonic::codegen::http::uri::PathAndQuery;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Status};
use tonic_prost::ProstCodec;

/// Snapshotter plugin type reported by containerd introspection.
const SNAPSHOT_PLUGIN_TYPE: &str = "io.containerd.snapshotter.v1";
const PLUGINS_PATH: &str = "/containerd.services.introspection.v1.Introspection/Plugins";
const TRANSFER_PATH: &str = "/containerd.services.transfer.v1.Transfer/Transfer";

/// Direct client configuration for a containerd namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StargzConfig {
    /// containerd Unix socket.
    pub address: PathBuf,
    /// Namespace shared with the configured execution runtime.
    pub namespace: String,
    /// Snapshotter plugin id, normally `stargz`.
    pub snapshotter: String,
}

impl StargzConfig {
    /// Reads explicit host configuration. Absence means eager Docker fallback.
    #[must_use]
    pub fn from_environment() -> Option<Self> {
        let address = std::env::var_os("GREENLIT_CONTAINERD_ADDRESS")?;
        let namespace = std::env::var("GREENLIT_CONTAINERD_NAMESPACE").ok()?;
        let snapshotter = std::env::var("GREENLIT_CONTAINERD_SNAPSHOTTER").ok()?;
        (!namespace.is_empty() && !snapshotter.is_empty()).then_some(Self {
            address: PathBuf::from(address),
            namespace,
            snapshotter,
        })
    }
}

/// A connected containerd client that never invokes `ctr`, `nerdctl`, or
/// another subprocess.
#[derive(Clone)]
pub struct StargzClient {
    channel: Channel,
    config: StargzConfig,
}

impl StargzClient {
    /// Connects to the configured Unix socket and verifies that the selected
    /// snapshotter plugin is initialized.
    pub async fn connect(config: StargzConfig) -> Result<Self, StargzError> {
        let channel = connect_unix(&config.address).await?;
        let client = Self { channel, config };
        client.verify_snapshotter().await?;
        Ok(client)
    }

    /// Pulls and unpacks a digest-qualified image through the configured
    /// remote snapshotter. containerd returns when the remote snapshot is
    /// mountable; individual eStargz chunks remain demand-fetched and
    /// independently verified by the snapshotter.
    pub async fn prepare(&self, reference: &str) -> Result<(), StargzError> {
        let source = OciRegistry {
            reference: reference.to_string(),
            resolver: Some(RegistryResolver::default()),
        };
        let platform = Platform {
            os: "linux".to_string(),
            architecture: "amd64".to_string(),
            variant: String::new(),
            os_version: String::new(),
        };
        let destination = ImageStore {
            name: reference.to_string(),
            labels: HashMap::new(),
            platforms: vec![platform.clone()],
            all_metadata: false,
            manifest_limit: 1,
            unpacks: vec![UnpackConfiguration {
                platform: Some(platform),
                snapshotter: self.config.snapshotter.clone(),
            }],
        };
        let request = TransferRequest {
            source: Some(any_message(
                "containerd.types.transfer.OCIRegistry",
                &source,
            )),
            destination: Some(any_message(
                "containerd.types.transfer.ImageStore",
                &destination,
            )),
            options: Some(TransferOptions::default()),
        };
        let mut grpc = Grpc::new(self.channel.clone());
        grpc.ready().await.map_err(StargzError::Transport)?;
        let request = namespaced(request, &self.config.namespace)?;
        grpc.unary(
            request,
            PathAndQuery::from_static(TRANSFER_PATH),
            ProstCodec::default(),
        )
        .await
        .map(|_: tonic::Response<Empty>| ())
        .map_err(StargzError::Rpc)
    }

    async fn verify_snapshotter(&self) -> Result<(), StargzError> {
        let mut grpc = Grpc::new(self.channel.clone());
        grpc.ready().await.map_err(StargzError::Transport)?;
        let response: PluginsResponse = grpc
            .unary(
                Request::new(PluginsRequest {
                    filters: vec![format!(
                        "type=={SNAPSHOT_PLUGIN_TYPE},id=={}",
                        self.config.snapshotter
                    )],
                }),
                PathAndQuery::from_static(PLUGINS_PATH),
                ProstCodec::default(),
            )
            .await
            .map_err(StargzError::Rpc)?
            .into_inner();
        let plugin = response
            .plugins
            .into_iter()
            .find(|plugin| {
                plugin.r#type == SNAPSHOT_PLUGIN_TYPE && plugin.id == self.config.snapshotter
            })
            .ok_or_else(|| StargzError::SnapshotterUnavailable {
                snapshotter: self.config.snapshotter.clone(),
            })?;
        if plugin
            .exports
            .get("enable_remote_snapshot_annotations")
            .is_none_or(|value| value != "true")
        {
            return Err(StargzError::RemoteAnnotationsDisabled {
                snapshotter: self.config.snapshotter.clone(),
            });
        }
        let supports_linux_amd64 = plugin.platforms.is_empty()
            || plugin
                .platforms
                .iter()
                .any(|platform| platform.os == "linux" && platform.architecture == "amd64");
        if !supports_linux_amd64 {
            return Err(StargzError::PlatformUnavailable {
                snapshotter: self.config.snapshotter.clone(),
            });
        }
        Ok(())
    }
}

async fn connect_unix(path: &Path) -> Result<Channel, StargzError> {
    let socket = path.to_path_buf();
    Endpoint::try_from("http://[::]")
        .map_err(StargzError::Endpoint)?
        .connect_with_connector(tower::service_fn(move |_| {
            let socket = socket.clone();
            async move {
                tokio::net::UnixStream::connect(socket)
                    .await
                    .map(TokioIo::new)
            }
        }))
        .await
        .map_err(StargzError::Endpoint)
}

fn namespaced<T>(message: T, namespace: &str) -> Result<Request<T>, StargzError> {
    let mut request = Request::new(message);
    let value = namespace
        .parse()
        .map_err(|_| StargzError::InvalidNamespace(namespace.to_string()))?;
    request.metadata_mut().insert("containerd-namespace", value);
    Ok(request)
}

fn any_message<T: Message>(type_url: &str, message: &T) -> Any {
    Any {
        type_url: type_url.to_string(),
        value: message.encode_to_vec(),
    }
}

/// Direct containerd provider failure.
#[derive(Debug, thiserror::Error)]
pub enum StargzError {
    /// The configured socket endpoint could not be reached.
    #[error("containerd socket connection failed: {0}")]
    Endpoint(tonic::transport::Error),
    /// A channel became unavailable before an RPC.
    #[error("containerd channel is unavailable: {0}")]
    Transport(tonic::transport::Error),
    /// containerd rejected a request.
    #[error("containerd request failed: {0}")]
    Rpc(Status),
    /// Namespace cannot be represented as gRPC metadata.
    #[error("containerd namespace '{0}' is not valid metadata")]
    InvalidNamespace(String),
    /// The explicitly configured snapshotter is not initialized.
    #[error("containerd snapshotter '{snapshotter}' is unavailable")]
    SnapshotterUnavailable {
        /// Requested plugin id.
        snapshotter: String,
    },
    /// The proxy plugin does not expose the annotations required for remote
    /// snapshots through containerd's transfer service.
    #[error("containerd snapshotter '{snapshotter}' does not enable remote snapshot annotations")]
    RemoteAnnotationsDisabled {
        /// Requested plugin id.
        snapshotter: String,
    },
    /// The selected snapshotter cannot serve the v0 Linux amd64 platform.
    #[error("containerd snapshotter '{snapshotter}' does not support linux/amd64")]
    PlatformUnavailable {
        /// Requested plugin id.
        snapshotter: String,
    },
}

#[derive(Clone, PartialEq, Message)]
struct Empty {}

#[derive(Clone, PartialEq, Message)]
struct PluginsRequest {
    #[prost(string, repeated, tag = "1")]
    filters: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
struct PluginsResponse {
    #[prost(message, repeated, tag = "1")]
    plugins: Vec<Plugin>,
}

#[derive(Clone, PartialEq, Message)]
struct Plugin {
    #[prost(string, tag = "1")]
    r#type: String,
    #[prost(string, tag = "2")]
    id: String,
    #[prost(message, repeated, tag = "4")]
    platforms: Vec<Platform>,
    #[prost(map = "string, string", tag = "5")]
    exports: HashMap<String, String>,
}

#[derive(Clone, PartialEq, Message)]
struct Any {
    #[prost(string, tag = "1")]
    type_url: String,
    #[prost(bytes = "vec", tag = "2")]
    value: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct TransferRequest {
    #[prost(message, optional, tag = "1")]
    source: Option<Any>,
    #[prost(message, optional, tag = "2")]
    destination: Option<Any>,
    #[prost(message, optional, tag = "3")]
    options: Option<TransferOptions>,
}

#[derive(Clone, PartialEq, Message)]
struct TransferOptions {
    #[prost(string, tag = "1")]
    progress_stream: String,
}

#[derive(Clone, PartialEq, Message)]
struct OciRegistry {
    #[prost(string, tag = "1")]
    reference: String,
    #[prost(message, optional, tag = "2")]
    resolver: Option<RegistryResolver>,
}

#[derive(Clone, PartialEq, Message)]
struct RegistryResolver {
    #[prost(string, tag = "1")]
    auth_stream: String,
    #[prost(map = "string, string", tag = "2")]
    headers: HashMap<String, String>,
    #[prost(string, tag = "3")]
    host_dir: String,
    #[prost(string, tag = "4")]
    default_scheme: String,
    #[prost(int32, tag = "5")]
    http_debug: i32,
    #[prost(string, tag = "6")]
    logs_stream: String,
}

#[derive(Clone, PartialEq, Message)]
struct ImageStore {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(map = "string, string", tag = "2")]
    labels: HashMap<String, String>,
    #[prost(message, repeated, tag = "3")]
    platforms: Vec<Platform>,
    #[prost(bool, tag = "4")]
    all_metadata: bool,
    #[prost(uint32, tag = "5")]
    manifest_limit: u32,
    #[prost(message, repeated, tag = "10")]
    unpacks: Vec<UnpackConfiguration>,
}

#[derive(Clone, PartialEq, Message)]
struct UnpackConfiguration {
    #[prost(message, optional, tag = "1")]
    platform: Option<Platform>,
    #[prost(string, tag = "2")]
    snapshotter: String,
}

#[derive(Clone, PartialEq, Message)]
struct Platform {
    #[prost(string, tag = "1")]
    os: String,
    #[prost(string, tag = "2")]
    architecture: String,
    #[prost(string, tag = "3")]
    variant: String,
    #[prost(string, tag = "4")]
    os_version: String,
}
