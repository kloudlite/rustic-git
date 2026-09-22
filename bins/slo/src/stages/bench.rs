//! The owner's bench: create, start, the tunnel (fast, stage 5); sleep and wake plus the session
//! journey (hourly, Experience); surviving a reschedule (weekly).
//!
//! The Bench's name is a hash of (owner, team), so the `run-{id}` teardown prefix never applies:
//! the probe owner's bench is long-lived and is left Running with no client, so it sleeps between
//! runs and costs nothing. Its region is bound once by hand.
//!
//! The tunnel is the real one — `kl-connect bench` as a child on a local port, handed a config
//! file under the run's tmp — so the probe walks exactly what a laptop walks.
//!
//! A skipped id is NO sample and reaches the run row as `skipped`, never `passed`
//! (`report::run_state`); the service-intercept merge found skipped ids reading as passed, which is
//! why `a_stub_bench_and_the_reschedule_drill_reach_the_run_row_as_skipped` exists.

use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use futures::FutureExt;
use k8s_openapi::api::core::v1::Pod;
use kloudlite_workspaces::crd::{self, ClusterSettings};
use kube::api::Api;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use super::{api, call, get, post, raw};
use crate::ctx::Ctx;

// Ceilings, each at least its catalogue target (`stages/workspace.rs`'s rule): a step that runs
// out of ceiling before its target is a timeout nobody can read as a verdict.
/// `bench.create`: two POSTs.
const CREATE_CEILING: Duration = Duration::from_secs(30);
/// `bench.start.p95`: start from stopped to `ready` only (target 90 s).
const START_CEILING: Duration = Duration::from_secs(120);
/// `bench.tunnel`: target 20 s; the forward waits up to its own 90 s for a waking bench.
const TUNNEL_CEILING: Duration = Duration::from_secs(30);
/// `bench.idle.wake`: target 480 s = default `benchIdleSecs` 300 + 90 s start + 90 s of reads.
const WAKE_CEILING: Duration = Duration::from_secs(540);
/// How long past `benchIdleSecs` the pod gets to be gone.
const IDLE_GRACE: Duration = Duration::from_secs(60);
/// A start's wait, from `kl-connect bench`'s own `BENCH_START_WAIT`.
const START_WAIT: Duration = Duration::from_secs(90);
/// `bench.delegate`: target 600 s — the whole top -> main -> sub chain, including a clone,
/// a real turn and a push back.
const DELEGATE_CEILING: Duration = Duration::from_secs(600);

pub const STUB: &str = "bench image is the stub";
pub const NO_DELETE_GRANT: &str = "no pod-delete grant for the probe";
/// The four ids that read `harness-bench`'s history, in journey order.
const SESSION_IDS: [&str; 4] =
    ["bench.session.roundtrip", "bench.exchange.both_views", "bench.two_clients", "bench.workspace.tool_roundtrip"];

fn bench_url(c: &Ctx, path: &str) -> String {
    api(c, &format!("/v1/bench{path}"))
}

async fn phase(c: &Ctx) -> Result<String> {
    let doc = get(c, &bench_url(c, ""), &c.probe_jwt).await?;
    Ok(doc.get("phase").and_then(Value::as_str).unwrap_or_default().to_string())
}

