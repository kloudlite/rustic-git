//! The thin api client. Errors are strings because every one of them is printed and exited on.

#[derive(serde::Deserialize)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    pub state: String,
    #[serde(default)]
    pub packages: Vec<String>,
}

/// Serialized too: `kl ws ssh` hands it to the ProxyCommand child through the environment.
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
            Error::Unauthorized => write!(f, "your login has expired — run `kl login`"),
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
pub async fn ssh_session(cfg: &crate::config::Config, target: &str) -> Result<Session, Error> {
    let r = client()
        .post(format!("{}/v1/workspaces/{target}/ssh-session", cfg.api))
        .bearer_auth(&cfg.token)
        .send()
        .await
        .map_err(|e| Error::Other(e.to_string()))?;
    json(r).await
}
