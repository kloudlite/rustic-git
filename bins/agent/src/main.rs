//! `rustic-git-agent`: the fleet-side process that materializes workspaces on local btrfs.
//!
//! `run` boots the node-scoped controller (`controller/`), which watches the CRDs bound to this
//! node and converges the local btrfs pool and its pods.

use rustic_git_agent::{run, Config};

#[tokio::main]
async fn main() {
    rustic_git_core::log::init();
    // Exactly one rustls CryptoProvider must be installed before the FIRST TLS handshake, which
    // for this binary is the kube client connecting to the API server. Its absence is not a
    // connection error — it is a panic in rustls that names nothing about kube or startup order.
    // The same omission crash-looped the api binary once; see the helper's own doc comment.
    rustic_git_storage::config::install_crypto_provider();

    rustic_git_core::metrics::init();
    rustic_git_core::metrics::serve_if_configured().await;
    if let Err(e) = run(Config::from_env()).await {
        tracing::error!("{e}");
        std::process::exit(1);
    }
}
