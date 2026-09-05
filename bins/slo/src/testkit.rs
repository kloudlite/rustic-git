//! A `Ctx` with no fleet behind it, and a one-route stub for the report tests.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use kloudlite_workspaces::slo::catalogue::Suite;

use crate::config::Config;
use crate::ctx::Ctx;

pub async fn ctx() -> Ctx {
    // `Ctx::new` builds the rustls-backed client, and the binary installs the provider in main().
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cfg = Config {
        admin_url: "http://127.0.0.1:1".into(),
        api_url: "http://127.0.0.1:1".into(),
        web_url: "http://127.0.0.1:1".into(),
        git_url: "http://127.0.0.1:1".into(),
        registry: "127.0.0.1:1".into(),
        ssh_host: "127.0.0.1".into(),
        region: "test".into(),
        hosts: vec![],
        origin_ip: None,
        jwt_secret: "0123456789abcdef0123456789abcdef".into(),
        ssh_key_path: "/dev/null".into(),
        ssh_hostkey: String::new(),
        canary_digest: None,
        azure: None,
        redis_host: None,
        probe_user: crate::ctx::PROBE_USER.into(),
        other_user: crate::ctx::OTHER_USER.into(),
    };
    Ctx::new(cfg, Suite::Fast, None).await.expect("ctx")
}

/// Serve a hand-built router and answer its base url. For the stage tests, which need particular
/// routes to fail rather than one blanket status.
pub async fn serve(app: axum::Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = l.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    format!("http://{addr}")
}

/// A `Ctx` whose `/v1` is `app`. Nothing else is reachable, which is the point: a stage that
/// reaches past the api under test fails loudly rather than hanging on a real hostname.
pub async fn ctx_against(app: axum::Router) -> Ctx {
    let url = serve(app).await;
    let mut c = ctx().await;
    c.cfg.api_url = url;
    // The stage tests write nothing, but `Ctx::tmp` is under the real temp dir and two tests must
    // not share one.
    static NTH: AtomicUsize = AtomicUsize::new(0);
    c.tmp = std::env::temp_dir()
        .join(format!("slo-test-{}-{}", std::process::id(), NTH.fetch_add(1, Ordering::SeqCst)));
    c
}

/// A server that answers every `PUT` with `status()` and counts the calls.
pub async fn stub(status: fn() -> axum::http::StatusCode) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let app = axum::Router::new().fallback(axum::routing::any(move || {
        let h = h.clone();
        async move {
            h.fetch_add(1, Ordering::SeqCst);
            status()
        }
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = l.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    (format!("http://{addr}"), hits)
}
