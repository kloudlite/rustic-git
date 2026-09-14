//! `kloudlite-controller`: one leader-elected process per k3s cluster.

use kloudlite_controller::{run, Config};

#[tokio::main]
async fn main() {
    kloudlite_core::log::init();
    kloudlite_core::metrics::init();
    kloudlite_core::metrics::serve_if_configured().await;
    // Exactly one rustls CryptoProvider before the first handshake — which for this binary is the
    // kube client. Its absence is a panic inside rustls naming nothing about startup order; it
    // crash-looped the api binary once.
    let _ = rustls::crypto::ring::default_provider().install_default();
    use kloudlite_core::metrics::Kind::*;
    kloudlite_core::metrics::register(&[
        ("reconciles_total", Counter, &[("kind", "space"), ("result", "error")]),
        ("reconciles_total", Counter, &[("kind", "environment"), ("result", "error")]),
        ("reconcile_duration_seconds", Histogram, &[]),
    ]);
    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "process.exiting");
            std::process::exit(1);
        }
    };
    if let Err(e) = run(cfg).await {
        tracing::error!(error = %e, "process.exiting");
        std::process::exit(1);
    }
}
