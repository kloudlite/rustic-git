//! `kloudlite-git-gateway`: the region's front door for SSH into a workspace.
//!
//! Two listeners on purpose. 443 is the real one — TLS with a Cloudflare Origin CA certificate,
//! bound to the node's public interface by `hostPort`, and the node firewall admits it from
//! Cloudflare's ranges only, so the edge is the only client that can complete a handshake. 8080 is
//! plaintext and cluster-internal: the health probe and the tests, nothing else. `GATEWAY_TLS_DIR`
//! set with no readable certificate is FATAL — falling back to plaintext there is a pod that
//! passes its probe and is unreachable from the edge. Unset is the laptop shape: HTTP only.

use kloudlite_git_core::jwt::Jwt;
use kloudlite_git_gateway::tunnel::{app, Gateway};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    kloudlite_git_core::log::init();
    kloudlite_git_core::metrics::init();
    // Its own listener: 8080 and 443 are both internet-facing here.
    kloudlite_git_core::metrics::serve_if_configured().await;
    // A gauge that is only ever incremented and decremented has no series until the first
    // tunnel opens, and an idle gateway then reads as "no samples" on the Signals page rather
    // than as zero. Register them so every series exists from boot.
    use kloudlite_git_core::metrics::Kind::*;
    kloudlite_git_core::metrics::register(&[
        ("gateway_open_tunnels", Gauge, &[]),
        ("http_request_duration_seconds", Histogram, &[]),
        ("http_requests_total", Counter, &[("listener", "gateway"), ("class", "probe"), ("status", "5xx")]),
        ("http_requests_total", Counter, &[("listener", "gateway"), ("class", "probe"), ("status", "421")]),
    ]);
    // Exactly one rustls CryptoProvider, installed before the first handshake — which for this
    // binary is the kube client, not the listener. Its absence is a panic inside rustls that names
    // nothing about startup order.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let secret = std::env::var("KLOUDLITE_GIT_JWT_SECRET").unwrap_or_default();
    let jwt = match Jwt::new(&secret) {
        Ok(j) => j,
        Err(e) => fatal(format!("KLOUDLITE_GIT_JWT_SECRET: {e}")),
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
    // The one object-store touch this binary makes: a minimal, read-only client for one key.
    // `object_store_views` (not `open_store`) so this stays free of the SlateDB pool machinery
    // every other tier needs and this one does not.
    match kloudlite_git_storage::config::object_store_views() {
        Ok((os, _mp)) => {
            if let Some(bytes) = kloudlite_git_storage::config::get_central(&os).await {
                match serde_json::from_slice(&bytes) {
                    Ok(doc) => gw.central.store(
                        kloudlite_git_core::settings::CentralSettings::from_env().merged_with(&doc),
                    ),
                    Err(e) => tracing::warn!(scope = "central", error = %e, "settings.invalid"),
                }
            }
            tokio::spawn(kloudlite_git_core::settings::refresh_central_beat(
                kloudlite_git_storage::config::central_fetch(os),
                gw.central.clone(),
            ));
        }
        // Not fatal: the gateway's own job (SSH tunnels) needs no object store at all, so an
        // unset/unreachable KLOUDLITE_GIT_S3_URL here means "central settings stay env-only",
        // never "the gateway cannot serve".
        // Info when there is simply no store URL (the dev/edge shape, true by design); warn only
        // when one was configured and could not be opened.
        Err(e) => {
            if std::env::var("KLOUDLITE_GIT_S3_URL").is_err() {
                tracing::info!(mode = "env-only", error = %e, "settings.central.unavailable");
            } else {
                tracing::warn!(mode = "env-only", error = %e, "settings.central.unavailable");
            }
        }
    }
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
            let addr: std::net::SocketAddr = ([0, 0, 0, 0], 443).into();
            tracing::info!(listener = "https", %addr, "listener.started");
            if let Err(e) = axum_server::bind_rustls(addr, cfg).serve(https.into_make_service()).await {
                // The TLS listener IS the product; losing it must not leave a pod that still
                // passes its health check on 8080.
                tracing::error!(listener = "https", addr = %addr, error = %e, "listener.failed");
                std::process::exit(1);
            }
        });
    } else {
        // True by design where TLS terminates at the edge: a mode, not a degradation.
        tracing::info!(mode = "plain-http", "tls.mode");
    }

    let l = match tokio::net::TcpListener::bind("0.0.0.0:8080").await {
        Ok(l) => l,
        Err(e) => fatal(format!("binding 8080: {e}")),
    };
    tracing::info!(listener = "http", addr = "0.0.0.0:8080", "listener.started");
    if let Err(e) = axum::serve(l, router).await {
        fatal(format!("serving: {e}"));
    }
}

fn fatal(msg: String) -> ! {
    tracing::error!(error = %msg, "process.exiting");
    std::process::exit(1)
}
