//! The workspace-state routes a UI renders from: `/fs/tree`, `/fs/stat`, `/fs/file`, `/fs/git`,
//! `/fs/changes`, `/fs/diff`. Separate from the tool API on purpose — an agent drives `/tools/*`,
//! a console DRAWS from these — so nothing here enters the tool registry and `GET /tools` never
//! lists them. Read-only; a write is a tool call and a git write is the session layer's decision.
//!
//! Serving rules: every answer carries an `ETag` and `If-None-Match` is a 304 with no body, so the
//! re-fetch storm a watch stream provokes costs a hash, not a body; one git process per request;
//! bytes are streamed only by `/fs/file`, everything else is JSON. Errors map exactly as the tool
//! API's do (`tools::status_of`).
pub mod git;
pub mod tree;

use crate::paths::confine;
use crate::server::App;
use crate::tools::{status_of, ToolError};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

pub const MAX_FILE: u64 = 10 << 20;
pub const MAX_DIFF: usize = 200_000;

fn etag_of(bytes: &[u8]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    format!("\"{:016x}\"", h.finish())
}

fn matches(headers: &HeaderMap, etag: &str) -> bool {
    headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).map(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*")).unwrap_or(false)
}

/// A JSON body under its own ETag, or 304 when the caller already holds it.
fn conditional_json(headers: &HeaderMap, v: &Value) -> Response {
    let body = v.to_string();
    let etag = etag_of(body.as_bytes());
    if matches(headers, &etag) {
        return with_etag(StatusCode::NOT_MODIFIED.into_response(), &etag);
    }
    let mut r = (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], body).into_response();
    r = with_etag(r, &etag);
    r
}

fn with_etag(mut r: Response, etag: &str) -> Response {
    if let Ok(v) = HeaderValue::from_str(etag) {
        r.headers_mut().insert(header::ETAG, v);
    }
    r
}

fn err(e: ToolError) -> Response {
    (status_of(&e), Json(json!({ "error": e.to_string() }))).into_response()
}

fn failed(msg: String) -> Response {
    err(ToolError::Failed(msg))
}

/// Text when the first 8 KiB carry no NUL; a few well-known extensions get their real type, the
/// rest is `application/octet-stream` and the UI decides.
pub fn sniff(name: &str, head: &[u8]) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => return "image/png",
        "jpg" | "jpeg" => return "image/jpeg",
        "gif" => return "image/gif",
        "svg" => return "image/svg+xml",
        "webp" => return "image/webp",
        "pdf" => return "application/pdf",
        "json" => return "application/json; charset=utf-8",
        "html" | "htm" => return "text/html; charset=utf-8",
        "css" => return "text/css; charset=utf-8",
        "js" | "mjs" => return "text/javascript; charset=utf-8",
        "md" => return "text/markdown; charset=utf-8",
        _ => {}
    }
    if head.iter().take(8192).any(|b| *b == 0) {
        "application/octet-stream"
    } else {
        "text/plain; charset=utf-8"
    }
}

#[derive(Deserialize)]
pub struct TreeQuery {
    #[serde(default = "dot")]
    path: String,
    #[serde(default = "one")]
    depth: u8,
}
fn dot() -> String {
    ".".into()
}
fn one() -> u8 {
    1
}

pub async fn tree(State(app): State<Arc<App>>, headers: HeaderMap, Query(q): Query<TreeQuery>) -> Response {
    match tree::tree(&app.cfg.root, &app.cfg.home, &q.path, q.depth).await {
        Ok((dir, entries, truncated)) => conditional_json(&headers, &json!({ "path": dir, "truncated": truncated, "entries": entries })),
        Err(e) => err(e),
    }
}

#[derive(Deserialize)]
pub struct PathQuery {
    path: String,
    at: Option<String>,
}

