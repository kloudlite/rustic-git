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
/// `bench.shell.roundtrip`: target 15 s.
const SHELL_CEILING: Duration = Duration::from_secs(20);
/// `bench.shell.workspace`: target 20 s; the bench resolves the workspace's tool server first.
const SHELL_WS_CEILING: Duration = Duration::from_secs(30);

pub const STUB: &str = "bench image is the stub";
pub const NO_DELETE_GRANT: &str = "no pod-delete grant for the probe";
/// The ids that need a live `harness-bench`, in journey order. The two shell ids need no model,
/// but they need the same bench, so they skip with the same reasons.
const SESSION_IDS: [&str; 6] = [
    "bench.session.roundtrip",
    "bench.exchange.both_views",
    "bench.two_clients",
    "bench.shell.roundtrip",
    "bench.shell.workspace",
    "bench.workspace.tool_roundtrip",
];

fn bench_url(c: &Ctx, path: &str) -> String {
    api(c, &format!("/v1/bench{path}"))
}

async fn phase(c: &Ctx) -> Result<String> {
    let doc = get(c, &bench_url(c, ""), &c.probe_jwt).await?;
    Ok(doc.get("phase").and_then(Value::as_str).unwrap_or_default().to_string())
}

pub(crate) async fn wait_phase(c: &Ctx, want: &str, cap: Duration) -> Result<()> {
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
        return skip_sessions(c, "no kubeconfig");
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
                if let Err(e) = wait_phase(c, "idle", deadline.saturating_duration_since(Instant::now())).await {
                    // What decides the next one: `clients > 0` is a socket something left open (a
                    // sibling group's dial), `busy` a turn or process still running, and an old
                    // `idleSince` a bench that should have exited. Plain HTTP resets no clock.
                    let health = async { anyhow::Ok(through(forward(c).await?.1, "/healthz").await?.1) }.await;
                    let health = health.unwrap_or_else(|e| format!("unreadable: {e:#}"));
                    bail!("{e:#}; /healthz {}", super::clip(&health));
                }
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
            return skip_sessions(c, STUB);
        }
        None => return skip_sessions(c, "the bench could not be reached before the sleep"),
        Some(false) => {}
    }
    sessions(c).await;
}

const TOOL: &str = "bench.workspace.tool_roundtrip";
const SHELL_WS: &str = "bench.shell.workspace";

/// The session ids this pod walks: a grouped hourly run leaves `TOOL` to group 0.
fn skip_sessions(c: &mut Ctx, why: &str) {
    for id in SESSION_IDS {
        if c.walks(id) {
            c.skip(id, why);
        }
    }
}

/// `TOOL` alone, for group 0 of a grouped hourly run, after the bench journey's group is done.
pub async fn tool_only(c: &mut Ctx) {
    let health = async {
        let (_child, port) = forward(c).await?;
        anyhow::Ok(through(port, "/healthz").await?.1)
    }
    .await;
    match health {
        Ok(h) if is_stub(&h) => {
            c.skip(TOOL, STUB);
            c.skip(SHELL_WS, STUB);
        }
        Ok(_) => {
            let thread = tool_roundtrip(c).await;
            shell_workspace(c).await;
            drop_sessions(c, thread).await;
        }
        Err(e) => {
            let why = format!("the bench could not be reached: {e:#}");
            c.skip(TOOL, &why);
            c.skip(SHELL_WS, &why);
        }
    }
}

