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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use anyhow::{anyhow, bail, Context, Result};
use futures::FutureExt;
use k8s_openapi::api::core::v1::Pod;
use kloudlite_workspaces::crd::{self, ClusterSettings};
use kube::api::Api;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
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
/// `bench.session.roundtrip`: target 60 s.
const ROUNDTRIP_CEILING: Duration = Duration::from_secs(60);

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

/// One GET through the forward: `(status, body)`. A hand-rolled `read_to_end` + split on
/// `\r\n\r\n` read the body as literal bytes, which is wrong the moment harness-bench answers
/// `Transfer-Encoding: chunked` (Node's default for a streamed JSON body) — the chunk framing
/// (`5f\r\n...\r\n0\r\n\r\n`) landed inside what the probe parsed as JSON and `ok` was never seen.
/// reqwest/hyper decode both chunked and Content-Length correctly, so let it own the response.
async fn through(port: u16, path: &str) -> Result<(u16, String)> {
    through_with(port, reqwest::Method::GET, path, None).await
}

async fn through_with(port: u16, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<(u16, String)> {
    let mut req = reqwest::Client::new().request(method, format!("http://127.0.0.1:{port}{path}")).header("host", "bench");
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().await?;
    let status = resp.status().as_u16();
    let body = resp.text().await?;
    Ok((status, body))
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
                if let Some(before) = before {
                    let after = history(port).await?;
                    diff_history(&before, &after)?;
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
            return SESSION_IDS.iter().for_each(|id| c.skip(id, STUB));
        }
        None => return SESSION_IDS.iter().for_each(|id| c.skip(id, "the bench could not be reached before the sleep")),
        Some(false) => {}
    }
    sessions(c).await;
}

/// The session journeys on a real harness-bench. One prompt feeds two ids: the round trip is timed
/// and judged from the transcript, and the two sockets that watched it are compared afterwards.
async fn sessions(c: &mut Ctx) {
    // What both sockets saw, filled by the round trip for `bench.two_clients` to judge.
    type Seen = Option<(Vec<String>, Vec<String>)>;
    let seen: Arc<Mutex<Seen>> = Default::default();
    let no_model: Arc<Mutex<Option<String>>> = Default::default();
    let created: Arc<Mutex<Option<String>>> = Default::default();
    let (seen_w, no_model_w, created_w) = (seen.clone(), no_model.clone(), created.clone());
    let answered = c
        .step("bench.session.roundtrip", ROUNDTRIP_CEILING, move |c| {
            async move {
                let (_child, port) = forward(c).await?;
                let (status, row) = through_with(port, reqwest::Method::POST, "/sessions", None).await?;
                if status != 201 {
                    bail!("POST /sessions answered {status}: {}", super::clip(&row));
                }
                let sid = serde_json::from_str::<Value>(&row)?["id"].as_str().context("session row missing id")?.to_string();
                *created_w.lock().unwrap() = Some(sid.clone());
                let url = format!("ws://127.0.0.1:{port}/sessions/{sid}/rpc");
                let (mut a, _) = tokio_tungstenite::connect_async(url.as_str()).await.context("socket A")?;
                let (b, _) = tokio_tungstenite::connect_async(url.as_str()).await.context("socket B")?;
                // Both sockets are open before the prompt, so each must see the whole turn.
                let watch_b = tokio::spawn(until_agent_end(b));
                a.send(Message::text(json!({"id": "1", "type": "prompt", "message": PROMPT}).to_string())).await?;
                let (ea, eb) = (until_agent_end(a).await?, watch_b.await??);
                *seen_w.lock().unwrap() = Some((ea, eb));
                let (status, body) = through(port, &format!("/sessions/{sid}/messages")).await?;
                if status != 200 {
                    bail!("GET messages answered {status}");
                }
                match judge_reply(&body)? {
                    Reply::Answered => Ok(()),
                    Reply::NoCredential(why) => {
                        *no_model_w.lock().unwrap() = Some(why.clone());
                        bail!("{why}")
                    }
                }
            }
            .boxed()
        })
        .await;
    let no_model = no_model.lock().unwrap().clone();
    if let Some(why) = &no_model {
        // Not a sample: the probe tenant holds no provider key, so nothing about the bench was measured.
        c.demote_to_skip("bench.session.roundtrip", &format!("{NO_MODEL}: {}", super::clip(why)));
    }
    let seen = seen.lock().unwrap().take();
    match (no_model, seen) {
        (Some(_), _) => c.skip("bench.two_clients", NO_MODEL),
        (None, Some((a, b))) => {
            c.step("bench.two_clients", Duration::from_secs(5), move |_| async move { same_events(&a, &b) }.boxed()).await;
        }
        (None, None) => c.skip("bench.two_clients", if answered { "no events were recorded" } else { "the round trip failed" }),
    }
    exchanges(c).await;
    c.skip("bench.workspace.tool_roundtrip", "needs a running probe workspace and a model credential; neither is provisioned for the probe tenant");
    // Untimed teardown: the bench mints session ids, so no run-{id} prefix exists to sweep by.
    let sid = created.lock().unwrap().take();
    if let Some(sid) = sid {
        let del = async {
            let (_child, port) = forward(c).await?;
            through_with(port, reqwest::Method::DELETE, &format!("/sessions/{sid}"), Some(json!({"stop": true}))).await
        };
        if let Err(e) = del.await {
            tracing::warn!(error = %e, "slo.bench.session.teardown");
        }
    }
}