pub async fn stat(State(app): State<Arc<App>>, headers: HeaderMap, Query(q): Query<PathQuery>) -> Response {
    match tree::stat(&app.cfg.root, &app.cfg.home, &q.path).await {
        Ok(Some(e)) => {
            let mut v = serde_json::to_value(&e).unwrap_or_default();
            if e.kind == "file" {
                let p = confine(&app.cfg.root, &app.cfg.home, &q.path).unwrap_or_default();
                let head = std::fs::read(&p).map(|b| b.into_iter().take(8192).collect::<Vec<u8>>()).unwrap_or_default();
                v["mime"] = json!(sniff(&e.name, &head));
            }
            conditional_json(&headers, &v)
        }
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({ "error": format!("{}: not found", q.path) }))).into_response(),
        Err(e) => err(e),
    }
}

/// The bytes of a file: the worktree copy, or `at=` a ref (`index` for the staged copy). The
/// worktree ETag is `"{len}-{mtime_ns}"`, decided from metadata alone, so a 304 never reads the file.
pub async fn file(State(app): State<Arc<App>>, headers: HeaderMap, Query(q): Query<PathQuery>) -> Response {
    let p = match confine(&app.cfg.root, &app.cfg.home, &q.path) {
        Ok(p) => p,
        Err(e) => return err(e),
    };
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if let Some(at) = q.at.as_deref() {
        let Ok(rel) = p.strip_prefix(&app.cfg.root) else { return err(ToolError::Invalid("at: only paths inside the workspace directory have a history".into())) };
        return match git::show(&app.cfg.root, at, &rel.to_string_lossy()).await {
            Ok(Some(bytes)) => {
                if bytes.len() as u64 > MAX_FILE {
                    return (StatusCode::PAYLOAD_TOO_LARGE, Json(json!({ "error": format!("{}: over {} bytes", q.path, MAX_FILE) }))).into_response();
                }
                let etag = etag_of(&bytes);
                if matches(&headers, &etag) {
                    return with_etag(StatusCode::NOT_MODIFIED.into_response(), &etag);
                }
                let ct = sniff(&name, &bytes);
                with_etag((StatusCode::OK, [(header::CONTENT_TYPE, ct)], bytes).into_response(), &etag)
            }
            Ok(None) => (StatusCode::NOT_FOUND, Json(json!({ "error": format!("{}: not at {at}", q.path) }))).into_response(),
            Err(e) => failed(e),
        };
    }
    let meta = match tokio::fs::metadata(&p).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (StatusCode::NOT_FOUND, Json(json!({ "error": format!("{}: not found", q.path) }))).into_response(),
        Err(e) => return failed(format!("{}: {e}", p.display())),
    };
    if meta.is_dir() {
        return err(ToolError::Invalid(format!("{}: a directory; use /fs/tree", q.path)));
    }
    if meta.len() > MAX_FILE {
        return (StatusCode::PAYLOAD_TOO_LARGE, Json(json!({ "error": format!("{}: {} bytes, over {}", q.path, meta.len(), MAX_FILE) }))).into_response();
    }
    let mtime_ns = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
    let etag = format!("\"{}-{}\"", meta.len(), mtime_ns);
    if matches(&headers, &etag) {
        return with_etag(StatusCode::NOT_MODIFIED.into_response(), &etag);
    }
    match tokio::fs::read(&p).await {
        Ok(bytes) => {
            let ct = sniff(&name, &bytes);
            with_etag((StatusCode::OK, [(header::CONTENT_TYPE, ct)], bytes).into_response(), &etag)
        }
        Err(e) => failed(format!("{}: {e}", p.display())),
    }
}

pub async fn git_state(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let st = match git::status(&app.cfg.root, false).await {
        Ok(s) => s,
        Err(e) => return failed(e),
    };
    if !st.repo {
        return conditional_json(&headers, &json!({ "repo": false }));
    }
    let stashes = git::stash_count(&app.cfg.root).await;
    conditional_json(&headers, &json!({
        "repo": true, "branch": st.branch, "head": st.head, "upstream": st.upstream,
        "ahead": st.ahead, "behind": st.behind, "dirty": !st.changes.is_empty(), "stashes": stashes,
    }))
}