async fn wait_phase(c: &Ctx, want: &str, cap: Duration) -> Result<()> {
    let start = Instant::now();
    loop {
        let p = phase(c).await?;
        if p == want {
            return Ok(());
        }
        if start.elapsed() >= cap {
            bail!("bench phase {p:?}, never {want:?} within {} s", cap.as_secs());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// `kl-connect bench` on an ephemeral port; the child dies with the handle.
async fn forward(c: &Ctx) -> Result<(Child, u16)> {
    let dir = c.tmp.join("kl-bench");
    std::fs::create_dir_all(&dir)?;
    let cfg = json!({"api": c.cfg.api_url, "token": c.probe_jwt, "expires_at": "2099-01-01T00:00:00Z", "username": c.cfg.probe_user});
    std::fs::write(dir.join("config.json"), cfg.to_string())?;
    let mut child = Command::new(&c.programs.kl)
        .args(["bench", "--port", "0"])
        .env("KL_CONFIG_DIR", &dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("could not start kl-connect bench")?;
    let mut line = String::new();
    let out = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    tokio::time::timeout(Duration::from_secs(10), BufReader::new(out).read_line(&mut line))
        .await
        .context("kl-connect bench printed no port")??;
    let port = line.trim().rsplit(':').next().and_then(|p| p.parse().ok()).ok_or_else(|| anyhow!("no port in {line:?}"))?;
    Ok((child, port))
}

/// One raw HTTP/1.1 GET through the forward: `(status, body)`.
async fn through(port: u16, path: &str) -> Result<(u16, String)> {
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    s.write_all(format!("GET {path} HTTP/1.1\r\nhost: bench\r\nconnection: close\r\n\r\n").as_bytes()).await?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await?;
    split_response(&String::from_utf8_lossy(&buf))
}

/// One raw HTTP/1.1 POST through the forward, JSON body: `(status, body)`. `bench.delegate` is
/// the first caller that needs anything but a GET, so this is new rather than a `through`
/// parameter nobody else would pass.
async fn through_post(port: u16, path: &str, body: &Value) -> Result<(u16, String)> {
    let payload = body.to_string();
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    s.write_all(
        format!(
            "POST {path} HTTP/1.1\r\nhost: bench\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
            payload.len()
        )
        .as_bytes(),
    )
    .await?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await?;
    split_response(&String::from_utf8_lossy(&buf))
}

fn split_response(r: &str) -> Result<(u16, String)> {
    let (head, body) = r.split_once("\r\n\r\n").ok_or_else(|| anyhow!("no HTTP answer: {}", super::clip(r)))?;
    let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or_else(|| anyhow!("bad status line"))?;
    Ok((status, body.to_string()))
}

fn is_stub(healthz: &str) -> bool {
    healthz.trim_start().starts_with("ok stub")
}

/// The real harness-bench answers JSON `{"ok":true,...}`; the stub answered text `ok stub ...`.
fn health_ok(healthz: &str) -> bool {
    is_stub(healthz)
        || serde_json::from_str::<serde_json::Value>(healthz).is_ok_and(|v| v["ok"] == serde_json::Value::Bool(true))
}

/// `"total":N` from a messages answer, read as text so a chunked body still compares.
fn total_of(body: &str) -> Option<u64> {
    let rest = &body[body.find("\"total\":")? + 8..];
    rest.trim_start().split(|ch: char| !ch.is_ascii_digit()).next()?.parse().ok()
}

pub async fn fast(c: &mut Ctx) {
    let region = c.cfg.region.clone();
    let created = c
        .step("bench.create", CREATE_CEILING, move |c| {
            async move {
                let body = json!({"region": region});
                let a = post(c, &bench_url(c, ""), &c.probe_jwt, body.clone()).await?;
                let b = post(c, &bench_url(c, ""), &c.probe_jwt, body).await?;
                let (a, b) = (super::id_of(&a)?, super::id_of(&b)?);
                if a != b {
                    bail!("a second POST named {b}, the first {a}");
                }
                Ok(())
            }
            .boxed()
        })
        .await;
    if !created {
        c.skip("bench.start.p95", "the bench was never created");
        return c.skip("bench.tunnel", "the bench was never created");
    }
    // Untimed: the sample is start→ready only. The 409 holds the moment /stop answers, because
    // `stop_bench` writes `desiredState: Stopped` before its 202 and `bench_session` refuses on
    // that spec field, not on the phase (crates/workspaces/src/api/bench.rs).
    let stopped = async {
        call(c, reqwest::Method::POST, &bench_url(c, "/stop"), &c.probe_jwt, None).await?;
        let (status, text) = raw(c, reqwest::Method::POST, &bench_url(c, "/session"), &c.probe_jwt, None, &[]).await?;
        if status != reqwest::StatusCode::CONFLICT || !text.contains("bench is stopped; start it") {
            bail!("a session on a stopped bench answered {status}: {}", super::clip(&text));
        }
        wait_phase(c, "stopped", START_WAIT).await
    }
    .await;
    let started = c
        .step("bench.start.p95", START_CEILING, move |c| {
            async move {
                stopped.context("the stop round trip before the start")?;
                call(c, reqwest::Method::POST, &bench_url(c, "/start"), &c.probe_jwt, None).await?;
                wait_phase(c, "ready", START_WAIT).await
            }
            .boxed()
        })
        .await;
    if !started {
        return c.skip("bench.tunnel", "the bench never reached ready");
    }
    c.step("bench.tunnel", TUNNEL_CEILING, |c| {
        async move {
            let (_child, port) = forward(c).await?;
            let (status, body) = through(port, "/healthz").await?;
            if status != 200 || !health_ok(&body) {
                bail!("/healthz through the tunnel answered {status}: {}", super::clip(&body));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

pub async fn hourly(c: &mut Ctx) {
    let Some(k) = c.kube.clone() else {
        c.skip("bench.idle.wake", "no kubeconfig");
        return SESSION_IDS.iter().for_each(|id| c.skip(id, "no kubeconfig"));
    };
    // Untimed: this suite's owner is not the fast suite's, so its bench is created here (idempotent,
    // and the first call binds the personal region); then the first connection — which may itself
    // wake last hour's idle bench — reads the history the sample compares against.
    let region = c.cfg.region.clone();
    let prep = async {
        post(c, &bench_url(c, ""), &c.probe_jwt, json!({"region": region})).await.context("could not create this suite's bench")?;
        let idle = Api::<ClusterSettings>::all(k.clone())
            .get_opt("default")
            .await?
            .and_then(|s| s.spec.bench_idle_secs)
            .unwrap_or_else(crd::defaults::bench_idle_secs);
        let (child, port) = forward(c).await?;
        let (_, health) = through(port, "/healthz").await?;
        let stub = is_stub(&health);
        let before = if stub { None } else { Some(history(port).await?) };
        anyhow::Ok((idle, child, stub, before))
    }
    .await;
    let stub = prep.as_ref().ok().map(|p| p.2);
    let woke = c
        .step("bench.idle.wake", WAKE_CEILING, move |c| {
            async move {
                let (idle, child, _, before) = prep.context("before the sleep")?;
                drop(child);
                let owner = c.cfg.probe_user.clone();
                let pods: Api<Pod> = Api::namespaced(k.clone(), &crd::ws_namespace(&owner, &owner));
                // One budget for both waits: idle, then the pod gone.
                let deadline = Instant::now() + Duration::from_secs(idle) + IDLE_GRACE;
                wait_phase(c, "idle", deadline.saturating_duration_since(Instant::now())).await?;
                tracing::info!(check = "phase.idle", "slo.bench.idle");
                while pods.get_opt(kloudlite_workspaces::k8s::BENCH_POD).await?.is_some() {
                    if Instant::now() >= deadline {
                        bail!("the bench is idle and its pod still exists");
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                tracing::info!(check = "pod.absent", "slo.bench.idle");
                let (_child, port) = forward(c).await?;
                let (status, _) = through(port, "/healthz").await?;
                if status != 200 {
                    bail!("a new connection did not wake the bench: /healthz {status}");
                }
                wait_phase(c, "ready", START_WAIT).await?;
                if let Some((list, total)) = before {
                    let (after_list, after_total) = history(port).await?;
                    if after_list != list || after_total != total {
                        bail!("history changed across the sleep: {total:?} messages before, {after_total:?} after");
                    }
                }
                Ok(())
            }
            .boxed()
        })
        .await;
    match stub {
        Some(true) => {
            if woke {
                c.demote_to_skip("bench.idle.wake", STUB);
            }
            c.skip("bench.delegate", STUB);
            return SESSION_IDS.iter().for_each(|id| c.skip(id, STUB));
        }
        None => {
            c.skip("bench.delegate", "the bench could not be reached before the sleep");
            return SESSION_IDS.iter().for_each(|id| c.skip(id, "the bench could not be reached before the sleep"));
        }
        Some(false) => {}
    }
    // ponytail: the four journeys need a harness-bench RPC WebSocket client the probe does not
    // carry yet; skipped (never passed) until the real image ships and the client lands with it.
    SESSION_IDS.iter().for_each(|id| c.skip(id, "the probe has no harness-bench RPC client yet"));
    // bench.delegate needs only plain HTTP through the same tunnel `bench.tunnel` already opens
    // (create/send/children, plus a POST `through` never needed before) — no RPC WebSocket, so
    // it is not blocked by the gap above.
    delegate(c).await;
}

/// `bench.delegate`: top opens a main by workspace, sends it a `delegate to <ws>: ...`
/// instruction, the main hands the work to a sub in a cloned workspace, and the sub's push lands
/// back on main's branch. Judged on OUTPUT throughout (`ws_exec` into the main workspace's tool
/// server, a real `GET /v1/workspaces/{clone}` 404, the child row's own `state`), never on a bare
/// "the call didn't error".
async fn delegate(c: &mut Ctx) {
    let name = format!("{}-delegate", c.prefix());
    c.step("bench.delegate", DELEGATE_CEILING, move |c| {
        let name = name.clone();
        async move {
            let ws = super::experience_ws::create(c, &name, json!({"packages": []})).await?;
            c.state.extra_workspaces.push(ws.clone());

            let (_child, port) = forward(c).await?;

            // Open a main session on the workspace — the real route is `/workspaces/{ws}/session`,
            // not `/sessions/workspaces/{ws}` (the latter does not exist in harness-bench).
            let (status, body) = through_post(port, &format!("/workspaces/{ws}/session"), &json!({})).await?;
            if status != 200 {
                bail!("POST /workspaces/{ws}/session answered {status}: {}", super::clip(&body));
            }

            // The top session always exists (a bench refuses ever having zero, and auto-recreates
            // one on archive) — found by scanning the session list for tier == "top", never minted
            // here.
            let (status, list) = through(port, "/sessions").await?;
            if status != 200 {
                bail!("GET /sessions answered {status}: {}", super::clip(&list));
            }
            let rows: Vec<Value> = serde_json::from_str(&list).context("GET /sessions did not answer a JSON array")?;
            let top = rows
                .iter()
                .find(|r| r.get("tier").and_then(Value::as_str) == Some("top"))
                .and_then(|r| r.get("id").and_then(Value::as_str))
                .ok_or_else(|| anyhow!("no tier=\"top\" session in {}", super::clip(&list)))?
                .to_string();

            let text = format!("delegate to {ws}: create hello.txt containing hi and commit it");
            let (status, body) = through_post(port, &format!("/sessions/{top}/send"), &json!({"text": text})).await?;
            if status != 200 {
                bail!("POST /sessions/{top}/send answered {status}: {}", super::clip(&body));
            }

            // Find the main's own session (opened above by workspace) to poll its children.
            let (status, list2) = through(port, "/sessions").await?;
            if status != 200 {
                bail!("GET /sessions answered {status}: {}", super::clip(&list2));
            }
            let rows2: Vec<Value> = serde_json::from_str(&list2).context("GET /sessions did not answer a JSON array")?;
            let main_id = rows2
                .iter()
                .find(|r| r.get("workspace").and_then(Value::as_str) == Some(ws.as_str()))
                .and_then(|r| r.get("id").and_then(Value::as_str))
                .ok_or_else(|| anyhow!("no session for workspace {ws} in {}", super::clip(&list2)))?
                .to_string();

            // Poll children until one closes — the sub finished and pushed back.
            let deadline = Instant::now() + DELEGATE_CEILING - Duration::from_secs(60);
            let clone_ws = loop {
                let (status, kids) = through(port, &format!("/sessions/{main_id}/children")).await?;
                if status != 200 {
                    bail!("GET /sessions/{main_id}/children answered {status}: {}", super::clip(&kids));
                }
                let kids: Vec<Value> = serde_json::from_str(&kids).context("children did not answer a JSON array")?;
                if let Some(child) = kids.iter().find(|k| k.get("state").and_then(Value::as_str) == Some("closed")) {
                    break child.get("workspace").and_then(Value::as_str).map(str::to_string);
                }
                if Instant::now() >= deadline {
                    bail!("no child closed within {} s; children: {}", DELEGATE_CEILING.as_secs(), super::clip(&serde_json::to_string(&kids).unwrap_or_default()));
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            };
            let clone_ws = clone_ws.ok_or_else(|| anyhow!("the closed child named no workspace"))?;

            // Assert on OUTPUT: the commit landed on main's branch, through the main workspace's
            // own tool server (same `/tools/exec` shape as `ide.exec`), never a bare "no error".
            let (code, out, err) = super::workspace::ws_exec(
                c,
                &ws,
                "curl -sf -X POST http://127.0.0.1:7788/tools/exec -H 'content-type: application/json' -d '{\"cmd\":\"git log -1 --format=%s\"}'",
                Duration::from_secs(30),
            )
            .await?;
            if code != 0 || !out.contains("create hello.txt") {
                bail!("main's tool server exec did not show the commit ({code}): {} {}", out.trim(), err.trim());
            }

            // The clone is gone: the workspace it lived in is a real 404, not merely absent from
            // a list.
            let (status, body) = raw(c, reqwest::Method::GET, &api(c, &format!("/v1/workspaces/{clone_ws}")), &c.probe_jwt, None, &[]).await?;
            if status != reqwest::StatusCode::NOT_FOUND {
                bail!("GET /v1/workspaces/{clone_ws} answered {status}, not 404: {}", super::clip(&body));
            }

            Ok(())
        }
        .boxed()
    })
    .await;
}

/// The session list and the first session's message total, as read through the forward.
async fn history(port: u16) -> Result<(String, Option<u64>)> {
    let (status, list) = through(port, "/sessions").await?;
    if status != 200 {
        bail!("GET /sessions answered {status}");
    }
    let first = list.split("\"id\":\"").nth(1).and_then(|r| r.split('"').next()).map(str::to_string);
    let total = match first {
        Some(id) => total_of(&through(port, &format!("/sessions/{id}/messages")).await?.1),
        None => None,
    };
    Ok((list, total))
}

pub async fn weekly(c: &mut Ctx) {
    c.skip("bench.survives.reschedule", NO_DELETE_GRANT);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::run_state;
    use kloudlite_workspaces::history::slo::RunState;

    #[test]
    fn stub_health_and_totals_read() {
        assert!(is_stub("ok stub running"));
        assert!(!is_stub("{\"clients\":0}"));
        let real = "{\"ok\":true,\"readOnly\":false,\"writable\":true,\"reason\":null,\"clients\":0,\"busy\":false}";
        assert!(!is_stub(real) && health_ok(real));
        assert!(health_ok("ok stub running"));
        assert!(!health_ok("{\"ok\":false}") && !health_ok("oops") && !health_ok("{\"ok\":\"true\"}"));
        assert_eq!(split_response("HTTP/1.1 200 OK\r\na: b\r\n\r\nok stub read-only").unwrap(), (200, "ok stub read-only".into()));
        assert_eq!(total_of("{\"messages\":[],\"total\": 12}"), Some(12));
    }

    #[test]
    fn ceilings_are_at_least_their_targets() {
        use kloudlite_workspaces::slo::catalogue::find;
        for (id, cap) in [("bench.start.p95", START_CEILING), ("bench.tunnel", TUNNEL_CEILING), ("bench.idle.wake", WAKE_CEILING), ("bench.delegate", DELEGATE_CEILING)] {
            assert!(cap.as_millis() >= find(id).unwrap().target.max_ms.unwrap() as u128, "{id}");
        }
    }

    #[tokio::test]
    async fn a_stub_bench_and_the_reschedule_drill_reach_the_run_row_as_skipped() {
        let mut c = crate::testkit::ctx().await;
        c.step("bench.idle.wake", Duration::from_secs(1), |_| async { Ok(()) }.boxed()).await;
        c.demote_to_skip("bench.idle.wake", STUB);
        SESSION_IDS.iter().for_each(|id| c.skip(id, STUB));
        weekly(&mut c).await;
        assert_eq!(c.steps.len(), 6);
        assert!(c.steps.iter().all(|s| s.skipped && !s.ok), "a skip read as a sample");
        assert_eq!(c.failed(), 0);
        assert_eq!(run_state(true, false, &c.steps), RunState::Skipped);
    }
}