/// `bench.exchange.both_views`: every exchange a session lists must read back identically through
/// its workspace's view. Nothing the probe does writes an exchange (only a model turn messaging a
/// workspace does), so a bench with none is a skip, never a pass.
async fn exchanges(c: &mut Ctx) {
    let prep = async {
        let (child, port) = forward(c).await?;
        let (_, list) = through(port, "/sessions").await?;
        let mut rows = Vec::new();
        for s in serde_json::from_str::<Vec<Value>>(&list).context("parsing /sessions")? {
            let id = s["id"].as_str().context("session row missing id")?;
            let (_, body) = through(port, &format!("/exchanges?session={id}")).await?;
            rows.extend(serde_json::from_str::<Vec<Value>>(&body).context("parsing ?session=")?);
        }
        anyhow::Ok((child, port, rows))
    }
    .await;
    match prep {
        Ok((_, _, rows)) if rows.is_empty() => c.skip("bench.exchange.both_views", "no exchange exists on the probe bench to read back"),
        Err(e) => c.skip("bench.exchange.both_views", &format!("could not list exchanges: {}", super::clip(&e.to_string()))),
        Ok((child, port, rows)) => {
            c.step("bench.exchange.both_views", Duration::from_secs(30), move |_| {
                async move {
                    let _child = child;
                    let mut by_ws = std::collections::BTreeMap::new();
                    for r in &rows {
                        let ws = r["workspace"].as_str().context("exchange row missing workspace")?.to_string();
                        if !by_ws.contains_key(&ws) {
                            let (_, body) = through(port, &format!("/exchanges?workspace={ws}")).await?;
                            by_ws.insert(ws.clone(), serde_json::from_str::<Vec<Value>>(&body).context("parsing ?workspace=")?);
                        }
                    }
                    views_agree(&rows, &by_ws)
                }
                .boxed()
            })
            .await;
        }
    }
}

const NO_MODEL: &str = "no model credential in the probe tenant";
/// Asks for no tool, so the turn is one model reply and nothing runs on the bench.
const PROMPT: &str = "Reply with exactly the word pong. Do not use any tools.";

/// Every non-response frame a socket sees until this turn's `agent_end`, verbatim.
async fn until_agent_end<S>(mut ws: tokio_tungstenite::WebSocketStream<S>) -> Result<Vec<String>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut out = Vec::new();
    while let Some(msg) = ws.next().await {
        let Message::Text(t) = msg? else { continue };
        let v: Value = serde_json::from_str(&t).context("a frame that is not JSON")?;
        match v["type"].as_str() {
            Some("response") if v["success"] == Value::Bool(false) => bail!("the prompt was refused: {}", super::clip(&t)),
            Some("response") => continue,
            Some("agent_end") => {
                out.push(t.to_string());
                return Ok(out);
            }
            _ => out.push(t.to_string()),
        }
    }
    bail!("the socket closed before agent_end")
}

enum Reply {
    Answered,
    NoCredential(String),
}

