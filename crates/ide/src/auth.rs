//! The credential every tool call carries, and the one reason this crate has auth code at all.
//!
//! The namespace was supposed to be the whole fence (`allow-bench-tools`, spec §2.5). It is not:
//! the SHELL sidecar shares the pod's network namespace, so from a person's terminal
//! `curl 127.0.0.1:7788/tools/exec` reached the tool server on LOOPBACK, where no NetworkPolicy
//! applies, and ran a command as the workspace user (2026-09-18). A NetworkPolicy cannot express
//! "not from the container beside you"; a credential can.
//!
//! The credential is the `workspace-token` the keys beat already projects into the WORKSPACE
//! container (`k8s::secrets::user_key_secret`, mounted at `USER_KEY_PATH`, the same file `kl`
//! reads). Nothing new is minted and nothing new is mounted — the fence is that the shell
//! container mounts no Secret at all, so it has no token to send and gets a 401.
//!
//! Read from disk PER REQUEST, never cached: the beat re-mints it every `KEYS_RESYNC_SECS`, and a
//! value cached at boot would start refusing the very callers it is meant to admit an hour in.
//! It is a file read of a few hundred bytes from the kubelet's tmpfs, beside a call that is about
//! to fork a process.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Where the keys beat projects the workspace's own platform token. The same path `kl` reads
/// (`bins/kl/src/api.rs`'s `TOKEN_PATH`) — one file, one credential, one rotation.
pub const TOKEN_PATH: &str = "/etc/kloudlite/ssh/workspace-token";

/// The paths served without a credential. Only liveness: a probe has no Secret to read and its
/// answer says nothing about the workspace beyond "this process is up".
const OPEN: [&str; 1] = ["/healthz"];

/// Whether a request is one that may be served unauthenticated.
pub fn is_open(path: &str) -> bool {
    OPEN.contains(&path)
}

/// The token this server admits, read fresh. `None` means the file is not there — which is the
/// state a workspace pod is in before the keys beat has run, and the state a non-default image is
/// in permanently.
fn expected(path: &Path) -> Option<String> {
    let t = std::fs::read_to_string(path).ok()?;
    let t = t.trim().to_string();
    (!t.is_empty()).then_some(t)
}

/// The bearer token a request carries, if any.
fn presented(req: &Request) -> Option<String> {
    let v = req.headers().get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let t = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer "))?.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Constant time in the length that matters: a token is compared byte by byte with no early
/// return, so a caller cannot learn a prefix from how long the refusal took. `ct_eq` by hand
/// rather than a dependency for six lines.
fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    // Lengths differ: still walk, so the answer costs the same whatever was sent.
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

/// Where the token file lives for this server. Its own field so a test can point it at a tempdir,
/// and so a future non-default image could be handed another path without touching this logic.
#[derive(Clone, Debug)]
pub struct Tokens {
    pub path: PathBuf,
}

impl Default for Tokens {
    fn default() -> Self {
        Tokens { path: PathBuf::from(TOKEN_PATH) }
    }
}

impl Tokens {
    /// Whether this request may proceed.
    ///
    /// Fails CLOSED when the token file is missing: a server that cannot read its own credential
    /// admits nobody rather than everybody. That is the opposite of the reflex — a missing config
    /// usually means "unconfigured, allow" — and it is deliberate, because "unconfigured" here is
    /// exactly the state an attacker would arrange if they could.
    pub fn admits(&self, req: &Request) -> bool {
        match (expected(&self.path), presented(req)) {
            (Some(want), Some(got)) => same(&want, &got),
            _ => false,
        }
    }
}

/// The layer every route but `/healthz` sits behind.
///
/// A 401 with `WWW-Authenticate`, and a body that says what to send WITHOUT naming the file: the
/// caller that should have a token has one, and the caller that should not is being told nothing
/// it can act on. The shell sidecar reaching this is working as designed, not a bug to debug.
pub async fn require(State(tokens): State<Arc<Tokens>>, req: Request, next: Next) -> Response {
    if is_open(req.uri().path()) || tokens.admits(&req) {
        return next.run(req).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
        axum::Json(serde_json::json!({ "error": "the tool server needs the workspace's token" })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(auth: Option<&str>) -> Request {
        let mut b = Request::builder().uri("/tools/exec");
        if let Some(a) = auth {
            b = b.header(axum::http::header::AUTHORIZATION, a);
        }
        b.body(axum::body::Body::empty()).unwrap()
    }

    fn with_token(tok: &str) -> (tempfile::TempDir, Tokens) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("workspace-token");
        std::fs::write(&path, tok).unwrap();
        (tmp, Tokens { path })
    }

    #[test]
    fn only_the_workspaces_own_token_is_admitted() {
        let (_t, tokens) = with_token("s3cret\n");
        assert!(tokens.admits(&req(Some("Bearer s3cret"))), "the trailing newline of the file is trimmed");
        assert!(tokens.admits(&req(Some("bearer s3cret"))), "the scheme is case-insensitive");
        assert!(!tokens.admits(&req(Some("Bearer s3cre"))), "a prefix is not the token");
        assert!(!tokens.admits(&req(Some("Bearer s3crets"))), "nor is an extension of it");
        assert!(!tokens.admits(&req(Some("s3cret"))), "the scheme is required");
        assert!(!tokens.admits(&req(Some("Bearer "))), "an empty token is no token");
        assert!(!tokens.admits(&req(None)), "this is what the shell sidecar sends");
    }

    /// The shell container mounts no Secret, so it has no token FILE either — and a server that
    /// cannot read its own credential must admit nobody. The reflex is the other way ("missing
    /// config, allow"), which here would restore exactly the hole this closes.
    #[test]
    fn a_missing_token_file_admits_nobody() {
        let tmp = tempfile::tempdir().unwrap();
        let tokens = Tokens { path: tmp.path().join("nope") };
        assert!(!tokens.admits(&req(Some("Bearer anything"))));
        assert!(!tokens.admits(&req(None)));
        // And an empty file is a missing one: a Secret projected before the beat has written it.
        let (_t2, empty) = with_token("   \n");
        assert!(!empty.admits(&req(Some("Bearer "))));
    }

    #[test]
    fn only_healthz_is_open() {
        assert!(is_open("/healthz"));
        for p in ["/tools", "/tools/exec", "/fs/tree", "/fs/file", "/stream/process/p-1", "/"] {
            assert!(!is_open(p), "{p} must need a credential");
        }
    }

    #[test]
    fn the_comparison_does_not_shortcut_on_length() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "ab"));
        assert!(!same("ab", "abc"));
        assert!(same("", ""));
    }
}
