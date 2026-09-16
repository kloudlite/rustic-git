//! The `/v1` client behind `kl pkg` and `kl env`. The workspace's identity is the
//! `workspace-token` file the keys beat projects beside `registry-token`: read fresh on every
//! call, never cached and never printed, so a re-mint or a revocation takes effect at once.
//!
//! Errors are the api's own sentence when it sent one — those sentences are the product here
//! (`nodejs@99: not in the index; nearest …`) — and `HTTP <status>` only when it did not.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

/// Same mount as `registry-token` (`USER_KEY_PATH` in `crates/workspaces/src/k8s/secrets.rs`).
pub const TOKEN_PATH: &str = "/etc/kloudlite/ssh/workspace-token";

pub struct Api {
    base: String,
    token_path: PathBuf,
    agent: ureq::Agent,
}

impl Api {
    pub fn from_env() -> Result<Self, String> {
        Ok(Self::new(crate::env("KL_API_URL")?, PathBuf::from(TOKEN_PATH)))
    }

    pub fn new(base: impl Into<String>, token_path: PathBuf) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_global(Some(Duration::from_secs(60)))
            // The api's refusal text is the message a person reads; a status-as-error would
            // throw the body away before we could read it.
            .http_status_as_error(false)
            .build()
            .new_agent();
        Self { base: base.into().trim_end_matches('/').to_string(), token_path, agent }
    }

    fn token(&self) -> Result<String, String> {
        std::fs::read_to_string(&self.token_path)
            .map(|t| t.trim().to_string())
            .map_err(|_| format!("no workspace credential at {} — kl runs inside a kloudlite workspace", self.token_path.display()))
    }

    pub fn get(&self, path: &str) -> Result<Value, String> {
        self.send("GET", path, None)
    }

    pub fn post(&self, path: &str) -> Result<Value, String> {
        self.send("POST", path, Some(Value::Object(Default::default())))
    }

    pub fn patch_json(&self, path: &str, body: &Value) -> Result<Value, String> {
        self.send("PATCH", path, Some(body.clone()))
    }

    pub fn put_json(&self, path: &str, body: &Value) -> Result<Value, String> {
        self.send("PUT", path, Some(body.clone()))
    }

    pub fn delete(&self, path: &str) -> Result<Value, String> {
        self.send("DELETE", path, None)
    }

    fn send(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
        let url = format!("{}{}", self.base, path);
        let auth = format!("Bearer {}", self.token()?);
        let mut res = match body {
            None if method == "DELETE" => self.agent.delete(&url).header("authorization", &auth).call(),
            None => self.agent.get(&url).header("authorization", &auth).call(),
            Some(b) => {
                let b = b.to_string();
                let r = match method {
                    "POST" => self.agent.post(&url),
                    "PUT" => self.agent.put(&url),
                    _ => self.agent.patch(&url),
                };
                r.header("authorization", &auth).header("content-type", "application/json").send(b)
            }
        }
        .map_err(|e| format!("{method} {path}: {e}"))?;
        let status = res.status().as_u16();
        let ctype = res.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let text = res.body_mut().read_to_string().map_err(|e| format!("{method} {path}: {e}"))?;
        if !(200..300).contains(&status) {
            return Err(failure(status, &ctype, &text));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| format!("{method} {path}: the api answered something that is not json: {e}"))
    }
}

/// The api's sentence when it sent one, else the bare status. Kept out of `send` so the mapping
/// is testable without a socket.
fn failure(status: u16, ctype: &str, body: &str) -> String {
    let body = body.trim();
    if !body.is_empty() {
        if ctype.starts_with("text/") {
            return body.to_string();
        }
        if let Some(m) = serde_json::from_str::<Value>(body).ok().and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string)) {
            return m;
        }
    }
    format!("HTTP {status}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// One request, one canned answer — enough to hold the client's own behaviour (auth header,
    /// body, status mapping) without a server crate.
    fn serve(answer: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap();
            s.write_all(answer.as_bytes()).unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });
        (format!("http://127.0.0.1:{port}"), h)
    }

    fn api(base: String) -> (Api, tempfile::NamedTempFile) {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"tok123\n").unwrap();
        let a = Api::new(base, f.path().to_path_buf());
        (a, f)
    }

    #[test]
    fn get_carries_the_bearer_and_parses_json() {
        let (base, h) = serve("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"ok\":true}");
        let (a, _f) = api(base);
        assert_eq!(a.get("/v1/x").unwrap(), serde_json::json!({"ok": true}));
        let req = h.join().unwrap();
        assert!(req.to_lowercase().contains("authorization: bearer tok123"), "{req}");
    }

    #[test]
    fn patch_sends_the_body_and_a_204_is_null() {
        let (base, h) = serve("HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n");
        let (a, _f) = api(base);
        assert_eq!(a.patch_json("/v1/x", &serde_json::json!({"packages": ["jq"]})).unwrap(), Value::Null);
        assert!(h.join().unwrap().contains("{\"packages\":[\"jq\"]}"));
    }

    #[test]
    fn a_text_refusal_is_the_message() {
        let (base, h) = serve("HTTP/1.1 422 Unprocessable Entity\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: 27\r\n\r\nnodejs@99: not in the index");
        let (a, _f) = api(base);
        assert_eq!(a.get("/v1/x").unwrap_err(), "nodejs@99: not in the index");
        let _ = h.join();
    }

    #[test]
    fn missing_token_file_never_reaches_the_wire() {
        let a = Api::new("http://127.0.0.1:1", PathBuf::from("/nonexistent/workspace-token"));
        assert!(a.get("/v1/x").unwrap_err().starts_with("no workspace credential at"));
    }

    #[test]
    fn error_mapping() {
        assert_eq!(failure(422, "text/plain; charset=utf-8", "nope\n"), "nope");
        assert_eq!(failure(409, "application/json", "{\"error\":\"busy\"}"), "busy");
        assert_eq!(failure(500, "application/json", "{\"detail\":\"x\"}"), "HTTP 500");
        assert_eq!(failure(502, "", ""), "HTTP 502");
    }
}
