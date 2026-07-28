//! Live Docker capability helpers.

use greenlit_runtime::DockerEngine;
use greenlit_runtime::detect::Endpoint;
use greenlit_runtime::engine::ContainerEngine;

/// Connect to the Docker capability the owning live-runtime job promises.
///
/// Missing or unresponsive Docker is a test failure: portable jobs exclude
/// these binaries through Cargo's `required-features` routing.
pub async fn required_engine(test: &str) -> DockerEngine {
    let engine = DockerEngine::connect(&Endpoint::DockerSocket).unwrap_or_else(|error| {
        panic!(
            "{test}: live-runtime-tests requires a reachable Docker daemon; \
             start Docker and make its socket available, then retry: {error}"
        )
    });
    engine
        .image_exists("greenlit/probe:definitely-absent")
        .await
        .unwrap_or_else(|error| {
            panic!(
                "{test}: Docker was configured but did not answer the required live-runtime \
                 probe; restore the daemon, then retry: {error}"
            )
        });
    engine
}

/// A unique-per-run container/name suffix.
pub fn unique_suffix() -> String {
    format!("{}-{:?}", std::process::id(), std::thread::current().id()).replace(['(', ')', ' '], "")
}
