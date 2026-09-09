//! The pump, and the connection count that decides when the builder goes away.

use crate::Gate;
use std::sync::Arc;

/// Held for exactly as long as one connection is charged to a builder. A guard rather than a
/// decrement at the end of `serve`: every failure path between the count and the copy — a refused
/// dial, a panic in the pump — must still release the builder, and a missed decrement is a
/// builder that never stops.
pub struct Open {
    gate: Arc<Gate>,
    slug: String,
}

impl Open {
    pub fn new(gate: Arc<Gate>, slug: String) -> Self {
        gate.idle.opened(&slug);
        metrics::gauge!("builder_gate_connections", "owner" => slug.clone()).increment(1.0);
        Self { gate, slug }
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        self.gate.idle.closed(&self.slug);
        metrics::gauge!("builder_gate_connections", "owner" => self.slug.clone()).decrement(1.0);
    }
}

/// Bytes, both ways, and nothing else — the gate never parses buildkit's gRPC, so it can never be
/// the thing that has to learn a new buildkit protocol version.
pub async fn splice(mut client: tokio::net::TcpStream, mut upstream: tokio::net::TcpStream) {
    if let Err(e) = tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
        // Expected on any half-closed build: logged at debug, never counted as a start failure.
        tracing::debug!(error = %e, "gate.splice.ended");
    }
}
