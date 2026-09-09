//! The gate process: one plain TCP listener on 1234, and a `/healthz` on 8080 for the probes.
//!
//! No TLS and no auth on 1234 by design — the client is buildkit's gRPC, which carries neither,
//! and what makes the port safe is Task 6's NetworkPolicies plus the fact that the only thing on
//! the other side of a connection is that pod's OWN builder.

// A panicking request path is a dead pod (`panic = "abort"` in the release profile), so a
// `.unwrap()`/`.expect()` here is a decision, taken per site with an `allow` and its reason.
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use kloudlite_builder_gate::{idle, serve, who, ApiClient, Gate};
use kloudlite_core::settings::CentralSettings;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    kloudlite_core::log::init();
    kloudlite_core::metrics::init();
    kloudlite_core::metrics::serve_if_configured().await;
    // Every outcome exists from boot: a gate nobody has built through must read as zero, not as
    // a missing series (the same reason the gateway registers its gauge).
    use kloudlite_core::metrics::Kind::*;
    kloudlite_core::metrics::register(&[
        ("builder_gate_starts_total", Counter, &[("outcome", "ok")]),
        ("builder_gate_starts_total", Counter, &[("outcome", "timeout")]),
        ("builder_gate_starts_total", Counter, &[("outcome", "refused")]),
        ("builder_gate_starts_total", Counter, &[("outcome", "unknown_peer")]),
        ("builder_gate_starts_total", Counter, &[("outcome", "client_gone")]),
    ]);
    // `builder_gate_connections` is NOT registered: its one label is the owner, and the set of
    // owners is not known until somebody builds. Pre-registering it label-less would export a
    // second, permanently-zero series beside the real ones rather than making them exist.

    // Exactly one rustls CryptoProvider, installed before the first handshake — the kube client's
    // and reqwest's, neither of which can choose between `ring` and `aws-lc-rs` on its own.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Both required, fail closed: without the secret every api call is a 401 and the gate would
    // accept connections it can do nothing with.
    let secret = match std::env::var("KLOUDLITE_BUILDER_SECRET") {
        Ok(s) if !s.is_empty() => s,
        _ => fatal("KLOUDLITE_BUILDER_SECRET is required"),
    };
    let base = match std::env::var("KLOUDLITE_API_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => fatal("KLOUDLITE_API_URL is required"),
    };
    let kube = match kube::Client::try_default().await {
        Ok(c) => c,
        Err(e) => fatal(&format!("kube client: {e}")),
    };

    let pods = who::Pods::default();
    pods.spawn(kube);
    let health = kloudlite_builder_gate::health(pods.clone());
    let gate = Arc::new(Gate {
        api: ApiClient::new(base, secret),
        who: Arc::new(pods),
        idle: idle::Idle::default(),
        central: kloudlite_core::settings::LiveSettings::new(CentralSettings::from_env()),
        buildkit: None,
    });

    // The same one-key read the gateway makes, and not fatal for the same reason: without a store
    // the gate runs on its env/default timings rather than not running.
    match kloudlite_storage::config::object_store_views() {
        Ok((os, _mp)) => {
            if let Some(bytes) = kloudlite_storage::config::get_central(&os).await {
                match serde_json::from_slice(&bytes) {
                    Ok(doc) => gate.central.store(CentralSettings::from_env().merged_with(&doc)),
                    Err(e) => tracing::warn!(scope = "central", error = %e, "settings.invalid"),
                }
            }
            tokio::spawn(kloudlite_core::settings::refresh_central_beat(
                kloudlite_storage::config::central_fetch(os),
                gate.central.clone(),
            ));
        }
        Err(e) => tracing::info!(mode = "env-only", error = %e, "settings.central.unavailable"),
    }

    // Re-seed before accepting: a builder left running by the previous process has no connection
    // count here, and nothing else would ever stop it.
    match gate.api.running().await {
        Ok(slugs) => {
            for slug in &slugs {
                gate.idle.seed(slug);
            }
            tracing::info!(count = slugs.len(), "gate.seeded");
        }
        Err(e) => tracing::warn!(error = %e, "gate.seed.failed"),
    }
    tokio::spawn(idle::beat(gate.clone()));

    let hl = match tokio::net::TcpListener::bind("0.0.0.0:8080").await {
        Ok(l) => l,
        Err(e) => fatal(&format!("binding 8080: {e}")),
    };
    tokio::spawn(async move {
        if let Err(e) = axum::serve(hl, health).await {
            tracing::error!(listener = "health", error = %e, "listener.failed");
        }
    });

    let l = match tokio::net::TcpListener::bind("0.0.0.0:1234").await {
        Ok(l) => l,
        Err(e) => fatal(&format!("binding 1234: {e}")),
    };
    tracing::info!(listener = "gate", addr = "0.0.0.0:1234", "listener.started");
    loop {
        match l.accept().await {
            Ok((sock, peer)) => {
                let gate = gate.clone();
                tokio::spawn(serve(gate, sock, peer.ip()));
            }
            Err(e) => tracing::warn!(error = %e, "gate.accept.failed"),
        }
    }
}

fn fatal(msg: &str) -> ! {
    tracing::error!(error = %msg, "process.exiting");
    std::process::exit(1)
}
