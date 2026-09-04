//! `kloudlite-git-agent`: the fleet-side process that materializes workspaces on local btrfs.
//!
//! `run` boots the node-scoped controller (`controller/`), which watches the CRDs bound to this
//! node and converges the local btrfs pool and its pods.

use kloudlite_git_agent::{run, Config};

#[tokio::main]
async fn main() {
    kloudlite_git_core::log::init();
    // Exactly one rustls CryptoProvider must be installed before the FIRST TLS handshake, which
    // for this binary is the kube client connecting to the API server. Its absence is not a
    // connection error — it is a panic in rustls that names nothing about kube or startup order.
    // The same omission crash-looped the api binary once; see the helper's own doc comment.
    kloudlite_git_storage::config::install_crypto_provider();

    kloudlite_git_core::metrics::init();
    kloudlite_git_core::metrics::serve_if_configured().await;
    // `ReconcileErrors` filters on `result="error"`, and a healthy agent never emits that series.
    use kloudlite_git_core::metrics::Kind::*;
    kloudlite_git_core::metrics::register(&[
        // The six kinds `controller::run`'s `timed` wraps, in its label order.
        ("reconciles_total", Counter, &[("kind", "volume"), ("result", "error")]),
        ("reconciles_total", Counter, &[("kind", "workspace"), ("result", "error")]),
        ("reconciles_total", Counter, &[("kind", "environment"), ("result", "error")]),
        ("reconciles_total", Counter, &[("kind", "claim"), ("result", "error")]),
        ("reconciles_total", Counter, &[("kind", "binding"), ("result", "error")]),
        ("reconciles_total", Counter, &[("kind", "snapshot"), ("result", "error")]),
        ("reconcile_duration_seconds", Histogram, &[]),
        ("node_pool_bytes_total", Gauge, &[]),
        ("node_pool_bytes_used", Gauge, &[]),
        ("node_working_copies_running", Gauge, &[]),
        // Outcome series. The label combinations are the ones a rule filters on: a gauge that is
        // absent until the first parked workspace reads as `unknown`, which is the opposite of the
        // `0` it means. Histograms stay untouched, per `Kind::Histogram`'s doc.
        ("workspace_start_duration_seconds", Histogram, &[]),
        ("workspaces_waiting", Gauge, &[("reason", "HomeNotReady")]),
        ("workspaces_waiting", Gauge, &[("reason", "NodeDead")]),
        ("workspaces_waiting", Gauge, &[("reason", "AwaitingReplica")]),
        ("snapshot_transfer_duration_seconds", Histogram, &[]),
        ("snapshot_transfer_bytes_total", Counter, &[("direction", "push")]),
        ("snapshot_transfer_bytes_total", Counter, &[("direction", "pull")]),
        ("replication_backlog", Gauge, &[]),
        ("snapshot_cut_failures_total", Counter, &[("kind", "workspace")]),
        ("snapshot_cut_failures_total", Counter, &[("kind", "environment")]),
    ]);
    if let Err(e) = run(Config::from_env()).await {
        tracing::error!(error = %e, "process.exiting");
        std::process::exit(1);
    }
}
