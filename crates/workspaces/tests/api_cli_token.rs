//! The CLI token's revocation check, and the 30 s memory in front of it.
//!
//! A `kl-connect` command makes several `/v1` calls, and each one was its own directory round trip
//! (2026-09-12). Positive answers are remembered for `CLI_LIVE_TTL`; nothing negative ever is.

use kloudlite_core::jwt::Jwt;
use kloudlite_workspaces::api::{router, ApiState, Directory, OwnerMaterial, TeamRole};
use kloudlite_workspaces::kube_test::{get, mock_client, Route};
use serde_json::json;
use std::sync::{atomic::{AtomicUsize, Ordering}, Arc};

struct CountingDir {
    calls: AtomicUsize,
    live: bool,
}

#[async_trait::async_trait]
impl Directory for CountingDir {
    async fn teams_for(&self, _u: &str) -> Vec<String> {
        Vec::new()
    }
    async fn is_live(&self, _jti: &str) -> bool {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.live
    }
    async fn for_owner(&self, _o: &str) -> Option<OwnerMaterial> {
        None
    }
    async fn authorized_keys_for_owner(&self, _o: &str) -> Option<String> {
        None
    }
    async fn owners_of(&self, _e: &str) -> Vec<String> {
        Vec::new()
    }
    async fn team_role(&self, _u: &str, _t: &str) -> Option<TeamRole> {
        None
    }
    async fn is_team(&self, _s: &str) -> bool {
        false
    }
    async fn ensure_user(&self, _e: &str, _n: &str, _u: &str) -> Result<(), String> {
        Err("no directory".into())
    }
    async fn add_superadmin(&self, _e: &str, _b: &str) -> Result<(), String> {
        Ok(())
    }
}

fn regions_route() -> Vec<Route> {
    vec![get(
        "/apis/kloudlite.io/v1alpha1/regions",
        json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "RegionList", "metadata": {}, "items": []}),
    )]
}

async fn serve(dir: Arc<CountingDir>) -> (String, Arc<Jwt>) {
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let (client, _rec) = mock_client(regions_route());
    let state = ApiState::new(jwt.clone()).with_kube(client).with_directory(dir);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router(Arc::new(state))).await.unwrap() });
    (format!("http://{addr}"), jwt)
}

async fn hit(base: &str, tok: &str) -> reqwest::StatusCode {
    reqwest::Client::new().get(format!("{base}/v1/regions")).bearer_auth(tok).send().await.unwrap().status()
}

#[tokio::test]
async fn a_live_cli_token_is_checked_once_and_then_remembered() {
    let dir = Arc::new(CountingDir { calls: AtomicUsize::new(0), live: true });
    let (base, jwt) = serve(dir.clone()).await;
    let (tok, _) = jwt.mint_cli("karthik@example.com", "K", Some("karthik")).unwrap();

    for _ in 0..3 {
        assert_eq!(hit(&base, &tok).await, 200);
    }
    assert_eq!(dir.calls.load(Ordering::SeqCst), 1, "three requests, one revocation lookup");
}

/// Nothing negative is ever remembered: a refused token costs a lookup EVERY time, so a directory
/// that starts answering yes is honoured at once, and a cached "no" can never outlive a fix.
#[tokio::test]
async fn a_refused_cli_token_is_asked_about_every_time() {
    let dir = Arc::new(CountingDir { calls: AtomicUsize::new(0), live: false });
    let (base, jwt) = serve(dir.clone()).await;
    let (tok, _) = jwt.mint_cli("karthik@example.com", "K", Some("karthik")).unwrap();

    for _ in 0..3 {
        assert_eq!(hit(&base, &tok).await, 401);
    }
    assert_eq!(dir.calls.load(Ordering::SeqCst), 3);
}

/// A session token carries no `jti` and is not revocable, so it never reaches the directory at all.
#[tokio::test]
async fn a_session_token_never_asks_the_directory() {
    let dir = Arc::new(CountingDir { calls: AtomicUsize::new(0), live: false });
    let (base, jwt) = serve(dir.clone()).await;
    let tok = jwt.mint("karthik@example.com", "K", Some("karthik")).unwrap();

    assert_eq!(hit(&base, &tok).await, 200);
    assert_eq!(dir.calls.load(Ordering::SeqCst), 0);
}
