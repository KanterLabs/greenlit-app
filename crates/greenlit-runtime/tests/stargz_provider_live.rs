//! Real containerd/stargz provider acceptance.

use std::path::PathBuf;

use greenlit_runtime::runner::containerd::{StargzClient, StargzConfig};
use greenlit_runtime::runner::{OciRunnerProvider, RunnerProvider};
use greenlit_store::cas::CasStore;

const LIVE_ENV_VAR: &str = "LITCI_TEST_LIVE_STARGZ";
const IMAGE: &str = "ghcr.io/stargz-containers/ubuntu@sha256:8ae89e93cf60d297da92e0e1649261401b5d173746d285e3c91de73546f3b9d4";
const DIGEST: &str = "sha256:8ae89e93cf60d297da92e0e1649261401b5d173746d285e3c91de73546f3b9d4";

#[tokio::test]
async fn direct_provider_prepares_a_verified_remote_snapshot() {
    let Some(address) = std::env::var_os(LIVE_ENV_VAR) else {
        eprintln!(
            "direct_provider_prepares_a_verified_remote_snapshot: skipped \
             (set {LIVE_ENV_VAR} to a configured containerd socket)"
        );
        return;
    };
    let temp = tempfile::tempdir().expect("temporary provider store");
    let provider = OciRunnerProvider::new(
        CasStore::open(temp.path().join("cas")).expect("provider CAS"),
        false,
    );
    let manifest = provider
        .resolve(IMAGE, DIGEST)
        .await
        .expect("pinned fixture resolves through the verified OCI provider");
    assert!(
        manifest.lazy_compatible,
        "every fixture layer must carry a verified eStargz TOC identity"
    );

    let client = StargzClient::connect(StargzConfig {
        address: PathBuf::from(address),
        namespace: "greenlit-provider-acceptance-v2".to_string(),
        snapshotter: "stargz".to_string(),
    })
    .await
    .expect("configured stargz snapshotter is reachable");
    client
        .prepare(&manifest.pull_reference)
        .await
        .expect("direct provider prepares the pinned eStargz image");
}
