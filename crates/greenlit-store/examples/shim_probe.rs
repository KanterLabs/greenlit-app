//! A standalone shim on loopback, for reproducing a real client's behavior
//! against it with the full error visible.
//!
//! `@actions/artifact` reports SDK failures as `error.message`, which for the
//! Azure Blob SDK is often empty — so a failing upload inside a workflow says
//! nothing at all about what went wrong. This binds the same shim the runtime
//! serves, on loopback, so a host-side script using the real
//! `@azure/storage-blob` can drive it and print `error.stack`.
//!
//! ```text
//! cargo run -p greenlit-store --example shim_probe
//! ```

use std::net::Ipv4Addr;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::temp_dir().join("greenlit-shim-probe");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root)?;

    let bound = greenlit_store::bind(Ipv4Addr::LOCALHOST).await?;
    let base = format!("http://127.0.0.1:{}/", bound.address().port());
    let token = "probe-token";
    const SIGNATURE: &str = "probe-signature";

    println!("BASE {base}");
    println!("TOKEN {token}");
    println!("SIG {SIGNATURE}");
    println!("ROOT {}", root.display());

    let state = greenlit_store::ShimState::new(
        greenlit_store::CacheStore::at(root.join("cache")),
        greenlit_store::ArtifactStore::at(root.join("artifacts")),
        token,
        SIGNATURE,
        base,
    );
    let _shim = bound.serve(state);

    // Serve until killed.
    std::future::pending::<()>().await;
    Ok(())
}