/// Status and line counts in one answer — the changes panel needs both and asks once.
pub async fn changes(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let st = match git::status(&app.cfg.root, false).await {
        Ok(s) => s,
        Err(e) => return failed(e),
    };
    if !st.repo {
        return conditional_json(&headers, &json!({ "repo": false, "changes": [] }));
    }
    let counts = match git::numstat(&app.cfg.root).await {
        Ok(c) => c,
        Err(e) => return failed(e),
    };
    let mut rows = Vec::with_capacity(st.changes.len());
    for c in &st.changes {
        let (additions, deletions, binary) = if c.worktree == '?' {
            // Untracked: git has no count, so the file itself is counted (bounded by MAX_FILE).
            match std::fs::metadata(app.cfg.root.join(&c.path)).ok().filter(|m| m.len() <= MAX_FILE).and_then(|_| std::fs::read(app.cfg.root.join(&c.path)).ok()) {
                Some(b) if b.iter().take(8192).any(|x| *x == 0) => (0, 0, true),
                Some(b) => (b.iter().filter(|x| **x == b'\n').count() as u32, 0, false),
                None => (0, 0, false),
            }
        } else {
            match counts.iter().find(|(p, _)| *p == c.path) {
                Some((_, Some((a, d)))) => (*a, *d, false),
                Some((_, None)) => (0, 0, true),
                None => (0, 0, false),
            }
        };
        rows.push(json!({
            "path": c.path, "index": c.index, "worktree": c.worktree, "renamed_from": c.renamed_from,
            "additions": additions, "deletions": deletions, "binary": binary,
        }));
    }
    conditional_json(&headers, &json!({ "repo": true, "changes": rows }))
}

#[derive(Deserialize)]
pub struct DiffQuery {
    path: Option<String>,
    against: Option<String>,
}

pub async fn diff(State(app): State<Arc<App>>, headers: HeaderMap, Query(q): Query<DiffQuery>) -> Response {
    let Some(against) = git::Against::parse(q.against.as_deref()) else { return err(ToolError::Invalid("against: HEAD, index or staged".into())) };
    let rel = match &q.path {
        Some(p) => match confine(&app.cfg.root, &app.cfg.home, p) {
            Ok(full) => match full.strip_prefix(&app.cfg.root) {
                Ok(r) => Some(r.to_string_lossy().into_owned()),
                Err(_) => return err(ToolError::Invalid("path: only the workspace directory has a diff".into())),
            },
            Err(e) => return err(e),
        },
        None => None,
    };
    // Untracked is decided from status so a brand-new file diffs against /dev/null.
    let untracked = match &rel {
        Some(r) => match git::status(&app.cfg.root, false).await {
            Ok(st) => st.changes.iter().any(|c| c.path == *r && c.worktree == '?'),
            Err(e) => return failed(e),
        },
        None => false,
    };
    match git::diff(&app.cfg.root, rel.as_deref(), against, untracked).await {
        Ok((mut patch, binary)) => {
            let truncated = patch.len() > MAX_DIFF;
            if truncated {
                let mut cut = MAX_DIFF;
                while !patch.is_char_boundary(cut) {
                    cut -= 1;
                }
                patch.truncate(cut);
            }
            let against = match against { git::Against::Head => "HEAD", git::Against::Index => "index", git::Against::Staged => "staged" };
            conditional_json(&headers, &json!({ "path": rel, "against": against, "binary": binary, "truncated": truncated, "patch": patch }))
        }
        Err(e) => failed(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_knows_text_binary_and_a_few_extensions() {
        assert_eq!(sniff("a.rs", b"fn main() {}"), "text/plain; charset=utf-8");
        assert_eq!(sniff("a.bin", b"\x00\x01"), "application/octet-stream");
        assert_eq!(sniff("logo.png", b"\x89PNG"), "image/png");
        assert_eq!(sniff("x.json", b"{}"), "application/json; charset=utf-8");
    }

    #[test]
    fn if_none_match_accepts_the_tag_a_list_or_a_star() {
        let mut h = HeaderMap::new();
        assert!(!matches(&h, "\"a\""));
        h.insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"b\", \"a\""));
        assert!(matches(&h, "\"a\""));
        h.insert(header::IF_NONE_MATCH, HeaderValue::from_static("*"));
        assert!(matches(&h, "\"zzz\""));
    }
}
