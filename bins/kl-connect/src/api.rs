//! The thin api client. Errors are strings because every one of them is printed and exited on.

#[derive(serde::Deserialize)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    pub state: String,
    #[serde(default)]
    pub packages: Vec<String>,
}

/// Serialized too: `kl-connect ws ssh` hands it to the ProxyCommand child through the environment.
#[derive(serde::Deserialize, serde::Serialize)]
pub struct Session {
    /// The workspace it was minted for — the api resolves a name, so this is how the CLI learns
    /// the id without listing.
    pub id: String,
    pub token: String,
    pub gateway: String,
    pub host_key: String,
}

/// One client for the process: it pools connections, and the timeouts stop `kl` hanging forever
/// behind a black-holed network — every call it makes is a small JSON request.
pub fn client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("building an http client")
    })
}

/// 401 is the one status callers branch on (an expired cli token), so it is its own variant.
pub enum Error {
    Unauthorized,
    Other(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unauthorized => write!(f, "your login has expired — run `kl-connect login`"),
            Error::Other(m) => write!(f, "{m}"),
        }
    }
}

async fn json<T: serde::de::DeserializeOwned>(r: reqwest::Response) -> Result<T, Error> {
    let status = r.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(Error::Unauthorized);
    }
    let body = r.text().await.map_err(|e| Error::Other(e.to_string()))?;
    if !status.is_success() {
        // The api's error bodies are `{"error": "…"}`; anything else is shown as it came.
        let msg = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
            .unwrap_or_else(|| body.trim().to_string());
        return Err(Error::Other(format!("{}: {msg}", status.as_u16())));
    }
    serde_json::from_str(&body).map_err(|e| Error::Other(e.to_string()))
}

pub async fn list(
    cfg: &crate::config::Config,
    team: Option<&str>,
) -> Result<Vec<Workspace>, Error> {
    let mut req = client()
        .get(format!("{}/v1/workspaces", cfg.api))
        .bearer_auth(&cfg.token);
    if let Some(t) = team {
        req = req.query(&[("team", t)]);
    }
    json(req.send().await.map_err(|e| Error::Other(e.to_string()))?).await
}

/// `target` is an id or a name; the api resolves either (see `api::ssh_session` on the server).
///
/// PERCENT-ENCODED into the path: the name comes from a person's command line, and a `/` or a `?`
/// in it changed which endpoint was called rather than being sent as a name (2026-09-12).
pub async fn ssh_session(cfg: &crate::config::Config, target: &str) -> Result<Session, Error> {
    let target = path_segment(target);
    let r = client()
        .post(format!("{}/v1/workspaces/{target}/ssh-session", cfg.api))
        .bearer_auth(&cfg.token)
        .send()
        .await
        .map_err(|e| Error::Other(e.to_string()))?;
    json(r).await
}

/// One path segment, percent-encoded. Everything outside the unreserved set is escaped, which is
/// stricter than a URL needs and exactly what a segment carrying somebody's typed name wants.
fn path_segment(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b => format!("%{b:02X}"),
        })
        .collect()
}

/// One condition off the builder's `Environment` status -- the same shape `GET
/// /v1/internal/builders/{slug}` and `/v1/builders/me` both answer.
#[derive(serde::Deserialize)]
pub struct Condition {
    // Kept for the shape's sake, unused by `kl-connect builder status`'s rendering.
    #[allow(dead_code)]
    #[serde(rename = "type")]
    pub type_: String,
    pub status: String,
    pub message: String,
}

#[derive(serde::Deserialize)]
pub struct BuilderStatus {
    pub state: String,
    pub ready: bool,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

/// `POST /v1/bench/session`'s 201 body.
#[derive(serde::Deserialize)]
pub struct BenchSession {
    #[allow(dead_code)]
    pub id: String,
    pub token: String,
    pub gateway: String,
    #[allow(dead_code)]
    pub expires_at: String,
}

/// `bench_session`'s answer: `Ready` on 201, `Waking(state)` on 202 (the api's `{"state": phase}`
/// while it wakes or starts).
pub enum SessionAnswer {
    Ready(BenchSession),
    Waking(String),
}

#[derive(serde::Deserialize)]
struct BenchState {
    state: String,
}

async fn bench_request(
    req: reqwest::RequestBuilder,
) -> Result<reqwest::Response, Error> {
    req.send().await.map_err(|e| Error::Other(e.to_string()))
}

/// `region` is sent only for a personal bench (its first use binds the person's region); a
/// team's is the team's.
pub async fn create_bench(
    cfg: &crate::config::Config,
    team: Option<&str>,
    region: Option<&str>,
) -> Result<serde_json::Value, Error> {
    let body = serde_json::json!({"team": team, "region": region});
    let r = bench_request(
        client()
            .post(format!("{}/v1/bench", cfg.api))
            .bearer_auth(&cfg.token)
            .json(&body),
    )
    .await?;
    json(r).await
}

#[allow(dead_code)] // part of the api surface the brief specifies; no `kl-connect bench status` yet
pub async fn get_bench(
    cfg: &crate::config::Config,
    team: Option<&str>,
) -> Result<serde_json::Value, Error> {
    let mut req = client().get(format!("{}/v1/bench", cfg.api)).bearer_auth(&cfg.token);
    if let Some(t) = team {
        req = req.query(&[("team", t)]);
    }
    json(bench_request(req).await?).await
}

/// 201 -> `Ready(session)`; 202 -> `Waking(state)`; anything else an `Error` carrying the body's
/// `error`.
pub async fn bench_session(cfg: &crate::config::Config, team: Option<&str>) -> Result<SessionAnswer, Error> {
    let mut req = client()
        .post(format!("{}/v1/bench/session", cfg.api))
        .bearer_auth(&cfg.token);
    if let Some(t) = team {
        req = req.query(&[("team", t)]);
    }
    let r = bench_request(req).await?;
    if r.status() == reqwest::StatusCode::ACCEPTED {
        let body = r.text().await.map_err(|e| Error::Other(e.to_string()))?;
        let st: BenchState = serde_json::from_str(&body).map_err(|e| Error::Other(e.to_string()))?;
        return Ok(SessionAnswer::Waking(st.state));
    }
    Ok(SessionAnswer::Ready(json(r).await?))
}

/// `GET /v1/builders/me` -- the caller's own hidden builder, or their team's with `team`.
pub async fn builder_status(
    cfg: &crate::config::Config,
    team: Option<&str>,
) -> Result<BuilderStatus, Error> {
    let mut req = client()
        .get(format!("{}/v1/builders/me", cfg.api))
        .bearer_auth(&cfg.token);
    if let Some(t) = team {
        req = req.query(&[("team", t)]);
    }
    json(req.send().await.map_err(|e| Error::Other(e.to_string()))?).await
}
