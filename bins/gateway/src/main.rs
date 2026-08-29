//! `rustic-git-gateway`: the region's front door for SSH into a workspace.
//!
//! Two listeners on purpose. 443 is the real one — TLS with a Cloudflare Origin CA certificate,
//! bound to the node's public interface by `hostPort`, and the node firewall admits it from
//! Cloudflare's ranges only, so the edge is the only client that can complete a handshake. 8080 is
//! plaintext and cluster-internal: the health probe and the tests, nothing else. `GATEWAY_TLS_DIR`
//! set with no readable certificate is FATAL — falling back to plaintext there is a pod that
//! passes its probe and is unreachable from the edge. Unset is the laptop shape: HTTP only.

use rustic_git_core::jwt::Jwt;
use rustic_git_gateway::{app, Gateway};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    rustic_git_core::log::init();
    rustic_git_core::metrics::init();
    // Its own listener: 8080 and 443 are both internet-facing here.
    rustic_git_core::metrics::serve_if_configured().await;
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

    // The ENV VAR is the switch, not the file: set (as the Deployment always sets it) means TLS is
    // required and a missing or unreadable certificate is a boot failure. A pod that quietly fell
    // back to plaintext would keep passing its 8080 probe while being unreachable from the edge —
    // an outage that looks like Cloudflare's. Unset is the dev shape: HTTP only, on purpose.
    //
    // The certificate is read ONCE, here. Rotating it (Origin CA certificates last 15 years) means
    // restarting the pods; there is no reload watch, and adding one before the first rotation is
    // due would be code nobody has exercised.
    if let Ok(tls_dir) = std::env::var("GATEWAY_TLS_DIR") {
        let (crt, key) = (format!("{tls_dir}/tls.crt"), format!("{tls_dir}/tls.key"));
        let cfg = match axum_server::tls_rustls::RustlsConfig::from_pem_file(&crt, &key).await {
            Ok(c) => c,
            Err(e) => fatal(format!(
                "GATEWAY_TLS_DIR is set but {crt}/{key} could not be loaded ({e}) — \
                 refusing to serve plaintext where TLS was configured"
            )),
        };
        let https = router.clone();
        tokio::spawn(async move {
            tracing::info!("gateway tls on 0.0.0.0:443");
            let addr: std::net::SocketAddr = ([0, 0, 0, 0], 443).into();
            if let Err(e) = axum_server::bind_rustls(addr, cfg).serve(https.into_make_service()).await {
                // The TLS listener IS the product; losing it must not leave a pod that still
                // passes its health check on 8080.
                tracing::error!("tls listener: {e}");
                std::process::exit(1);
            }
        });
    } else {
        tracing::warn!("GATEWAY_TLS_DIR unset — serving plain HTTP only (dev)");
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
