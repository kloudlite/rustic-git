//! `GET /v1/bench/teams`: the desktop app's team picker. Only the verified caller's current teams,
//! each as `{slug, name, region}` ("" when unbound); every refusal is bodiless of detail, every
//! answer is `no-store`, and an anonymous or forged request never reaches the membership lookup.

use kloudlite_core::jwt::Jwt;
use kloudlite_workspaces::api::{router, ApiState, Directory, OwnerMaterial, TeamRole};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};

struct Dir {
    live: bool,
    up: AtomicBool,
    lookups: AtomicUsize,
    /// (handle, team)
    members: Mutex<Vec<(&'static str, &'static str)>>,
}

impl Dir {
    fn new(live: bool) -> Arc<Self> {
        Arc::new(Dir {
            live,
            up: AtomicBool::new(true),
            lookups: AtomicUsize::new(0),
            members: Mutex::new(vec![("karthik", "acme"), ("karthik", "fresh"), ("meera", "other")]),
        })
    }
}

#[async_trait::async_trait]
impl Directory for Dir {
    async fn teams_for(&self, _u: &str) -> Vec<String> {
        panic!("the picker must use the fail-closed member_teams")
    }
    async fn is_live(&self, _jti: &str) -> bool {
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
        true
    }
    async fn ensure_user(&self, _e: &str, _n: &str, _u: &str) -> Result<(), String> {
        Ok(())
    }
    async fn add_superadmin(&self, _e: &str, _b: &str) -> Result<(), String> {
        Ok(())
    }
    async fn member_teams(&self, user: &str) -> Result<Vec<String>, String> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        if !self.up.load(Ordering::SeqCst) {
            return Err("mongo: connection refused at 10.0.0.9:27017".into());
        }
        Ok(self.members.lock().unwrap().iter().filter(|(u, _)| *u == user).map(|(_, t)| t.to_string()).collect())
    }
    async fn bench_team(&self, slug: &str) -> Result<Option<(String, String)>, String> {
        let region = if slug == "fresh" { "" } else { "r1" };
        Ok(Some((format!("{slug} inc"), region.to_string())))
    }
}

async fn serve(dir: Arc<Dir>) -> (String, Arc<Jwt>) {
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let state = ApiState::new(jwt.clone()).with_directory(dir);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router(Arc::new(state))).await.unwrap() });
    (format!("http://{addr}"), jwt)
}

struct Answer {
    status: u16,
    no_store: bool,
    text: String,
}

async fn get(base: &str, path: &str, tok: Option<&str>) -> Answer {
    let mut req = reqwest::Client::new().get(format!("{base}{path}"));
    if let Some(t) = tok {
        req = req.bearer_auth(t);
    }
    let r = req.send().await.unwrap();
    let no_store = r.headers().get("cache-control").is_some_and(|v| v == "no-store");
    Answer { status: r.status().as_u16(), no_store, text: r.text().await.unwrap() }
}

fn cli(jwt: &Jwt, handle: &str) -> String {
    jwt.mint_cli(&format!("{handle}@example.com"), "N", Some(handle)).unwrap().0
}

#[tokio::test]
async fn no_token_is_401_no_store_and_never_reaches_the_directory() {
    let dir = Dir::new(true);
    let (base, _) = serve(dir.clone()).await;
    let a = get(&base, "/v1/bench/teams", None).await;
    assert_eq!(a.status, 401);
    assert!(a.no_store);
    assert_eq!(dir.lookups.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_garbage_token_is_401_and_never_reaches_the_directory() {
    let dir = Dir::new(true);
    let (base, _) = serve(dir.clone()).await;
    let a = get(&base, "/v1/bench/teams", Some("not.a.jwt")).await;
    assert_eq!(a.status, 401);
    assert!(!a.text.contains("signature") && !a.text.contains("expired"), "no verifier detail: {}", a.text);
    assert_eq!(dir.lookups.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_revoked_cli_login_is_401() {
    let dir = Dir::new(false);
    let (base, jwt) = serve(dir.clone()).await;
    assert_eq!(get(&base, "/v1/bench/teams", Some(&cli(&jwt, "karthik"))).await.status, 401);
    assert_eq!(dir.lookups.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_member_sees_only_their_own_teams_whatever_the_query_says() {
    let (base, jwt) = serve(Dir::new(true)).await;
    let a = get(&base, "/v1/bench/teams?user=meera&team=other", Some(&cli(&jwt, "karthik"))).await;
    assert_eq!(a.status, 200);
    assert!(a.no_store);
    assert_eq!(
        serde_json::from_str::<Value>(&a.text).unwrap(),
        json!([
            {"slug": "acme", "name": "acme inc", "region": "r1"},
            {"slug": "fresh", "name": "fresh inc", "region": ""},
        ]),
    );
    // A session token authenticates the same person the same way.
    let s = jwt.mint("karthik@example.com", "K", Some("karthik")).unwrap();
    assert_eq!(get(&base, "/v1/bench/teams", Some(&s)).await.text, a.text);
}

#[tokio::test]
async fn a_removed_member_no_longer_sees_the_team() {
    let dir = Dir::new(true);
    let (base, jwt) = serve(dir.clone()).await;
    let tok = cli(&jwt, "karthik");
    assert!(get(&base, "/v1/bench/teams", Some(&tok)).await.text.contains("acme"));
    dir.members.lock().unwrap().retain(|(u, t)| !(*u == "karthik" && *t == "acme"));
    let a = get(&base, "/v1/bench/teams", Some(&tok)).await;
    assert_eq!(a.status, 200);
    assert!(!a.text.contains("acme"), "{}", a.text);
}

#[tokio::test]
async fn an_unreadable_directory_is_503_without_detail_never_an_empty_list() {
    let dir = Dir::new(true);
    let (base, jwt) = serve(dir.clone()).await;
    dir.up.store(false, Ordering::SeqCst);
    let a = get(&base, "/v1/bench/teams", Some(&cli(&jwt, "karthik"))).await;
    assert_eq!(a.status, 503);
    assert!(a.no_store);
    assert!(!a.text.contains("mongo") && !a.text.contains("10.0.0.9"), "{}", a.text);
}