/// The transcript's last assistant message: text is an answer, an auth-shaped error is the tenant
/// having no key, and any other error or an empty reply fails.
fn judge_reply(body: &str) -> Result<Reply> {
    let doc: Value = serde_json::from_str(body).context("parsing messages")?;
    let msgs = doc["messages"].as_array().context("messages answer has no messages")?;
    if !msgs.iter().any(|m| m["role"] == "user") {
        bail!("the prompt is not in the transcript");
    }
    let last = msgs.iter().rev().find(|m| m["role"] == "assistant").context("no assistant message in the transcript")?;
    if last["stopReason"] == "error" || last["stopReason"] == "aborted" {
        let why = last["errorMessage"].as_str().unwrap_or_default().to_string();
        let lower = why.to_lowercase();
        if ["api key", "apikey", "auth", "credential", "unauthorized", "401"].iter().any(|k| lower.contains(k)) {
            return Ok(Reply::NoCredential(why));
        }
        bail!("the model turn failed: {}", super::clip(&why));
    }
    let text: String = last["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect();
    if text.trim().is_empty() {
        bail!("the assistant reply is empty");
    }
    Ok(Reply::Answered)
}

fn same_events(a: &[String], b: &[String]) -> Result<()> {
    if a.is_empty() {
        bail!("socket A saw no events");
    }
    if let Some(i) = (0..a.len().max(b.len())).find(|&i| a.get(i) != b.get(i)) {
        bail!("the sockets diverge at event {i} of {} vs {}", a.len(), b.len());
    }
    Ok(())
}

fn views_agree(by_session: &[Value], by_ws: &std::collections::BTreeMap<String, Vec<Value>>) -> Result<()> {
    for r in by_session {
        let ws = r["workspace"].as_str().unwrap_or_default();
        let found = by_ws.get(ws).and_then(|rows| rows.iter().find(|w| w["id"] == r["id"]));
        match found {
            None => bail!("exchange {} is missing from its workspace view", r["id"]),
            Some(w) if w != r => bail!("exchange {} differs between the two views", r["id"]),
            _ => {}
        }
    }
    Ok(())
}

/// Each session's id and message total, in list order — the stable projection compared across a
/// sleep/wake. `lastActive` and other row fields legitimately change on wake (raw JSON does not
/// round-trip identically), so the comparison must not use it; ids and totals must.
async fn history(port: u16) -> Result<Vec<(String, Option<u64>)>> {
    let (status, list) = through(port, "/sessions").await?;
    if status != 200 {
        bail!("GET /sessions answered {status}");
    }
    let rows: Vec<Value> = serde_json::from_str(&list).context("parsing /sessions")?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let id = row["id"].as_str().context("session row missing id")?.to_string();
        let total = total_of(&through(port, &format!("/sessions/{id}/messages")).await?.1);
        out.push((id, total));
    }
    Ok(out)
}

