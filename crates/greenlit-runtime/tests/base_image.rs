//! Real-daemon check that the convergent base image builds on first use, is
//! content-addressed and reused, and carries the expected tool set.
//!
//! `PHASE-2-execution.md` ("Base image and private init helper") and exit
//! criterion 5's image half: the base is built through the engine API, tagged
//! with the label and a content hash, and contains the slim tool set plus the
//! `greenlit-init` entrypoint.

#[path = "dockerkit/engine.rs"]
mod engine_support;
#[path = "dockerkit/sink.rs"]
mod sink_support;

use greenlit_runtime::engine::{ContainerEngine, ContainerSpec, ExecSpec};
use greenlit_runtime::{ProgressNull, UbuntuRelease, ensure_base_image};

use engine_support::{required_engine, unique_suffix};
use sink_support::CollectSink;

#[tokio::test]
async fn base_image_builds_reuses_and_carries_the_toolset() {
    let engine = required_engine("base_image_builds_reuses_and_carries_the_toolset").await;

    // First use builds it; the tag carries the resolved label.
    let tag = ensure_base_image(&engine, UbuntuRelease::Noble2404, &mut ProgressNull)
        .await
        .expect("base image builds");
    assert!(
        tag.starts_with("greenlit/base:ubuntu-24.04-"),
        "tag carries the label: {tag}"
    );
    assert!(
        engine.image_exists(&tag).await.expect("inspect"),
        "the built image is present"
    );

    // Content-addressed: a second ensure returns the same tag and does not
    // depend on a rebuild.
    let again = ensure_base_image(&engine, UbuntuRelease::Noble2404, &mut ProgressNull)
        .await
        .expect("reuse");
    assert_eq!(tag, again, "identical inputs converge on one tag");

    // Boot a container and confirm the slim tool set and the helper entrypoint
    // are present.
    let name = format!("greenlit-base-check-{}", unique_suffix());
    let spec = ContainerSpec {
        image: tag.clone(),
        name: Some(name.clone()),
        // Override the helper entrypoint so the container just idles for probes.
        entrypoint: vec!["sleep".to_string()],
        cmd: vec!["120".to_string()],
        ..ContainerSpec::default()
    };
    let id = engine.create_container(&spec).await.expect("create");
    let result = probe_toolset(&engine, &id).await;
    // Always tear the container down, then assert.
    let _ = engine.remove_container(&id).await;
    let (missing, helper_present) = result.expect("probe ran");

    assert!(missing.is_empty(), "missing tools: {missing:?}");
    assert!(
        helper_present,
        "greenlit-init helper is present in the image"
    );
}

/// Start `id` and probe for the expected tools and the helper binary.
async fn probe_toolset(
    engine: &dyn ContainerEngine,
    id: &str,
) -> Result<(Vec<String>, bool), Box<dyn std::error::Error>> {
    engine.start_container(id).await?;

    let mut sink = CollectSink::default();
    let script = "for t in bash sh git curl wget jq tar unzip gcc ca-certificates; do :; done; \
                  for t in bash git curl wget jq tar unzip gcc; do \
                  command -v \"$t\" >/dev/null 2>&1 || echo \"MISSING:$t\"; done; \
                  test -x /greenlit/bin/greenlit-init && echo HELPER_OK";
    let spec = ExecSpec {
        cmd: vec!["sh".to_string(), "-c".to_string(), script.to_string()],
        ..ExecSpec::default()
    };
    engine.exec(id, &spec, &mut sink).await?;

    let out = sink.out();
    let missing: Vec<String> = out
        .lines()
        .filter_map(|line| line.strip_prefix("MISSING:").map(str::to_string))
        .collect();
    let helper_present = out.contains("HELPER_OK");
    Ok((missing, helper_present))
}
