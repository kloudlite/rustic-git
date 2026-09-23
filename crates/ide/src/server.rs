//! The HTTP surface: `/healthz`, `/tools`, `/fs`, and the two streams. Loopback only — see the crate doc.
use crate::procs::Procs;
use crate::trees::{TreeCtx, Trees};
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
    /// Every tree this workspace serves, built lazily. There is no separate `graft` handle on the
    /// App any more: a graph belongs to a tree, and `main`'s is the one on `main`'s ctx.
    pub trees: Arc<Trees>,
}

impl App {
    /// Wires everything; `Graft::start` is the caller's (`serve`) so a test App spawns nothing.
    pub fn new(cfg: Config) -> Self {
        let procs = Arc::new(Procs::default());
        let watches = Arc::new(Watches::default());
        let trees = Arc::new(Trees::under(cfg.home.clone(), cfg.root.clone(), cfg.graft_dir.clone()));
        let registry = Registry::new(vec![
            Box::new(Files { trees: trees.clone() }),
            Box::new(Exec { trees: trees.clone(), procs: procs.clone() }),
            Box::new(WatchTools { trees: trees.clone(), procs: procs.clone(), watches: watches.clone() }),
            Box::new(GraftTools { trees: trees.clone(), procs: procs.clone() }),
        ]);
        App { cfg, registry, procs, watches, trees }
    }

    /// The tree a request names, or the main one. The accessor every route goes through, so a
    /// route added later cannot quietly read the workspace when it was asked for a tree.
    pub fn tree(&self, name: Option<&str>) -> Result<Arc<TreeCtx>, crate::tools::ToolError> {
        self.trees.resolve(name)
    }

    /// `main`'s graph. Named because the server's own beat and `/healthz` are about the
    /// workspace, not about whichever tree happened to be asked for last.
    pub fn graft(&self) -> Arc<crate::graft::Graft> {
        self.trees.resolve(None).map(|t| t.graft.clone()).expect("the main tree always resolves")
    }
}

pub fn router(app: Arc<App>) -> Router {
    // Read from the config so a test can point it at a tempdir; the default is the file the keys
    // beat projects into the workspace container and the shell sidecar does not mount.
    let tokens = Arc::new(crate::auth::Tokens {
        path: app.cfg.token_path.clone().unwrap_or_else(|| std::path::PathBuf::from(crate::auth::TOKEN_PATH)),
    });
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
        .route("/fs/log", get(crate::fs::log))
        .route("/stream/process/{id}", get(crate::stream::process))
        .route("/stream/watch/{id}", get(crate::stream::watch))
        // NO PTY route: a terminal is the SHELL SIDECAR's `ttyd` on 7790 now (spec §2.3), in its
        // own container with the home and nothing else. The tool server used to carry one, with
        // named sessions and replay; a dropped connection is a new shell instead, and the tool
        // server is only tools again.
        // Axum's own default is 2 MiB, which refused a `write` or `patch` body the file tools
        // themselves accept up to `MAX_BYTES` (2026-09-12).
        // One server span per request, named by the route TEMPLATE (`/tools/{name}`, `/fs/file`),
        // never a path or query; `/healthz` is untraced, and a stream is one span that ends at the
        // upgrade, never one per message. Untrusted like a public door: a caller's sampled flag
        // counts only for probe traffic, within the probe bucket (`kloudlite_trace::sampler`).
        // ABOVE the trace layer, so a refused request is still one line in the log — a 401 from
        // the shell sidecar is the fence working, and the fleet should be able to count them.
        // Below nothing else: every route but `/healthz` needs the credential, including the two
        // WebSocket upgrades, which carry headers like any other request.
        .layer(axum::middleware::from_fn_with_state(tokens, crate::auth::require))
        .layer(axum::middleware::from_fn(kloudlite_trace::traced))
        .layer(DefaultBodyLimit::max(crate::tools::files::MAX_BYTES as usize + (1 << 20)))
        .with_state(app)
}

async fn healthz(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    // One `graph` per tree, keyed by name: `main`'s is what the workspace's own session reads,
    // and a subagent's is its own (spec §4.4).
    let graphs: serde_json::Map<String, serde_json::Value> = app
        .trees
        .served()
        .into_iter()
        .map(|t| (t.name.clone(), serde_json::json!(t.graft.state())))
        .collect();
    let main = app.graft().state();
    Json(serde_json::json!({ "ok": true, "root": app.cfg.root, "graph": main, "graphs": graphs }))
}

