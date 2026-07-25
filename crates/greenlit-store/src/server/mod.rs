//! The workflow-facing shim server.
//!
//! `PHASE-4-environment.md` ("Cache and artifacts"): "Implement a local shim
//! server (axum) on the job network exposing the endpoints those actions
//! call." The actions in a workflow are unmodified — they address the service
//! named by `ACTIONS_CACHE_URL` and authenticate with `ACTIONS_RUNTIME_TOKEN`,
//! exactly as on a hosted runner — so serving that protocol locally is what
//! makes `actions/cache@v4` work with no network egress and no per-action
//! special-casing.
//!
//! The shim binds on the Greenlit bridge gateway only. It is the one
//! host-side listener a workflow container is permitted to reach through the
//! network policy, which is why every route authenticates: the job network is
//! shared with service containers and Docker actions.
//!
//! # Binding is separate from serving
//!
//! The `archiveLocation` a cache hit returns has to carry the port the shim
//! actually ended up on, and the caller has to put that same port into the
//! job's `ACTIONS_CACHE_URL`. Port 0 means the kernel chooses, so the port is
//! only knowable after the bind — hence [`bind`] first, read
//! [`Bound::address`], build the state around it, then [`Bound::serve`].
//! Binding a fixed port instead would collide between two concurrent
//! `litci run` invocations on the same machine.

mod artifact_api;
mod cache_api;
mod state;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use tokio::net::TcpListener;

pub use state::ShimState;

/// How long a shutdown waits for in-flight requests before abandoning the
/// server task.
///
/// Graceful shutdown alone is not enough here: an HTTP client that keeps a
/// pooled keep-alive connection open holds the server task alive
/// indefinitely, because from the server's side an idle pooled socket is
/// indistinguishable from a connection about to send another request. A
/// workflow container is exactly such a client — `@actions/cache` runs on
/// Node's undici, which pools — so an unbounded graceful shutdown would hang
/// job teardown behind a socket nobody intends to use again. The grace period
/// lets a request that is genuinely mid-flight finish; anything still open
/// after it is dropped.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// A bound-but-not-yet-serving shim, holding its listener.
#[derive(Debug)]
pub struct Bound {
    listener: TcpListener,
    address: SocketAddr,
}

impl Bound {
    /// The address the listener is bound to, with the port the kernel chose.
    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Starts serving `state` on this listener.
    #[must_use]
    pub fn serve(self, state: ShimState) -> Shim {
        let router =
            artifact_api::routes(cache_api::routes(Router::new())).with_state(Arc::new(state));
        let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
        let address = self.address;

        let task = tokio::spawn(async move {
            let served = axum::serve(self.listener, router).with_graceful_shutdown(async move {
                // A dropped sender means the `Shim` was dropped, which is
                // also a shutdown request.
                let _ = shutdown_rx.await;
            });
            if let Err(error) = served.await {
                tracing::error!(target: "greenlit_store::shim", %error, "the cache shim stopped");
            }
        });

        Shim {
            address,
            shutdown: Some(shutdown),
            task,
        }
    }
}

/// A running shim, stopped by dropping it or by [`Shim::shutdown`].
#[derive(Debug)]
pub struct Shim {
    address: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Shim {
    /// The address the shim is listening on.
    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Stops the shim, waiting up to a short grace period for in-flight
    /// requests to finish before abandoning any connection still open.
    ///
    /// Dropping a [`Shim`] also stops it; this is the form that lets a caller
    /// observe teardown before it removes the network the shim was bound for.
    pub async fn shutdown(mut self) {
        if let Some(signal) = self.shutdown.take() {
            // The receiver is gone only if the server task already exited,
            // which is the state this method is trying to reach anyway.
            let _ = signal.send(());
        }
        if tokio::time::timeout(SHUTDOWN_GRACE, &mut self.task)
            .await
            .is_err()
        {
            // Only pooled-but-idle connections should ever reach this.
            tracing::debug!(
                target: "greenlit_store::shim",
                "the cache shim still had open connections after the grace period; dropping them"
            );
            self.task.abort();
        }
    }
}

impl Drop for Shim {
    fn drop(&mut self) {
        if let Some(signal) = self.shutdown.take() {
            let _ = signal.send(());
        }
        // A drop cannot await the grace period, so the task is abandoned
        // outright; `shutdown` is the form that waits.
        self.task.abort();
    }
}

/// Binds a shim listener on `bind_address` and an ephemeral port.
///
/// # Errors
/// Returns the bind failure if `bind_address` cannot be listened on.
pub async fn bind(bind_address: Ipv4Addr) -> std::io::Result<Bound> {
    let listener = TcpListener::bind(SocketAddr::from((bind_address, 0))).await?;
    let address = listener.local_addr()?;
    Ok(Bound { listener, address })
}