/// The session journeys on a real harness-bench. One prompt feeds two ids: the round trip is timed
/// and judged from the transcript, and the two sockets that watched it are compared afterwards.
async fn sessions(c: &mut Ctx) {
    if c.walks("bench.shell.roundtrip") {
        shell_roundtrip(c).await;
    }
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
                // Aborted on drop, so a timeout or an early `?` never leaks socket B.
                let mut watch_b = AbortOnDrop(tokio::spawn(until_agent_end(b)));
                a.send(Message::text(json!({"id": "1", "type": "prompt", "message": PROMPT}).to_string())).await?;
                let ea = mark_no_key(until_agent_end(a).await, &no_model_w)?;
                let eb = mark_no_key((&mut watch_b.0).await?, &no_model_w)?;
                *seen_w.lock().unwrap() = Some((ea, eb));
                let (status, body) = through(port, &format!("/sessions/{sid}/messages")).await?;
                if status != 200 {
                    bail!("GET messages answered {status}");
                }
                answered(&body, &no_model_w)
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
    match (no_model.is_some(), seen) {
        (true, _) => c.skip("bench.two_clients", NO_MODEL),
        (false, Some((a, b))) => {
            c.step("bench.two_clients", Duration::from_secs(5), move |_| async move { same_events(&a, &b) }.boxed()).await;
        }
        (false, None) => c.skip("bench.two_clients", if answered { "no events were recorded" } else { "the round trip failed" }),
    }
    let sid = created.lock().unwrap().take();
    let thread = match &no_model {
        Some(_) => {
            c.skip("bench.exchange.both_views", NO_MODEL);
            if c.walks(TOOL) {
                c.skip(TOOL, NO_MODEL);
                // The shell needs no model, so a missing provider key never skips it.
                shell_workspace(c).await;
            }
            None
        }
        None => {
            exchanges(c, sid.clone()).await;
            if c.walks(TOOL) {
                let thread = tool_roundtrip(c).await;
                shell_workspace(c).await;
                thread
            } else {
                None
            }
        }
    };
    drop_sessions(c, sid.into_iter().chain(thread)).await;
}

/// Untimed teardown: the bench mints session ids, so no run-{id} prefix exists to sweep by. The
/// workspace thread's id is `w-{ws}`, deleted the same way.
async fn drop_sessions(c: &Ctx, ids: impl IntoIterator<Item = String>) {
    for sid in ids {
        let del = async {
            let (_child, port) = forward(c).await?;
            delete_session(port, &sid, TEARDOWN_BOUND).await
        };
        // The forward bounds itself at 10 s; this bounds the whole teardown so a hung bench never stalls the suite.
        match tokio::time::timeout(TEARDOWN_BOUND + Duration::from_secs(10), del).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "slo.bench.session.teardown"),
            Err(_) => tracing::warn!("slo.bench.session.teardown timed out"),
        }
    }
}