/// Fails on a dropped/renamed session or a changed message total; passes on anything else
/// (row order and any field the projection dropped, e.g. `lastActive`, are not compared).
fn diff_history(before: &[(String, Option<u64>)], after: &[(String, Option<u64>)]) -> Result<()> {
    let before_ids: Vec<&str> = before.iter().map(|(id, _)| id.as_str()).collect();
    let after_ids: Vec<&str> = after.iter().map(|(id, _)| id.as_str()).collect();
    if before_ids != after_ids {
        bail!("session ids changed: {before_ids:?} before, {after_ids:?} after");
    }
    for ((id, b), (_, a)) in before.iter().zip(after.iter()) {
        if b != a {
            bail!("session {id}: {b:?} messages before, {a:?} after");
        }
    }
    Ok(())
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
        assert_eq!(total_of("{\"messages\":[],\"total\": 12}"), Some(12));
    }

    #[test]
    fn history_diff_ignores_volatile_fields_but_catches_a_real_loss() {
        let before = vec![("s-1".to_string(), Some(3)), ("s-2".to_string(), Some(0))];
        // Same ids and totals: a wake that only touched lastActive/ordering-irrelevant fields passes.
        assert!(diff_history(&before, &before.clone()).is_ok());

        let dropped = vec![("s-1".to_string(), Some(3))];
        assert!(diff_history(&before, &dropped).unwrap_err().to_string().contains("session ids changed"));

        let changed_total = vec![("s-1".to_string(), Some(3)), ("s-2".to_string(), Some(5))];
        let err = diff_history(&before, &changed_total).unwrap_err().to_string();
        assert!(err.contains("s-2") && err.contains("Some(0) messages before, Some(5) after"));
    }

    /// A local listener that answers `/healthz` chunked (split across writes, the exact shape
    /// from the fleet evidence) and one that answers with `Content-Length` instead; `through`
    /// must read the JSON body correctly either way, and `health_ok` must see `ok==true`.
    #[tokio::test]
    async fn through_decodes_chunked_and_content_length_bodies() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        async fn serve_once(listener: TcpListener, response: &'static [u8]) {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;
            // Split the write so a body straddling two reads is exercised too.
            let mid = response.len() / 2;
            sock.write_all(&response[..mid]).await.unwrap();
            tokio::task::yield_now().await;
            sock.write_all(&response[mid..]).await.unwrap();
            sock.shutdown().await.unwrap();
        }

        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
            5f\r\n{\"ok\":true,\"readOnly\":false,\"writable\":true,\"clients\":0,\"busy\":false,\"idleSince\":1789355813019}\r\n0\r\n\r\n";
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve_once(listener, chunked));
        let (status, body) = through(port, "/healthz").await.unwrap();
        assert_eq!(status, 200);
        assert!(health_ok(&body), "chunked body did not parse as ok: {body}");

        let cl_body = b"{\"ok\":true}";
        let cl_response: Vec<u8> = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            cl_body.len(),
            std::str::from_utf8(cl_body).unwrap()
        )
        .into_bytes();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let leaked: &'static [u8] = Box::leak(cl_response.into_boxed_slice());
        tokio::spawn(serve_once(listener, leaked));
        let (status, body) = through(port, "/healthz").await.unwrap();
        assert_eq!(status, 200);
        assert!(health_ok(&body));

        assert!(!health_ok("{\"ok\":false}"));
    }

    #[test]
    fn replies_events_and_exchange_views_judge() {
        let user = json!({"role": "user", "content": [{"type": "text", "text": PROMPT}]});
        let ok = json!({"messages": [user, {"role": "assistant", "content": [{"type": "text", "text": "pong"}], "stopReason": "stop"}], "total": 2});
        assert!(matches!(judge_reply(&ok.to_string()).unwrap(), Reply::Answered));
        let nokey = json!({"messages": [user, {"role": "assistant", "content": [], "stopReason": "error", "errorMessage": "No API key found for deepseek"}]});
        assert!(matches!(judge_reply(&nokey.to_string()).unwrap(), Reply::NoCredential(_)));
        let broke = json!({"messages": [user, {"role": "assistant", "content": [], "stopReason": "error", "errorMessage": "socket hang up"}]});
        assert!(judge_reply(&broke.to_string()).is_err());
        let empty = json!({"messages": [user, {"role": "assistant", "content": [{"type": "text", "text": " "}], "stopReason": "stop"}]});
        assert!(judge_reply(&empty.to_string()).is_err());
        assert!(judge_reply(&json!({"messages": []}).to_string()).is_err());

        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(same_events(&s(&["a", "b", "end"]), &s(&["a", "b", "end"])).is_ok());
        assert!(same_events(&s(&["a", "b", "end"]), &s(&["b", "a", "end"])).unwrap_err().to_string().contains("event 0"));
        assert!(same_events(&s(&["a", "end"]), &s(&["a"])).is_err());
        assert!(same_events(&[], &[]).is_err());

        let row = json!({"id": "x1", "session": "s", "workspace": "w", "dir": "out", "text": "hi", "state": "sent", "ts": 1});
        let mut ws = std::collections::BTreeMap::new();
        ws.insert("w".to_string(), vec![row.clone()]);
        assert!(views_agree(std::slice::from_ref(&row), &ws).is_ok());
        ws.insert("w".to_string(), vec![json!({"id": "x1", "session": "s", "workspace": "w", "dir": "out", "text": "hi", "state": "done", "ts": 1})]);
        assert!(views_agree(std::slice::from_ref(&row), &ws).unwrap_err().to_string().contains("differs"));
        ws.insert("w".to_string(), vec![]);
        assert!(views_agree(&[row], &ws).unwrap_err().to_string().contains("missing"));
    }

    #[test]
    fn ceilings_are_at_least_their_targets() {
        use kloudlite_workspaces::slo::catalogue::find;
        for (id, cap) in [("bench.session.roundtrip", ROUNDTRIP_CEILING), ("bench.start.p95", START_CEILING), ("bench.tunnel", TUNNEL_CEILING), ("bench.idle.wake", WAKE_CEILING)] {
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
