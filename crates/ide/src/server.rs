//! The HTTP surface: `/healthz`, `/tools`, `/fs`, and the two streams. Loopback only — see the crate doc.
use crate::graft::Graft;
use crate::procs::Procs;
use crate::tools::{exec::Exec, files::Files, graft::GraftTools, watch::WatchTools, Registry};
use crate::watches::Watches;
use crate::Config;
use axum::{extract::{DefaultBodyLimit, State}, routing::{get, post}, Json, Router};
use std::sync::Arc;

pub struct App {
    pub cfg: Config,
    pub registry: Registry,
    pub procs: Arc<Procs>,
    pub watches: Arc<Watches>,
    pub graft: Arc<Graft>,
}

impl App {
    /// Wires everything; `Graft::start` is the caller's (`serve`) so a test App spawns nothing.
    pub fn new(cfg: Config) -> Self {
        let procs = Arc::new(Procs::default());
        let watches = Arc::new(Watches::default());
        let graft = Graft::new(cfg.root.clone(), cfg.graft_dir.clone());
        let after: Arc<dyn Fn() + Send + Sync> = {
            let g = graft.clone();
            Arc::new(move || g.refresh_soon())
        };
        let registry = Registry::new(vec![
            Box::new(Files { root: cfg.root.clone(), home: cfg.home.clone(), after_change: Some(after.clone()) }),
            Box::new(Exec { root: cfg.root.clone(), home: cfg.home.clone(), procs: procs.clone(), after_change: Some(after) }),
            Box::new(WatchTools { root: cfg.root.clone(), home: cfg.home.clone(), procs: procs.clone(), watches: watches.clone() }),
            Box::new(GraftTools { graft: graft.clone(), procs: procs.clone() }),
        ]);
        App { cfg, registry, procs, watches, graft }
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/tools", get(crate::api::list))
        .route("/tools/{name}", post(crate::api::call))
        .route("/fs/tree", get(crate::fs::tree))
        .route("/fs/stat", get(crate::fs::stat))
        .route("/fs/file", get(crate::fs::file))
        .route("/fs/git", get(crate::fs::git_state))
        .route("/fs/changes", get(crate::fs::changes))
        .route("/fs/diff", get(crate::fs::diff))
        .route("/stream/process/{id}", get(crate::stream::process))
        .route("/stream/watch/{id}", get(crate::stream::watch))
        // Axum's own default is 2 MiB, which refused a `write` or `patch` body the file tools
        // themselves accept up to `MAX_BYTES` (2026-09-12).
        .layer(DefaultBodyLimit::max(crate::tools::files::MAX_BYTES as usize + (1 << 20)))
        .with_state(app)
}

async fn healthz(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true, "root": app.cfg.root, "graph": app.graft.state() }))
}