/// One shell over the bench's `/pty`, to its end: the protocol's first frame is the resize, input
/// goes as binary frames, output comes back as binary frames, and the server ends with one text
/// control frame. Returns what the shell printed and its exit code.
async fn pty_shell(port: u16, scope: &str, input: &str) -> Result<(String, i64)> {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/pty?scope={scope}")).await.context("pty socket")?;
    ws.send(Message::text(json!({"resize": {"cols": 100, "rows": 30}}).to_string())).await?;
    ws.send(Message::binary(input.as_bytes().to_vec())).await?;
    let mut out = String::new();
    while let Some(msg) = ws.next().await {
        match msg.context("pty frame")? {
            // Not necessarily UTF-8 on a boundary — this is a judgement, not a terminal.
            Message::Binary(b) => out.push_str(&String::from_utf8_lossy(&b)),
            Message::Text(t) => {
                let v: Value = serde_json::from_str(&t).with_context(|| format!("pty control frame {}", super::clip(&t)))?;
                if let Some(e) = v["error"].as_str() {
                    bail!("the shell did not start: {e}");
                }
                let code = v["exit"].as_i64().with_context(|| format!("a control frame that is neither exit nor error: {}", super::clip(&t)))?;
                return Ok((out, code));
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    bail!("the shell closed without an exit frame; output {}", super::clip(&out))
}

/// The shell ran what it was given and left cleanly. `want` is looked for in the OUTPUT, and the
/// PTY echoes the typed line back too — which is why both probes send a line that does not itself
/// contain what is asserted.
fn judge_shell(out: &str, want: &str, code: i64) -> Result<()> {
    if !out.contains(want) {
        bail!("the shell never printed {want}: {}", super::clip(out));
    }
    if code != 0 {
        bail!("the shell exited {code}");
    }
    Ok(())
}

/// `bench.shell.roundtrip`: a shell on the bench itself. `printf` builds the marker, so the echoed
/// command line cannot pass the assertion on its own.
async fn shell_roundtrip(c: &mut Ctx) {
    c.step("bench.shell.roundtrip", SHELL_CEILING, move |c| {
        async move {
            let (_child, port) = forward(c).await?;
            let (out, code) = pty_shell(port, "bench", "printf 'kl-%s\\n' ok; exit 0\n").await?;
            judge_shell(&out, "kl-ok", code)
        }
        .boxed()
    })
    .await;
}

/// `bench.shell.workspace`: the same socket with a workspace scope, which the bench splices to
/// that workspace's tool server — so `pwd` proves both the splice and the shell's cwd. Runs in
/// group 0, which owns the workspace, beside `bench.workspace.tool_roundtrip`.
async fn shell_workspace(c: &mut Ctx) {
    let ws = match tool_workspace(c.state.ux_workspace.clone(), c.state.ux_ready) {
        Ok(ws) => ws,
        Err(why) => return c.skip(SHELL_WS, why),
    };
    // The bench resolves the tool server with its pod token, exactly as the tool round trip does,
    // so the same login has to be live for this step.
    let login = match super::bench_tool::arm(c).await {
        Ok(id) => id,
        Err(why) => return c.skip(SHELL_WS, &why),
    };
    let want = kloudlite_workspaces::k8s::workspace_dir(&ws);
    c.step(SHELL_WS, SHELL_WS_CEILING, move |c| {
        async move {
            let (_child, port) = forward(c).await?;
            let (out, code) = pty_shell(port, &ws, "pwd; exit 0\n").await?;
            judge_shell(&out, &want, code)?;
            // The PROMPT is the product here: starship's character is what says the splice landed
            // in the workspace's own zsh rather than the `/bin/sh` the PTY used to fall back to.
            judge_shell(&out, "❯", code)
        }
        .boxed()
    })
    .await;
    if let Err(e) = super::bench_tool::revoke_login(c, &login).await {
        tracing::warn!(error = %format!("{e:#}"), "slo.bench.shell.login.revoke");
    }
}

/// `bench.exchange.both_views` has no bound (availability only), so this caps one tool call plus a
/// one-word reply: the round trip's own 60 s target, doubled for the extra model step.
const EXCHANGE_CEILING: Duration = Duration::from_secs(120);
/// `bench.workspace.tool_roundtrip`: target 180 s.
const TOOL_CEILING: Duration = Duration::from_secs(180);

/// Only a `kl_workspace_*`/`kl_environment_*` call writes an exchange (harness/pi/kloudlite.ts), and
/// all of them mutate — so the call names a workspace that does not exist: the 404 is recorded as a
/// `failed` exchange with nothing created anywhere, and no teardown is owed for it.
fn exchange_prompt(target: &str) -> String {
    format!("Call the tool kl_workspace_start exactly once with id \"{target}\". Whatever it answers, then reply with exactly the word done.")
}

fn tool_prompt(marker: &str) -> String {
    format!("Use the bash tool exactly once to run: echo {marker}. Then reply with exactly the word done.")
}

/// One prompt on one socket, to this turn's end. A refusal that is pi reporting no provider key
/// lands in `no_model`, so the caller demotes instead of failing.
async fn one_turn(port: u16, sid: &str, prompt: &str, no_model: &Mutex<Option<String>>) -> Result<()> {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/sessions/{sid}/rpc")).await.context("socket")?;
    ws.send(Message::text(json!({"id": "1", "type": "prompt", "message": prompt}).to_string())).await?;
    mark_no_key(until_agent_end(ws).await, no_model).map(|_| ())
}

/// The transcript answered and holds no-key or a real reply; `NoCredential` is written to `no_model`
/// and still fails the step, which the caller then demotes.
fn answered(body: &str, no_model: &Mutex<Option<String>>) -> Result<()> {
    match judge_reply(body)? {
        Reply::Answered => Ok(()),
        Reply::NoCredential(why) => {
            *no_model.lock().unwrap() = Some(why.clone());
            bail!("{why}")
        }
    }
}

/// `bench.exchange.both_views`: the probe's own turn writes an exchange on the round trip's session,
/// and every exchange that session lists must read back identically through its workspace's view.
async fn exchanges(c: &mut Ctx, sid: Option<String>) {
    let Some(sid) = sid else {
        return c.skip("bench.exchange.both_views", "the round trip created no session");
    };
    let target = format!("{}-exchange", c.prefix());
    let no_model: Arc<Mutex<Option<String>>> = Default::default();
    let nm = no_model.clone();
    c.step("bench.exchange.both_views", EXCHANGE_CEILING, move |c| {
        async move {
            let (_child, port) = forward(c).await?;
            let mut rows: Vec<Value> = Vec::new();
            // Two tries: a model may answer without calling the tool; a second miss is a failure.
            for _ in 0..2 {
                one_turn(port, &sid, &exchange_prompt(&target), &nm).await?;
                answered(&through(port, &format!("/sessions/{sid}/messages")).await?.1, &nm)?;
                let (_, body) = through(port, &format!("/exchanges?session={sid}")).await?;
                rows = serde_json::from_str(&body).context("parsing ?session=")?;
                if rows.iter().any(|r| r["workspace"] == target.as_str()) {
                    break;
                }
            }
            if !rows.iter().any(|r| r["workspace"] == target.as_str()) {
                bail!("two turns recorded no exchange naming {target} ({} rows)", rows.len());
            }
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
    if let Some(why) = no_model.lock().unwrap().clone() {
        c.demote_to_skip("bench.exchange.both_views", &format!("{NO_MODEL}: {}", super::clip(&why)));
    };
}

/// `bench.workspace.tool_roundtrip`: a workspace thread on the bench runs `echo` through the tool
/// server of the workspace `ws.packages.add` created — the one live pod this stage keeps. Returns
/// the thread's session id for teardown whenever the step ran: `w-{ws}` is the bench's own naming,
/// so a thread opened but never parsed is still deleted.
async fn tool_roundtrip(c: &mut Ctx) -> Option<String> {
    let ws = match tool_workspace(c.state.ux_workspace.clone(), c.state.ux_ready) {
        Ok(ws) => ws,
        Err(why) => {
            c.skip("bench.workspace.tool_roundtrip", why);
            return None;
        }
    };
    // Untimed: the kubelet's Secret sync is not the round trip. The login stays live through the step
    // so the token keeps passing the gate; a sign-in answer after this is a failure, never a skip.
    let login = match super::bench_tool::arm(c).await {
        Ok(id) => id,
        Err(why) => {
            c.skip("bench.workspace.tool_roundtrip", &why);
            return None;
        }
    };
    let thread = format!("w-{ws}");
    let marker = format!("{}-tool", c.prefix());
    let no_model: Arc<Mutex<Option<String>>> = Default::default();
    let nm = no_model.clone();
    let sid = thread.clone();
    c.step("bench.workspace.tool_roundtrip", TOOL_CEILING, move |c| {
        async move {
            let (_child, port) = forward(c).await?;
            let (status, row) = through_with(port, reqwest::Method::POST, &format!("/workspaces/{ws}/session"), None).await?;
            if status != 200 {
                bail!("POST /workspaces/{ws}/session answered {status}: {}", super::clip(&row));
            }
            // Two tries: a model may answer without calling the tool; a second miss is a failure.
            let mut last = Ok(());
            for _ in 0..2 {
                one_turn(port, &sid, &tool_prompt(&marker), &nm).await?;
                // Read by the workspace route, which is the thread file under /bench/workspaces/{ws}/.
                let (status, body) = through(port, &format!("/workspaces/{ws}/messages")).await?;
                if status != 200 {
                    bail!("GET /workspaces/{ws}/messages answered {status}");
                }
                answered(&body, &nm)?;
                last = tool_ran(&body, &marker);
                if last.is_ok() {
                    break;
                }
            }
            last
        }
        .boxed()
    })
    .await;
    if let Some(why) = no_model.lock().unwrap().clone() {
        c.demote_to_skip("bench.workspace.tool_roundtrip", &format!("{NO_MODEL}: {}", super::clip(&why)));
    }
    if let Err(e) = super::bench_tool::revoke_login(c, &login).await {
        tracing::warn!(error = %format!("{e:#}"), "slo.bench.tool_login.revoke");
    }
    Some(thread)
}

/// The workspace the tool round trip runs in, or why it cannot: a workspace recorded but never
/// ready already failed `ws.packages.add`, and must not fail a second id for the same fault.
fn tool_workspace(ws: Option<String>, ready: bool) -> std::result::Result<String, &'static str> {
    match (ws, ready) {
        (None, _) => Err("the stage's workspace was never created"),
        (Some(_), false) => Err("the stage's workspace never became ready (ws.packages.add failed)"),
        (Some(ws), true) => Ok(ws),
    }
}

/// A successful tool result carrying the marker: the echo ran and its output came back. The call's
/// own arguments hold the marker too, which is why only a `toolResult` counts.
fn tool_ran(body: &str, marker: &str) -> Result<()> {
    let doc: Value = serde_json::from_str(body).context("parsing messages")?;
    let results: Vec<&Value> = doc["messages"].as_array().into_iter().flatten().filter(|m| m["role"] == "toolResult").collect();
    let ran = results.iter().any(|m| {
        m["isError"] != Value::Bool(true)
            && m["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).any(|t| t.contains(marker))
    });
    if !ran {
        // What each result actually said, so the next fleet failure names its cause: the thread is
        // deleted at teardown and cannot be read afterwards. Only `isError` and the text parts —
        // never the whole message, whose other fields are not ours to vouch for.
        let seen: Vec<String> = results
            .iter()
            .map(|m| {
                let text: String = m["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect();
                format!("isError={} {:?}", m["isError"], text.chars().take(300).collect::<String>())
            })
            .collect();
        bail!("no successful tool result carries {marker} ({} tool results: {})", results.len(), seen.join("; "));
    }
    Ok(())
}

const TEARDOWN_BOUND: Duration = Duration::from_secs(10);

struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);
impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn delete_session(port: u16, sid: &str, bound: Duration) -> Result<(u16, String)> {
    tokio::time::timeout(bound, through_with(port, reqwest::Method::DELETE, &format!("/sessions/{sid}"), Some(json!({"stop": true}))))
        .await
        .map_err(|_| anyhow!("DELETE session did not answer within {} s", bound.as_secs()))?
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

/// pi's own wording for "the probe tenant has no key for this provider" — narrow on purpose, so a
/// real model-turn failure (rate limit, timeout, a tool error) still fails the probe instead of
/// silently skipping it. Both `AgentSession.getModel` (unquoted, `formatNoApiKeyFoundMessage`) and
/// `ModelRegistry` (quoted) throw this shape in pi 0.73 and 0.85 alike. Returns the provider name
/// only — never the surrounding message, which may echo other text but never a key.
fn pi_no_api_key_provider(msg: &str) -> Option<String> {
    let after = msg.split_once("No API key found for ")?.1;
    let provider = after.trim_start_matches('"');
    let end = provider.find(['"', '.', '\n']).unwrap_or(provider.len());
    let provider = provider[..end].trim();
    (!provider.is_empty()).then(|| provider.to_string())
}

/// Runs after a socket task that may have failed on a refused prompt: if the failure is pi
/// reporting no provider key, records the provider in `no_model` before propagating the error, so
/// the caller can demote the step (and its dependents) to skip instead of failing it.
fn mark_no_key(res: Result<Vec<String>>, no_model: &Mutex<Option<String>>) -> Result<Vec<String>> {
    if let Err(e) = &res {
        if let Some(provider) = pi_no_api_key_provider(&e.to_string()) {
            *no_model.lock().unwrap() = Some(format!("no API key for {provider}"));
        }
    }
    res
}

/// The transcript's last assistant message: text is an answer, pi reporting no provider key is the
/// tenant having no key, and any other error or an empty reply fails.
fn judge_reply(body: &str) -> Result<Reply> {
    let doc: Value = serde_json::from_str(body).context("parsing messages")?;
    let msgs = doc["messages"].as_array().context("messages answer has no messages")?;
    if !msgs.iter().any(|m| m["role"] == "user") {
        bail!("the prompt is not in the transcript");
    }
    let last = msgs.iter().rev().find(|m| m["role"] == "assistant").context("no assistant message in the transcript")?;
    if last["stopReason"] == "error" || last["stopReason"] == "aborted" {
        let why = last["errorMessage"].as_str().unwrap_or_default().to_string();
        if pi_no_api_key_provider(&why).is_some() {
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

    #[tokio::test]
    async fn teardown_delete_returns_within_its_bound_against_a_silent_bench() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _held = listener.accept().await;
            std::future::pending::<()>().await
        });
        let start = Instant::now();
        assert!(delete_session(port, "s-1", Duration::from_millis(200)).await.is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn abort_on_drop_cancels_the_task() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let guard = AbortOnDrop(tokio::spawn(async move {
            let _tx = tx;
            std::future::pending::<()>().await
        }));
        drop(guard);
        assert!(rx.await.is_err(), "the task still holds its sender");
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

        // pi's WS refusal frame carries the message unquoted, wrapped by `until_agent_end`'s bail
        // context — this is the wording that actually reached the fleet (2026-09-14 hourly run).
        let ws_refusal = "the prompt was refused: {\"id\":\"1\",\"type\":\"response\",\"success\":false,\"error\":\"No API key found for deepseek.\\n\\nUse /login...\"}";
        assert_eq!(pi_no_api_key_provider(ws_refusal).as_deref(), Some("deepseek"));
        // The exact clipped detail from the 2026-09-14 12:02 UTC hourly run
        // (kloudlite-slo-hourly-29823122), byte-for-byte including the `"command":"prompt"` field
        // pi's RPC response envelope carries and the literal `\n\n` escapes as they arrive over the
        // wire (never a real newline) — proves clip()'s 200-char cut lands after the provider name,
        // and that the extra envelope field does not defeat the match.
        let fleet_evidence = "the prompt was refused: {\"id\":\"1\",\"type\":\"response\",\"command\":\"prompt\",\"success\":false,\"error\":\"No API key found for deepseek.\\n\\nUse /login to log into a provider via OAuth or API key. See: /opt/harness/node_modules/@mar";
        assert_eq!(pi_no_api_key_provider(fleet_evidence).as_deref(), Some("deepseek"));
        // ModelRegistry's quoted wording, still narrow.
        assert_eq!(pi_no_api_key_provider("No API key found for \"anthropic\"").as_deref(), Some("anthropic"));
        // A non-auth refusal (rate limit, tool error, ...) must still fail, never skip.
        assert_eq!(pi_no_api_key_provider("the prompt was refused: rate limited, retry later"), None);
        match judge_reply(&broke.to_string()) {
            Err(e) => assert!(e.to_string().contains("the model turn failed")),
            Ok(_) => panic!("a non-auth refusal must fail, not skip or pass"),
        }

        // mark_no_key: an auth-shaped socket failure records the provider and still propagates
        // the error; a non-auth failure passes through untouched.
        let no_model: Mutex<Option<String>> = Mutex::new(None);
        assert!(mark_no_key(Err(anyhow!("{ws_refusal}")), &no_model).is_err());
        assert_eq!(no_model.lock().unwrap().as_deref(), Some("no API key for deepseek"));
        let no_model2: Mutex<Option<String>> = Mutex::new(None);
        assert!(mark_no_key(Err(anyhow!("connection reset")), &no_model2).is_err());
        assert!(no_model2.lock().unwrap().is_none());

        // bench.two_clients' own skip reason: skipped (not "the round trip failed") whenever the
        // round trip itself was a credential skip, and only reports the generic failure otherwise.
        let dependent_reason = |no_model: Option<&str>, answered: bool| match no_model {
            Some(_) => NO_MODEL,
            None if answered => "no events were recorded",
            None => "the round trip failed",
        };
        assert_eq!(dependent_reason(Some("no API key for deepseek"), false), NO_MODEL);
        assert_eq!(dependent_reason(None, false), "the round trip failed");

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
        for (id, cap) in [("bench.shell.roundtrip", SHELL_CEILING), (SHELL_WS, SHELL_WS_CEILING), ("bench.workspace.tool_roundtrip", TOOL_CEILING), ("bench.session.roundtrip", ROUNDTRIP_CEILING), ("bench.start.p95", START_CEILING), ("bench.tunnel", TUNNEL_CEILING), ("bench.idle.wake", WAKE_CEILING)] {
            assert!(cap.as_millis() >= find(id).unwrap().target.max_ms.unwrap() as u128, "{id}");
        }
    }

    #[test]
    fn a_tool_turn_is_judged_from_its_result_not_its_call() {
        let m = "run-abc-tool";
        let call = json!({"role": "assistant", "content": [{"type": "toolCall", "id": "t1", "name": "bash", "arguments": {"command": format!("echo {m}")}}]});
        let ok = json!({"messages": [call, {"role": "toolResult", "toolCallId": "t1", "isError": false, "content": [{"type": "text", "text": m}]}]});
        assert!(tool_ran(&ok.to_string(), m).is_ok());
        // The marker only in the call: the tool never answered.
        assert!(tool_ran(&json!({"messages": [call]}).to_string(), m).is_err());
        let failed = json!({"messages": [call, {"role": "toolResult", "toolCallId": "t1", "isError": true, "content": [{"type": "text", "text": format!("{m}\n[exit 1]")}]}]});
        let why = tool_ran(&failed.to_string(), m).unwrap_err().to_string();
        // The failure carries what the tool said, so a fleet run names its cause.
        assert!(why.contains("isError=true") && why.contains("[exit 1]"), "{why}");
        assert!(tool_workspace(None, true).is_err());
        assert!(tool_workspace(Some("w".into()), false).unwrap_err().contains("ws.packages.add"));
        assert_eq!(tool_workspace(Some("w".into()), true).unwrap(), "w");
        assert!(tool_prompt(m).contains(m) && exchange_prompt("run-abc-exchange").contains("kl_workspace_start"));

        // The sign-in answer is an ordinary failed round trip now that the probe mints the token.
        let login = json!({"role": "toolResult", "toolCallId": "t1", "isError": false, "content": [{"type": "text", "text": "sign in on the Kloudlite desktop app"}]});
        assert!(tool_ran(&json!({"messages": [call, login]}).to_string(), m).is_err());

        let no_model = Mutex::new(None);
        let nokey = json!({"messages": [{"role": "user", "content": "x"}, {"role": "assistant", "content": [], "stopReason": "error", "errorMessage": "No API key found for deepseek"}]});
        assert!(answered(&nokey.to_string(), &no_model).is_err());
        assert!(no_model.lock().unwrap().is_some(), "a missing key must be recorded for the demote");
    }

    #[tokio::test]
    async fn a_stub_bench_and_the_reschedule_drill_reach_the_run_row_as_skipped() {
        let mut c = crate::testkit::ctx().await;
        c.step("bench.idle.wake", Duration::from_secs(1), |_| async { Ok(()) }.boxed()).await;
        c.demote_to_skip("bench.idle.wake", STUB);
        SESSION_IDS.iter().for_each(|id| c.skip(id, STUB));
        weekly(&mut c).await;
        assert_eq!(c.steps.len(), 8);
        assert!(c.steps.iter().all(|s| s.skipped && !s.ok), "a skip read as a sample");
        assert_eq!(c.failed(), 0);
        assert_eq!(run_state(true, false, &c.steps), RunState::Skipped);
    }
}
