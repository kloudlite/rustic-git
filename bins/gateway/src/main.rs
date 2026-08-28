//! `rustic-git-gateway`: the region's front door for SSH into a workspace.
//!
//! Two listeners on purpose. 443 is the real one — TLS with a Cloudflare Origin CA certificate,
//! bound to the node's public interface by `hostPort`, and the node firewall admits it from
//! Cloudflare's ranges only, so the edge is the only client that can complete a handshake. 8080 is
//! plaintext and cluster-internal: the health probe and the tests, nothing else. A missing
//! certificate is not fatal — it is what `docker run` and a laptop look like, and failing closed
//! there would only mean the dev path needs a different binary.

use rustic_git_core::jwt::Jwt;
use rustic_git_gateway::{app, Gateway};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    rustic_git_core::log::init();
    // Exactly one rustls CryptoProvider, installed before the first handshake — which for this
    // binary is the kube client, not the listener. Its absence is a panic inside rustls that names
    // nothing about startup order.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let secret = std::env::var("RUSTIC_GIT_JWT_SECRET").unwrap_or_default();
    let jwt = match Jwt::new(&secret) {
        Ok(j) => j,
        Err(e) => fatal(format!("RUSTIC_GIT_JWT_SECRET: {e}")),
    };
    // No default region: a gateway that guessed one would accept tokens minted for somewhere else.
    let region = match std::env::var("WS_REGION") {
        Ok(r) if !r.is_empty() => r,
        _ => fatal("WS_REGION is required".into()),
    };
    let kube = match kube::Client::try_default().await {
        Ok(c) => c,
        Err(e) => fatal(format!("kube client: {e}")),
    };

    let gw = Arc::new(Gateway::new(jwt, region, kube, 22));
    let router = app(gw);

    let tls_dir = std::env::var("GATEWAY_TLS_DIR").unwrap_or_else(|_| "/etc/gateway/tls".into());
    let (crt, key) = (format!("{tls_dir}/tls.crt"), format!("{tls_dir}/tls.key"));
    if std::path::Path::new(&crt).exists() {
        match axum_server::tls_rustls::RustlsConfig::from_pem_file(&crt, &key).await {
            Ok(cfg) => {
                let https = router.clone();
                tokio::spawn(async move {
                    tracing::info!("gateway tls on 0.0.0.0:443");
                    let addr: std::net::SocketAddr = ([0, 0, 0, 0], 443).into();
                    if let Err(e) = axum_server::bind_rustls(addr, cfg)
                        .serve(https.into_make_service())
                        .await
                    {
                        // The TLS listener IS the product; losing it must not leave a pod that
                        // still passes its health check on 8080.
                        tracing::error!("tls listener: {e}");
                        std::process::exit(1);
                    }
                });
            }
            Err(e) => fatal(format!("reading {crt}: {e}")),
        }
    } else {
        tracing::warn!("{crt} absent — serving plain HTTP only (dev)");
    }

    let l = match tokio::net::TcpListener::bind("0.0.0.0:8080").await {
        Ok(l) => l,
        Err(e) => fatal(format!("binding 8080: {e}")),
    };
    tracing::info!("gateway on 0.0.0.0:8080");
    if let Err(e) = axum::serve(l, router).await {
        fatal(format!("serving: {e}"));
    }
}

fn fatal(msg: String) -> ! {
    tracing::error!("{msg}");
    std::process::exit(1)
}