pub async fn serve(cfg: Config) -> anyhow::Result<()> {
    let bind = cfg.bind;
    let app = Arc::new(App::new(cfg));
    app.graft.start();
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "ide.listening");
    axum::serve(listener, router(app)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    #[tokio::test]
    async fn healthz_names_the_root() {
        let cfg = Config { bind: "127.0.0.1:0".parse().unwrap(), root: "/home/kl/workspaces/api".into(), home: "/home/kl".into(), graft_dir: None };
        let r = router(Arc::new(App::new(cfg))).oneshot(axum::http::Request::get("/healthz").body(axum::body::Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), 200);
        let b = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["root"], "/home/kl/workspaces/api");
    }

    async fn post(app: &Arc<App>, name: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
        let r = router(app.clone()).oneshot(axum::http::Request::post(format!("/tools/{name}")).header("content-type", "application/json").body(axum::body::Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = r.status().as_u16();
        (status, serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap())
    }

    /// The whole surface a session layer uses, against a tempdir root: list, call, and every
    /// error as an HTTP status rather than a field inside a tool's own answer.
    #[tokio::test]
    async fn the_tool_api_lists_calls_and_answers_errors_as_statuses() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let root = home.join("workspaces/api");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        let app = Arc::new(App::new(Config { bind: "127.0.0.1:0".parse().unwrap(), root, home, graft_dir: None }));
        let r = router(app.clone()).oneshot(axum::http::Request::get("/tools").body(axum::body::Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap();
        let names: Vec<&str> = v["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["read", "write", "edit", "patch", "glob", "grep", "exec", "process_list", "process_output", "process_write", "process_kill", "watch", "watch_poll", "watch_stop", "graft_find_code", "graft_find_all", "graft_trace_calls", "graft_file_api", "graft_repo_map", "graft_build", "graft_blast"]);
        assert_eq!(v["tools"][0]["schema"]["type"], "object");
        let (s, v) = post(&app, "read", serde_json::json!({"path": "a.txt"})).await;
        assert_eq!(s, 200);
        assert!(v["content"].as_str().unwrap().contains("two"));
        let (s, v) = post(&app, "read", serde_json::json!({"path": "/etc/passwd"})).await;
        assert_eq!(s, 403, "{v}");
        assert!(v["error"].as_str().unwrap().contains("outside"), "{v}");
        let (s, _) = post(&app, "read", serde_json::json!({})).await;
        assert_eq!(s, 400);
        let (s, _) = post(&app, "nope", serde_json::json!({})).await;
        assert_eq!(s, 404);
        // Bigger than axum's 2 MiB default: the limit is the file tools' own.
        let (s, v) = post(&app, "write", serde_json::json!({"path": "big.txt", "content": "x".repeat(3 << 20)})).await;
        assert_eq!((s, v["bytes"].as_u64()), (200, Some(3 << 20)), "{v}");
    }

    async fn get(app: &Arc<App>, uri: &str, inm: Option<&str>) -> (u16, Option<String>, Vec<u8>) {
        let mut req = axum::http::Request::get(uri);
        if let Some(t) = inm {
            req = req.header("if-none-match", t);
        }
        let r = router(app.clone()).oneshot(req.body(axum::body::Body::empty()).unwrap()).await.unwrap();
        let status = r.status().as_u16();
        let etag = r.headers().get("etag").and_then(|v| v.to_str().ok()).map(str::to_string);
        (status, etag, axum::body::to_bytes(r.into_body(), 1 << 24).await.unwrap().to_vec())
    }

    /// The six `/fs` routes against a real repository: shapes, the conditional 304, every status.
    #[tokio::test]
    async fn the_fs_routes_render_a_repository_and_answer_304_on_a_repeat() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let root = home.join("workspaces/api");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let sh = |args: &[&str]| {
            let o = std::process::Command::new("git").args(args).current_dir(&root).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").output().unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        };
        sh(&["init", "-q", "-b", "main"]);
        std::fs::write(root.join("README.md"), "hi\n").unwrap();
        sh(&["add", "-A"]);
        sh(&["commit", "-q", "-m", "one"]);
        std::fs::write(root.join("README.md"), "hi\nmore\n").unwrap();
        std::fs::write(root.join("src/new.rs"), "fn a() {}\n").unwrap();
        let app = Arc::new(App::new(Config { bind: "127.0.0.1:0".parse().unwrap(), root: root.clone(), home: home.clone(), graft_dir: None }));

        let (s, etag, b) = get(&app, "/fs/tree?depth=2", None).await;
        assert_eq!(s, 200);
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        let names: Vec<&str> = v["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec![".git", "src", "README.md"]);
        assert_eq!(v["entries"][1]["git"], "?");
        assert_eq!(v["entries"][1]["entries"][0]["name"], "new.rs");
        assert_eq!(v["entries"][2]["git"], "M");
        let (s, _, b) = get(&app, "/fs/tree?depth=2", etag.as_deref()).await;
        assert_eq!((s, b.len()), (304, 0));
        assert_eq!(get(&app, "/fs/tree?depth=9", None).await.0, 400);
        assert_eq!(get(&app, "/fs/tree?path=/etc", None).await.0, 403);

        let (s, _, b) = get(&app, "/fs/stat?path=README.md", None).await;
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!((s, v["mime"].as_str().unwrap(), v["git"].as_str().unwrap()), (200, "text/markdown; charset=utf-8", "M"));
        assert_eq!(get(&app, "/fs/stat?path=nope", None).await.0, 404);

        let (s, etag, b) = get(&app, "/fs/file?path=README.md", None).await;
        assert_eq!((s, b.as_slice()), (200, b"hi\nmore\n".as_slice()));
        assert_eq!(get(&app, "/fs/file?path=README.md", etag.as_deref()).await.0, 304);
        let (s, _, b) = get(&app, "/fs/file?path=README.md&at=HEAD", None).await;
        assert_eq!((s, b.as_slice()), (200, b"hi\n".as_slice()));
        assert_eq!(get(&app, "/fs/file?path=src/new.rs&at=HEAD", None).await.0, 404);
        assert_eq!(get(&app, "/fs/file?path=src", None).await.0, 400);

        let (s, _, b) = get(&app, "/fs/git", None).await;
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!((s, v["repo"].as_bool(), v["branch"].as_str(), v["dirty"].as_bool()), (200, Some(true), Some("main"), Some(true)));

        let (s, _, b) = get(&app, "/fs/changes", None).await;
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(s, 200);
        let rows = v["changes"].as_array().unwrap();
        assert_eq!(rows.iter().map(|r| (r["path"].as_str().unwrap(), r["additions"].as_u64().unwrap())).collect::<Vec<_>>(), vec![("README.md", 1), ("src/new.rs", 1)]);

        let (s, _, b) = get(&app, "/fs/diff?path=README.md", None).await;
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(s, 200);
        assert!(v["patch"].as_str().unwrap().contains("+more"), "{v}");
        let (_, _, b) = get(&app, "/fs/diff?path=src/new.rs", None).await;
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert!(v["patch"].as_str().unwrap().contains("+fn a()"), "untracked diffs against /dev/null: {v}");
        assert_eq!(get(&app, "/fs/diff?against=main", None).await.0, 400);

        // Not a repository is an answer, not an error.
        let plain = Arc::new(App::new(Config { bind: "127.0.0.1:0".parse().unwrap(), root: home.join("workspaces"), home: home.clone(), graft_dir: None }));
        let (s, _, b) = get(&plain, "/fs/git", None).await;
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!((s, v["repo"].as_bool()), (200, Some(false)));
    }
}
