//! `oftd`: a Linux OFT Timer. Owns one [`NodeHost`] per Node, keeps crossings and sample
//! history, and serves the OFT Protocol (spec §6) with axum.
//!
//! The Node transport is a trait ([`link::NodeLink`]) so the same Timer runs against the
//! in-memory [`oft_sim::fake_node::FakeNode`] (`--fake`) or, later, a serial port.

pub mod api;
pub mod link;
pub mod mdns;
pub mod protocol;
pub mod serial;
pub mod settings;
pub mod timer;

pub use timer::{Timer, TimerConfig};

use std::net::SocketAddr;
use std::sync::Arc;

/// Bind, serve the OFT Protocol, and drive the Node. Returns the bound address.
pub async fn serve(timer: Arc<Timer>, addr: SocketAddr) -> std::io::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    let app = api::router(timer.clone());
    let server = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("oftd: server error: {e}");
        }
    });
    Ok((bound, server))
}