pub async fn serve(cfg: Config) -> anyhow::Result<()> {
    let bind = cfg.bind;
    let app = Arc::new(App::new(cfg));
    app.graft().start();
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "ide.listening");
    axum::serve(listener, router(app)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    const PARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    /// The workspace token these tests present. Written into each fixture's own tempdir, so the
    /// router reads a real file the way it does in a pod.
    const TOKEN: &str = "test-workspace-token";

    /// A token file beside the fixture, and the path to hand `Config`.
    fn token_in(dir: &std::path::Path) -> Option<std::path::PathBuf> {
        let p = dir.join("workspace-token");
        std::fs::write(&p, TOKEN).unwrap();
        Some(p)
    }

    fn traced_app() -> (tempfile::TempDir, Arc<App>) {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let root = home.join("workspaces/api");
        std::fs::create_dir_all(root.join("secret-dir")).unwrap();
        std::fs::write(root.join("secret-dir/a.txt"), "SECRET-CONTENT\n").unwrap();
        (tmp, Arc::new(App::new(Config { bind: "127.0.0.1:0".parse().unwrap(), token_path: token_in(&home), root, home, graft_dir: None })))
    }

    /// Probe-marked so the flag is obeyed: the tool server trusts nobody else's.
    async fn send(app: &Arc<App>, req: axum::http::request::Builder, body: axum::body::Body) -> u16 {
        let req = req.header("traceparent", PARENT).header(kloudlite_trace::PROBE_HEADER, "1").header("authorization", format!("Bearer {TOKEN}")).body(body).unwrap();
        router(app.clone()).oneshot(req).await.unwrap().status().as_u16()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_tool_call_continues_the_bench_trace_and_carries_only_the_tool_name() {
        let (dispatch, spans) = kloudlite_trace::testing::subscriber();
        let _g = tracing::dispatcher::set_default(&dispatch);
        let (_tmp, app) = traced_app();
        let body = serde_json::json!({ "path": "secret-dir/a.txt" }).to_string();
        assert_eq!(send(&app, axum::http::Request::post("/tools/read").header("content-type", "application/json"), body.into()).await, 200);
        assert_eq!(send(&app, axum::http::Request::get("/fs/file?path=secret-dir/a.txt"), axum::body::Body::empty()).await, 200);
        let got = spans.get_finished_spans().unwrap();
        let names: Vec<_> = got.iter().map(|s| s.name.to_string()).collect();
        assert_eq!(names, vec!["POST /tools/{name}", "GET /fs/file"]);
        assert!(got.iter().all(|s| s.span_context.trace_id().to_string() == "4bf92f3577b34da6a3ce929d0e0e4736"));
        assert!(got[0].attributes.iter().any(|kv| kv.key.as_str() == "kl.tool.name" && kv.value.as_str() == "read"), "{:?}", got[0].attributes);
        let all = format!("{got:?}");
        for leak in ["secret-dir", "SECRET-CONTENT", "a.txt", "workspaces/api"] {
            assert!(!all.contains(leak), "{leak} reached exported span data: {all}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_unknown_tool_name_is_not_recorded() {
        let (dispatch, spans) = kloudlite_trace::testing::subscriber();
        let _g = tracing::dispatcher::set_default(&dispatch);
        let (_tmp, app) = traced_app();
        assert_eq!(send(&app, axum::http::Request::post("/tools/made-up-secret"), axum::body::Body::empty()).await, 404);
        assert!(!format!("{:?}", spans.get_finished_spans().unwrap()).contains("made-up-secret"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn healthz_is_untraced_and_a_stream_is_one_span() {
        let (dispatch, spans) = kloudlite_trace::testing::subscriber();
        let _g = tracing::dispatcher::set_default(&dispatch);
        let (_tmp, app) = traced_app();
        assert_eq!(send(&app, axum::http::Request::get("/healthz"), axum::body::Body::empty()).await, 200);
        send(&app, axum::http::Request::get("/stream/process/p-123"), axum::body::Body::empty()).await;
        let got = spans.get_finished_spans().unwrap();
        let names: Vec<_> = got.iter().map(|s| s.name.to_string()).collect();
        assert_eq!(names, vec!["GET /stream/process/{id}"]);
        assert!(!format!("{got:?}").contains("p-123"));
    }

    /// The tool server carries NO terminal (spec §2.3, 2026-09-17): a terminal is the shell
    /// sidecar's ttyd on 7790, in a container with the home and no code, no token and no tools.
    /// The three PTY routes and their named-session table are gone, and this is what keeps a
/// `?tree=` on the URL must name the tree exactly as `{"tree": …}` in the body does.
///
/// It did not: `call` handed the body to the registry and discarded the query string, so
/// `exec?tree=T` ran in MAIN and — worse — `write?tree=T` WROTE into main. A caller that believed
/// the query form was doing anything got silent cross-tree writes, and only the agents' own calls
/// (which pass `tree` in the body) were confined at all (R-D22, 2026-09-18).
///
/// Every tool, not just the ones that were noticed: the merge is one place, so a tool added later
/// cannot be the next one to ignore it.
    #[tokio::test]
    async fn the_query_string_names_a_tree_for_every_tool() {
        let (tmp, app) = traced_app();
        let home = tmp.path().canonicalize().unwrap();
        let root = home.join("workspaces/api");
        // The tree is a snapshot of the whole home, so its copy of main sits at the same path below it.
        let tree = home.join(".agents/t/workspaces/api");
        std::fs::create_dir_all(tree.join("sub")).unwrap();
        std::fs::write(tree.join("in-tree.txt"), "tree\n").unwrap();
        std::fs::write(root.join("in-main.txt"), "main\n").unwrap();

        let q = |uri: &'static str, body: &'static str| {
            let app = app.clone();
            async move {
                let req = axum::http::Request::post(uri)
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {TOKEN}"));
                let r = router(app).oneshot(req.body(axum::body::Body::from(body)).unwrap()).await.unwrap();
                let status = r.status().as_u16();
                let v: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap();
                (status, v)
            }
        };

        // read: the tree's own file, and main's is NOT visible from the tree.
        let (st, v) = q("/tools/read?tree=t", r#"{"path":"in-tree.txt"}"#).await;
        assert_eq!((st, v["content"].as_str()), (200, Some("     1\ttree")), "{v}");
        let (st, _) = q("/tools/read?tree=t", r#"{"path":"in-main.txt"}"#).await;
        assert_ne!(st, 200, "the tree can see main's file through the query form");

        // exec: the cwd is the TREE's root, which is what reported main's pwd before.
        let (st, v) = q("/tools/exec?tree=t", r#"{"cmd":"pwd"}"#).await;
        assert_eq!(st, 200, "{v}");
        assert!(v["stdout"].as_str().unwrap().trim().ends_with("/.agents/t/workspaces/api"), "exec ran in main: {v}");

        // write: the file lands in the TREE and main is untouched — the isolation hole itself.
        let (st, v) = q("/tools/write?tree=t", r#"{"path":"written.txt","content":"x"}"#).await;
        assert_eq!(st, 200, "{v}");
        assert!(tree.join("written.txt").exists(), "the write did not land in the tree");
        assert!(!root.join("written.txt").exists(), "the write landed in MAIN: the isolation hole");

        // glob: walks the tree, and never main.
        let (st, v) = q("/tools/glob?tree=t", r#"{"pattern":"*.txt"}"#).await;
        assert_eq!(st, 200, "{v}");
        let hits: Vec<&str> = v["paths"].as_array().unwrap().iter().map(|p| p.as_str().unwrap()).collect();
        assert!(hits.contains(&"in-tree.txt"), "{hits:?}");
        assert!(!hits.contains(&"in-main.txt"), "the glob reached into main: {hits:?}");

        // The BODY still wins when both are given: a caller that pins per session sends the body,
        // and a URL a person happened to type must not override it.
        let (st, v) = q("/tools/exec?tree=main", r#"{"cmd":"pwd","tree":"t"}"#).await;
        assert_eq!(st, 200, "{v}");
        assert!(v["stdout"].as_str().unwrap().trim().ends_with("/.agents/t/workspaces/api"), "the query overrode the body: {v}");

        // An unknown tree in the query is named, never silently main.
        let (st, v) = q("/tools/read?tree=nope", r#"{"path":"in-main.txt"}"#).await;
        assert_eq!(st, 400, "{v}");
    }

    /// The shell sidecar shares this pod's network namespace, so it reaches the tool server on
    /// LOOPBACK, where no NetworkPolicy applies. A `curl` from a person's terminal ran a command
    /// as the workspace user (2026-09-18, ws-632cf9f23d9f2fbf). The fence is the credential now,
    /// and this is what the shell sends: nothing.
    ///
    /// Every route FAMILY, not one route: the hole was that `/tools/exec` was reachable, and a
    /// test of `/tools/exec` alone would not have caught `/fs/file` reading the same tree.
    #[tokio::test]
    async fn without_the_workspace_token_every_route_but_healthz_is_401() {
        let (_t, app) = traced_app();
        let bare = |uri: &'static str, method: &'static str| {
            let app = app.clone();
            async move {
                let req = match method {
                    "POST" => axum::http::Request::post(uri).header("content-type", "application/json"),
                    _ => axum::http::Request::get(uri),
                };
                router(app).oneshot(req.body(axum::body::Body::from("{}")).unwrap()).await.unwrap().status().as_u16()
            }
        };
        for (uri, method) in [
            ("/tools", "GET"),
            ("/tools/exec", "POST"),
            ("/fs/tree", "GET"),
            ("/fs/stat?path=.", "GET"),
            ("/fs/file?path=a.txt", "GET"),
            ("/fs/git", "GET"),
            ("/fs/changes", "GET"),
            ("/fs/diff", "GET"),
            ("/stream/process/p-1", "GET"),
            ("/stream/watch/w-1", "GET"),
        ] {
            assert_eq!(bare(uri, method).await, 401, "{method} {uri} is reachable without a token");
        }
        // Liveness stays open: a kubelet probe has no Secret to read, and the answer says nothing
        // about the workspace beyond "this process is up".
        assert_eq!(bare("/healthz", "GET").await, 200);
    }

    /// A WRONG token is refused exactly like a missing one — no hint that a credential exists or
    /// that this one was close.
    #[tokio::test]
    async fn a_token_that_is_not_this_workspaces_is_401() {
        let (_t, app) = traced_app();
        let r = router(app)
            .oneshot(
                axum::http::Request::post("/tools/exec")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer some-other-workspaces-token")
                    .body(axum::body::Body::from(r#"{"cmd":"true"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
        assert_eq!(r.headers().get("www-authenticate").and_then(|v| v.to_str().ok()), Some("Bearer"));
    }

    /// convenience from quietly putting one back.
    #[tokio::test]
    async fn no_pty_route() {
        // A real token: the point is that the route does not EXIST, and a 401 would hide that
        // behind the credential check.
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config { bind: "127.0.0.1:0".parse().unwrap(), root: "/home/kl/workspaces/api".into(), home: "/home/kl".into(), graft_dir: None, token_path: token_in(tmp.path()) };
        let app = Arc::new(App::new(cfg));
        for path in ["/stream/pty", "/stream/pty/sessions", "/stream/pty/sessions/probe-1"] {
            assert_eq!(send(&app, axum::http::Request::get(path), axum::body::Body::empty()).await, 404, "{path}");
        }
    }

    #[tokio::test]
    async fn healthz_names_the_root() {
        let cfg = Config { bind: "127.0.0.1:0".parse().unwrap(), root: "/home/kl/workspaces/api".into(), home: "/home/kl".into(), graft_dir: None, token_path: None };
        let r = router(Arc::new(App::new(cfg))).oneshot(axum::http::Request::get("/healthz").body(axum::body::Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), 200);
        let b = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["root"], "/home/kl/workspaces/api");
    }

    async fn post(app: &Arc<App>, name: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
        let r = router(app.clone()).oneshot(axum::http::Request::post(format!("/tools/{name}")).header("content-type", "application/json").header("authorization", format!("Bearer {TOKEN}")).body(axum::body::Body::from(body.to_string())).unwrap()).await.unwrap();
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
        let app = Arc::new(App::new(Config { bind: "127.0.0.1:0".parse().unwrap(), token_path: token_in(&home), root, home, graft_dir: None }));
        let r = router(app.clone()).oneshot(axum::http::Request::get("/tools").header("authorization", format!("Bearer {TOKEN}")).body(axum::body::Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap();
        let names: Vec<&str> = v["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["read", "write", "edit", "patch", "glob", "grep", "exec", "process_list", "process_output", "process_write", "process_kill", "watch", "watch_poll", "watch_stop", "graft_find_code", "graft_find_all", "graft_trace_calls", "graft_file_api", "graft_repo_map", "graft_build", "graft_blast"]);
        assert!(!names.contains(&"pty"), "a PTY is not a tool: {names:?}");
        assert_eq!(v["tools"][0]["schema"]["type"], "object");
        let (s, v) = post(&app, "read", serde_json::json!({"path": "a.txt"})).await;
        assert_eq!(s, 200);
        assert!(v["content"].as_str().unwrap().contains("two"));
        // An absolute path is a 400 and a SHAPE to correct, not a 403: there is nothing to deny,
        // and the sentence never names the layout (spec §3.5).
        let (s, v) = post(&app, "read", serde_json::json!({"path": "/etc/passwd"})).await;
        assert_eq!(s, 400, "{v}");
        assert_eq!(v["error"], crate::paths::RELATIVE_ONLY, "{v}");
        // Climbing OUT is the 403, and it names the path the caller can act on.
        let (s, v) = post(&app, "read", serde_json::json!({"path": "../../etc/passwd"})).await;
        assert_eq!(s, 403, "{v}");
        assert!(v["error"].as_str().unwrap().contains("etc/passwd"), "{v}");
        // A tree nobody cut is a 400 naming it, never a read of the workspace instead.
        let (s, v) = post(&app, "read", serde_json::json!({"path": "a.txt", "tree": "nope"})).await;
        assert_eq!(s, 400, "{v}");
        let (s, _) = post(&app, "read", serde_json::json!({})).await;
        assert_eq!(s, 400);
        let (s, _) = post(&app, "nope", serde_json::json!({})).await;
        assert_eq!(s, 404);
        // Bigger than axum's 2 MiB default: the limit is the file tools' own.
        let (s, v) = post(&app, "write", serde_json::json!({"path": "big.txt", "content": "x".repeat(3 << 20)})).await;
        assert_eq!((s, v["bytes"].as_u64()), (200, Some(3 << 20)), "{v}");
    }

    async fn get(app: &Arc<App>, uri: &str, inm: Option<&str>) -> (u16, Option<String>, Vec<u8>) {
        let mut req = axum::http::Request::get(uri).header("authorization", format!("Bearer {TOKEN}"));
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
        let app = Arc::new(App::new(Config { bind: "127.0.0.1:0".parse().unwrap(), root: root.clone(), home: home.clone(), graft_dir: None, token_path: token_in(&home) }));

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
        assert_eq!(get(&app, "/fs/tree?path=/etc", None).await.0, 400, "absolute is a shape error");
        assert_eq!(get(&app, "/fs/tree?path=../../etc", None).await.0, 403);
        assert_eq!(get(&app, "/fs/tree?tree=nope", None).await.0, 400, "an unknown tree is named, never silently main");

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

        // `/fs/log`: the branch's commits newest first, each with what it touched. The second
        // commit renames a file and deletes another, so the A/M/D/R statuses are all exercised
        // rather than only the easy one. The CHANGES tab draws "committed this session" from this.
        sh(&["add", "-A"]);
        sh(&["commit", "-q", "-m", "two"]);
        std::fs::rename(root.join("src/new.rs"), root.join("src/renamed.rs")).unwrap();
        std::fs::remove_file(root.join("README.md")).unwrap();
        sh(&["add", "-A"]);
        sh(&["commit", "-q", "-m", "three"]);

        let (s, etag, b) = get(&app, "/fs/log?n=2", None).await;
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!((s, v["repo"].as_bool()), (200, Some(true)));
        let commits = v["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 2, "n bounds the walk: {v}");
        assert_eq!(commits[0]["subject"], "three", "newest first");
        assert_eq!(commits[1]["subject"], "two");
        assert_eq!(commits[0]["author"], "t");
        assert_eq!(commits[0]["short"].as_str().unwrap().len(), 8);
        assert!(commits[0]["hash"].as_str().unwrap().starts_with(commits[0]["short"].as_str().unwrap()));
        assert!(commits[0]["at"].as_str().unwrap().contains('T'), "RFC 3339: {}", commits[0]["at"]);
        let touched: Vec<(&str, &str)> = commits[0]["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| (f["path"].as_str().unwrap(), f["status"].as_str().unwrap()))
            .collect();
        assert_eq!(touched, vec![("README.md", "D"), ("src/renamed.rs", "R")], "{}", commits[0]["files"]);
        assert_eq!(commits[0]["files"][1]["from"], "src/new.rs", "a rename names where it came from");
        // The first commit of a repository is diffed against the empty tree, not skipped.
        let (_, _, b) = get(&app, "/fs/log?n=99", None).await;
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        let all = v["commits"].as_array().unwrap();
        assert_eq!(all.last().unwrap()["subject"], "one");
        assert_eq!(all.last().unwrap()["files"][0]["status"], "A", "a root commit adds its files");
        assert_eq!(get(&app, "/fs/log?n=2", etag.as_deref()).await.0, 304);
        assert_eq!(get(&app, "/fs/log?n=0", None).await.0, 400);
        assert_eq!(get(&app, "/fs/log?n=201", None).await.0, 400);

        // Not a repository is an answer, not an error.
        let plain = Arc::new(App::new(Config { bind: "127.0.0.1:0".parse().unwrap(), root: home.join("workspaces"), home: home.clone(), graft_dir: None, token_path: token_in(&home) }));
        let (s, _, b) = get(&plain, "/fs/git", None).await;
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!((s, v["repo"].as_bool()), (200, Some(false)));
        let (s, _, b) = get(&plain, "/fs/log", None).await;
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!((s, v["repo"].as_bool(), v["commits"].as_array().unwrap().len()), (200, Some(false), 0));
    }
}
