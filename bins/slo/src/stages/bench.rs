//! The owner's bench: create, start, the tunnel (fast, stage 5); sleep and wake plus the session
//! journey (hourly, Experience); surviving a reschedule (weekly).
//!
//! The bench's name is a hash of (owner, team), so the `run-{id}` teardown prefix never applies:
//! the probe owner's bench is long-lived and is left Running with no client, so it sleeps between
//! runs and costs nothing. Its region is bound once by hand.
//!
//! The tunnel is the real one — `kl-connect bench` as a child on a local port, handed a config
//! file under the run's tmp — so the probe walks exactly what a laptop walks.
//!
//! Everything that used to speak to the retired harness-bench HTTP server now runs INSIDE the pod
//! by kube-exec (design 2026-10-05): `node`'s global `fetch` against `kl-sessions`
//! (`127.0.0.1:8917`'s `/state`, `/send`, `/idle`), the same way `experience_teams::paused` already
//! reads the pod's mounted tool token. The tunnel itself carries raw bytes to sshd now, so only
//! `bench.tunnel`'s SSH-banner check and `shell.up`'s direct gateway `/term` GET still use it.
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
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::TcpStream;
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
/// How long `status.idleSince` gets to land after the pod is gone: one reconcile pass writes both.
const IDLE_STAMP: Duration = Duration::from_secs(30);
/// A start's wait, from `kl-connect bench`'s own `BENCH_START_WAIT`.
const START_WAIT: Duration = Duration::from_secs(90);
/// `shell.up`: target 15 s.
const SHELL_CEILING: Duration = Duration::from_secs(20);
/// `bench.claude.tool_roundtrip`: target 120 s.
const CLAUDE_TOOL_CEILING: Duration = Duration::from_secs(120);
/// `bench.builtin.refused`: no bound target (`avail`), but a step still needs a cap.
const BUILTIN_REFUSED_CEILING: Duration = Duration::from_secs(120);
const EXEC: Duration = Duration::from_secs(20);
pub const STUB: &str = "bench image is the stub";
pub const NO_DELETE_GRANT: &str = "no pod-delete grant for the probe";
/// The ids that need a live bench and are walked straight from `hourly`, in journey order.
const SESSION_IDS: [&str; 1] = ["shell.up"];

fn bench_url(c: &Ctx, path: &str) -> String {
    api(c, &format!("/v1/bench{path}"))
}

async fn phase(c: &Ctx) -> Result<String> {
    let doc = get(c, &bench_url(c, ""), &c.probe_jwt).await?;
    Ok(doc.get("phase").and_then(Value::as_str).unwrap_or_default().to_string())
}

/// The `bench` container's readiness as the pod reports it; `None` while there is no status for
/// it yet. The agent reads the same field.
fn bench_ready(pod: &Pod) -> Option<bool> {
    pod.status.as_ref()?.container_statuses.as_ref()?.iter().find(|c| c.name == kloudlite_workspaces::k8s::BENCH_CONTAINER).map(|c| c.ready)
}

/// One `POST /v1/bench/session`, polled to 201: 202 is the wake being asked for, not a failure.
async fn wake(c: &Ctx) -> Result<()> {
    let url = bench_url(c, "/session");
    let start = Instant::now();
    loop {
        let (status, body) = raw(c, reqwest::Method::POST, &url, &c.probe_jwt, None, &[]).await?;
        if status == reqwest::StatusCode::CREATED {
            return Ok(());
        }
        if status != reqwest::StatusCode::ACCEPTED {
            bail!("POST /v1/bench/session answered {status}: {}", super::clip(&body));
        }
        if start.elapsed() >= START_WAIT {
            bail!("the bench never woke: /v1/bench/session still answers 202 {}", super::clip(&body));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// One `POST /v1/bench/session` that keeps the answer: `(id, token, gateway base url)`. Still
/// live after the agent-CLI rework (it mints the tunnel ticket), and `shell.up` uses it to hit
/// the gateway's `/term` route directly rather than through the tunnel.
async fn session_triple(c: &Ctx) -> Result<(String, String, String)> {
    let (status, body) = raw(c, reqwest::Method::POST, &bench_url(c, "/session"), &c.probe_jwt, None, &[]).await?;
    if status != reqwest::StatusCode::CREATED {
        bail!("POST /v1/bench/session answered {status}: {}", super::clip(&body));
    }
    let v: Value = serde_json::from_str(&body).context("parsing /v1/bench/session")?;
    let get = |k: &str| v[k].as_str().map(str::to_string).with_context(|| format!("no {k:?} in session answer"));
    Ok((get("id")?, get("token")?, get("gateway")?))
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

/// `kl-connect bench` on an ephemeral port; the child dies with the handle. Still needed by
/// `bench.tunnel`'s raw SSH-banner check — the only thing left on this end of the tunnel now that
/// it carries raw bytes to sshd and not an HTTP API.
pub(crate) async fn forward(c: &Ctx) -> Result<(Child, u16)> {
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

/// `wss://ws-{region}.khost.dev/tunnel/{id}` → `https://ws-{region}.khost.dev` (origin only, no
/// new `url` crate): the gateway terminates TLS itself and mounts `/term/...` at its ORIGIN, not
/// under `/tunnel/...` (`bins/gateway/src/term.rs`), the same rule `web/apps/web/src/lib/term-url.ts`'s
/// `termUrl` follows.
fn term_origin(gateway: &str) -> Result<String> {
    let (scheme, rest) = gateway.split_once("://").with_context(|| format!("no scheme in {gateway:?}"))?;
    let https = match scheme {
        "wss" => "https",
        "ws" => "http",
        other => other,
    };
    let host = rest.split('/').next().unwrap_or(rest);
    Ok(format!("{https}://{host}"))
}

/// One `node -e` exec into the bench container, print-and-check convention shared with
/// `bench_tool.rs`'s `node`/`pod_call`: `js` is a self-contained script, `argv` lands in
/// `process.argv.slice(1)` (never shell-interpolated, so a prompt with metacharacters is safe).
async fn in_bench(c: &Ctx, js: &str, argv: &[&str]) -> Result<String> {
    let k = c.kube.as_ref().ok_or_else(|| anyhow!("no kubeconfig"))?;
    let owner = &c.cfg.probe_user;
    let pod = super::bench_pod(c, None).await?;
    let mut full = vec!["node", "-e", js];
    full.extend_from_slice(argv);
    let (code, out, err) =
        crate::kube::exec(k, &crd::ws_namespace(owner, owner), &pod, Some(kloudlite_workspaces::k8s::BENCH_CONTAINER), &full, EXEC).await?;
    if code != 0 {
        bail!("node in the bench pod exited {code}: {}", super::clip(&err));
    }
    Ok(out)
}

/// `(status, body)` printed by every `*_JS` script below, mirroring `bench_tool::parse_call`.
fn parse_status_body(out: &str) -> Result<(u16, String)> {
    let (status, body) = out.split_once('\n').unwrap_or((out, ""));
    Ok((status.trim().parse().with_context(|| format!("no status in {:?}", super::clip(out)))?, body.trim().to_string()))
}

const STATE_JS: &str = r#"fetch("http://127.0.0.1:8917/state").then(async r=>{console.log(r.status);console.log(await r.text())}).catch(e=>{console.log(0);console.log(String(e))})"#;
const IDLE_JS: &str = r#"fetch("http://127.0.0.1:8917/idle").then(async r=>{console.log(r.status);console.log(await r.text())}).catch(e=>{console.log(0);console.log(String(e))})"#;
/// `argv[1]` is the workspace session id, `argv[2]` the prompt text — never shell-interpolated.
const SEND_JS: &str = r#"const [ws,text]=process.argv.slice(1);fetch("http://127.0.0.1:8917/send",{method:"POST",headers:{"content-type":"application/json"},body:JSON.stringify({ws,text})}).then(async r=>{console.log(r.status);console.log((await r.text()).slice(0,500))}).catch(e=>{console.log(0);console.log(String(e).slice(0,500))})"#;

/// `kl-sessions`' `/state`, from inside the pod. Status 0 is the fetch never answering at all
/// (connection refused): the stub bench image runs no `kl-sessions`, so this is how a stub is told
/// apart from a real one now that there is no `/healthz`.
async fn state_raw(c: &Ctx) -> Result<(u16, String)> {
    parse_status_body(&in_bench(c, STATE_JS, &[]).await?)
}

async fn state(c: &Ctx) -> Result<Value> {
    let (status, body) = state_raw(c).await?;
    if status != 200 {
        bail!("GET /state from the pod answered {status}: {}", super::clip(&body));
    }
    serde_json::from_str(&body).context("parsing /state")
}

async fn send(c: &Ctx, ws: &str, text: &str) -> Result<()> {
    let (status, body) = parse_status_body(&in_bench(c, SEND_JS, &[ws, text]).await?)?;
    if status != 200 {
        bail!("POST /send answered {status}: {}", super::clip(&body));
    }
    Ok(())
}

/// Poll `/idle` until `busy` is false, which is how the caller knows a sent turn has finished.
async fn wait_not_busy(c: &Ctx, cap: Duration) -> Result<()> {
    let start = Instant::now();
    loop {
        if let Ok((200, body)) = parse_status_body(&in_bench(c, IDLE_JS, &[]).await?) {
            if let Ok(v) = serde_json::from_str::<Value>(&body) {
                if v["busy"] == Value::Bool(false) {
                    return Ok(());
                }
            }
        }
        if start.elapsed() >= cap {
            bail!("kl-sessions still busy {} s after the prompt", cap.as_secs());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
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
            // The tunnel now carries raw bytes to sshd (`BENCH_PORT`), not an HTTP API: its own
            // protocol banner is the only thing to check from this end.
            let (_child, port) = forward(c).await?;
            let mut sock = TcpStream::connect(("127.0.0.1", port)).await.context("dialling the tunnel")?;
            let mut buf = [0u8; 16];
            let n = tokio::time::timeout(Duration::from_secs(10), sock.read(&mut buf)).await.context("reading the SSH banner")??;
            if !buf[..n].starts_with(b"SSH-2.0-") {
                bail!("the tunnel's first bytes were not an SSH banner: {:?}", String::from_utf8_lossy(&buf[..n]));
            }
            // ttyd, reached the browser's way: through the gateway, never the tunnel.
            let (id, token, gateway) = session_triple(c).await?;
            let origin = term_origin(&gateway)?;
            let url = format!("{origin}/term/{id}/?token={token}");
            let status = reqwest::Client::new().get(&url).send().await.context("GET gateway /term")?.status();
            if status != 200 {
                bail!("GET {{origin}}/term/{{id}}/ answered {status}");
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// Fails on a dropped/renamed session or a changed transcript (`lines`); passes on anything else.
fn diff_state(before: &Value, after: &Value) -> Result<()> {
    let keys = |v: &Value| -> Vec<String> { v.as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default() };
    let (mut b, mut a) = (keys(before), keys(after));
    b.sort();
    a.sort();
    if b != a {
        bail!("session ids changed: {b:?} before, {a:?} after");
    }
    for id in &b {
        if before[id]["lines"] != after[id]["lines"] {
            bail!("session {id}: the transcript changed across the sleep/wake");
        }
    }
    Ok(())
}

pub async fn hourly(c: &mut Ctx) {
    let Some(k) = c.kube.clone() else {
        c.skip("bench.idle.wake", "no kubeconfig");
        return skip_sessions(c, "no kubeconfig");
    };
    // Untimed: this suite's owner is not the fast suite's, so its bench is created here (idempotent,
    // and the first call binds the personal region); then the first read — which may itself wake
    // last hour's idle bench — is the `/state` snapshot the sample compares against.
    let region = c.cfg.region.clone();
    let prep = async {
        post(c, &bench_url(c, ""), &c.probe_jwt, json!({"region": region})).await.context("could not create this suite's bench")?;
        let idle = Api::<ClusterSettings>::all(k.clone())
            .get_opt("default")
            .await?
            .and_then(|s| s.spec.bench_idle_secs)
            .unwrap_or_else(crd::defaults::bench_idle_secs);
        let (status, body) = state_raw(c).await?;
        let stub = status == 0;
        let before = if stub { None } else { Some(serde_json::from_str::<Value>(&body).context("parsing /state")?) };
        anyhow::Ok((idle, stub, before))
    }
    .await;
    let stub = prep.as_ref().ok().map(|p| p.1);
    let woke = c
        .step("bench.idle.wake", WAKE_CEILING, move |c| {
            async move {
                let (idle, _, before) = prep.context("before the sleep")?;
                let owner = c.cfg.probe_user.clone();
                let pods: Api<Pod> = Api::namespaced(k.clone(), &crd::ws_namespace(&owner, &owner));
                // The pod is named by the bench's workspace id; there is no constant for it.
                let pod_name = super::bench_pod(c, None).await?;
                // One budget for the whole chain: readiness false, then the pod gone, then idle.
                let deadline = Instant::now() + Duration::from_secs(idle) + IDLE_GRACE;
                // The idle SIGNAL is the `bench` container's readiness, not the pod's exit code —
                // `kl-sessions` keeps serving now that the pod's restartPolicy is the workspace's
                // `Always` (bins/agent/src/controller/workspace/bench.rs). So the chain is
                // asserted in the order the agent walks it: readiness false is what it believes,
                // deleting the pod is what it does, `status.idleSince` (the facade's `idle`
                // phase) is what it records.
                let mut saw_not_ready = false;
                loop {
                    match pods.get_opt(&pod_name).await? {
                        None => break,
                        Some(pod) => {
                            if bench_ready(&pod) == Some(false) && !saw_not_ready {
                                saw_not_ready = true;
                                tracing::info!(check = "bench.notready", "slo.bench.idle");
                            }
                        }
                    }
                    if Instant::now() >= deadline {
                        let state = state_raw(c).await.map(|(s, b)| format!("{s} {}", super::clip(&b))).unwrap_or_else(|e| format!("unreadable: {e:#}"));
                        let what = if saw_not_ready { "the bench container went unready and its pod still exists" } else { "the bench container never went unready" };
                        bail!("{what}; /state {state}");
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                if !saw_not_ready {
                    bail!("the bench pod went away without the `bench` container ever reporting unready");
                }
                tracing::info!(check = "pod.absent", "slo.bench.idle");
                // Its own small window, not what is left of `deadline`: the agent stamps
                // `status.idleSince` in the same pass that deletes the pod, and a loop that ran
                // close to its deadline would report a stamp that never had time to land.
                wait_phase(c, "idle", IDLE_STAMP).await.context("the pod is gone but the bench is not idle")?;
                tracing::info!(check = "phase.idle", "slo.bench.idle");
                // The wake is `/v1/bench/session`, the call every client makes: on an idle bench it
                // patches `wakeAt` and answers 202, and the client re-asks until 201. Still mints
                // the tunnel ticket, so this stays the wake call even with no HTTP left in the pod.
                wake(c).await?;
                wait_phase(c, "ready", START_WAIT).await?;
                let (status, body) = state_raw(c).await?;
                if status != 200 {
                    bail!("a woken bench's /state answered {status}: {}", super::clip(&body));
                }
                if let Some(before) = before {
                    let after: Value = serde_json::from_str(&body).context("parsing /state")?;
                    diff_state(&before, &after)?;
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
    shell_up(c).await;
}

/// The session ids this pod walks straight from `hourly`.
fn skip_sessions(c: &mut Ctx, why: &str) {
    for id in SESSION_IDS {
        if c.walks(id) {
            c.skip(id, why);
        }
    }
}

/// `shell.up`: the gateway's own `/term` route answers for the session token `session_triple`
/// minted, built the way `web/apps/web/src/lib/term-url.ts`'s `termUrl` does (ruling 2, task 9 fix
/// round 1) — the tunnel itself carries raw bytes to sshd now, so this never goes through it.
async fn shell_up(c: &mut Ctx) {
    c.step("shell.up", SHELL_CEILING, move |c| {
        async move {
            let (id, token, gateway) = session_triple(c).await?;
            let origin = term_origin(&gateway)?;
            let url = format!("{origin}/term/{id}/?token={token}");
            let status = reqwest::Client::new().get(&url).send().await.context("GET gateway /term")?.status();
            if status != 200 {
                bail!("GET {{origin}}/term/{{id}}/ answered {status}");
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

fn tool_workspace(ws: Option<String>, ready: bool) -> std::result::Result<String, &'static str> {
    match (ws, ready) {
        (None, _) => Err("the stage's workspace was never created"),
        (Some(_), false) => Err("the stage's workspace never became ready (ws.packages.add failed)"),
        (Some(ws), true) => Ok(ws),
    }
}

/// A line starting `t:` is a tool call, `r:` its result (`kl-sessions`' own transcript prefixes,
/// `docs/superpowers/specs/2026-10-05-claude-code-bench-design.md`).
fn claude_tool_ran(state: &Value, ws: &str) -> Result<()> {
    let lines = state[ws]["lines"].as_array().context("no lines for the workspace session in /state")?;
    let text: Vec<&str> = lines.iter().filter_map(Value::as_str).collect();
    if !text.iter().any(|l| l.starts_with("t:")) {
        bail!("no tool call in the transcript: {}", super::clip(&text.join("\n")));
    }
    if !text.iter().any(|l| l.starts_with("r:")) {
        bail!("no tool result in the transcript: {}", super::clip(&text.join("\n")));
    }
    Ok(())
}

/// `bench.claude.tool_roundtrip`: `POST /send` on the probe workspace asks the agent to read the
/// workspace's README, and `/state` shows a tool call answered from the workspace.
async fn claude_tool_roundtrip(c: &mut Ctx) {
    const ID: &str = "bench.claude.tool_roundtrip";
    const PROMPT: &str = "Use the read tool to read this workspace's README.md and summarize it in one sentence.";
    let ws = match tool_workspace(c.state.ux_workspace.clone(), c.state.ux_ready) {
        Ok(ws) => ws,
        Err(why) => return c.skip(ID, why),
    };
    if c.kube.is_none() {
        return c.skip(ID, "no kubeconfig");
    }
    c.step(ID, CLAUDE_TOOL_CEILING, move |c| {
        async move {
            wait_phase(c, "ready", START_WAIT).await.context("the bench is not ready")?;
            send(c, &ws, PROMPT).await?;
            wait_not_busy(c, CLAUDE_TOOL_CEILING).await?;
            let state = state(c).await?;
            claude_tool_ran(&state, &ws)
        }
        .boxed()
    })
    .await;
}

/// The deny hook's own wording (`bench/mod/hooks/register.tsx`): a built-in tool call is refused,
/// never run, and the refusal reaches the transcript carrying this substring.
const BUILTIN_DENY_MARKER: &str = "no local tools";

fn builtin_denied(state: &Value, ws: &str) -> Result<()> {
    let lines = state[ws]["lines"].as_array().context("no lines for the workspace session in /state")?;
    let text: Vec<&str> = lines.iter().filter_map(Value::as_str).collect();
    if !text.iter().any(|l| l.contains(BUILTIN_DENY_MARKER)) {
        bail!("the built-in tool was not refused: {}", super::clip(&text.join("\n")));
    }
    Ok(())
}

/// `bench.builtin.refused`: a prompt forcing the built-in Bash tool is denied, never run.
async fn builtin_refused(c: &mut Ctx) {
    const ID: &str = "bench.builtin.refused";
    const PROMPT: &str = "Use the Bash tool to run the command: echo hi";
    let ws = match tool_workspace(c.state.ux_workspace.clone(), c.state.ux_ready) {
        Ok(ws) => ws,
        Err(why) => return c.skip(ID, why),
    };
    if c.kube.is_none() {
        return c.skip(ID, "no kubeconfig");
    }
    c.step(ID, BUILTIN_REFUSED_CEILING, move |c| {
        async move {
            wait_phase(c, "ready", START_WAIT).await.context("the bench is not ready")?;
            send(c, &ws, PROMPT).await?;
            wait_not_busy(c, BUILTIN_REFUSED_CEILING).await?;
            let state = state(c).await?;
            builtin_denied(&state, &ws)
        }
        .boxed()
    })
    .await;
}

/// Dispatched from `experience.rs`'s two arms (ruling 3, task 9 fix round 1): kube-exec has no
/// contention with the tunnel, so unlike the retired `bench.workspace.tool_roundtrip` these need
/// no `suite::wait_for_group` coordination.
pub async fn claude_tools(c: &mut Ctx) {
    claude_tool_roundtrip(c).await;
    builtin_refused(c).await;
}

pub async fn weekly(c: &mut Ctx) {
    c.skip("bench.survives.reschedule", NO_DELETE_GRANT);
}

// `ws.terminal.persists` is RETIRED (spec §2.3, 2026-09-17): the tool server has no PTY any more
// and a terminal is a ttyd socket inside the workspace container itself (owner ruling 2026-09-25:
// no shell sidecar), so nothing survives a restart by design — "a dropped connection is a new
// shell". The bench's own terminal is now `kl-connect bench`/ttyd onto the tmux session, covered
// by `bench.tunnel`.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::run_state;
    use kloudlite_workspaces::history::slo::RunState;

    #[test]
    fn term_origin_swaps_scheme_and_drops_the_path() {
        assert_eq!(term_origin("wss://ws-blr.khost.dev/tunnel/abc").unwrap(), "https://ws-blr.khost.dev");
        assert_eq!(term_origin("ws://ws-blr.khost.dev/tunnel/abc").unwrap(), "http://ws-blr.khost.dev");
        assert!(term_origin("not-a-url").is_err());
    }

    #[test]
    fn status_body_parses_and_state_diff_catches_drift() {
        assert_eq!(parse_status_body("200\n{\"ok\":true}\n").unwrap(), (200, "{\"ok\":true}".to_string()));
        assert_eq!(parse_status_body("0\nTypeError: fetch failed").unwrap().0, 0);
        assert!(parse_status_body("").is_err());

        let before = json!({"s-1": {"lines": ["u:hi", "r:ok"]}, "s-2": {"lines": []}});
        assert!(diff_state(&before, &before).is_ok());
        let dropped = json!({"s-1": {"lines": ["u:hi", "r:ok"]}});
        assert!(diff_state(&before, &dropped).unwrap_err().to_string().contains("session ids changed"));
        let changed = json!({"s-1": {"lines": ["u:hi", "r:ok"]}, "s-2": {"lines": ["u:new"]}});
        assert!(diff_state(&before, &changed).unwrap_err().to_string().contains("s-2"));
    }

    #[test]
    fn transcripts_are_judged_by_their_prefixes() {
        let state = json!({"w-1": {"lines": ["u:summarize", "t:read(README.md)", "r:ok summary"]}});
        assert!(claude_tool_ran(&state, "w-1").is_ok());
        assert!(claude_tool_ran(&json!({"w-1": {"lines": ["u:hi"]}}), "w-1").is_err());

        let denied = json!({"w-1": {"lines": ["u:echo hi", "s:This bench runs no local tools; use the workspace's tools (mcp__kloudlite__*) instead."]}});
        assert!(builtin_denied(&denied, "w-1").is_ok());
        assert!(builtin_denied(&json!({"w-1": {"lines": ["u:echo hi", "r:hi"]}}), "w-1").is_err());

        assert!(tool_workspace(None, true).is_err());
        assert!(tool_workspace(Some("w".into()), false).unwrap_err().contains("ws.packages.add"));
        assert_eq!(tool_workspace(Some("w".into()), true).unwrap(), "w");
    }

    #[test]
    fn ceilings_are_at_least_their_targets() {
        use kloudlite_workspaces::slo::catalogue::find;
        for (id, cap) in [
            ("shell.up", SHELL_CEILING),
            ("bench.claude.tool_roundtrip", CLAUDE_TOOL_CEILING),
            ("bench.start.p95", START_CEILING),
            ("bench.tunnel", TUNNEL_CEILING),
            ("bench.idle.wake", WAKE_CEILING),
        ] {
            assert!(cap.as_millis() >= find(id).unwrap().target.max_ms.unwrap_or(0) as u128, "{id}");
        }
    }

    #[tokio::test]
    async fn a_stub_bench_and_the_reschedule_drill_reach_the_run_row_as_skipped() {
        let mut c = crate::testkit::ctx().await;
        c.step("bench.idle.wake", Duration::from_secs(1), |_| async { Ok(()) }.boxed()).await;
        c.demote_to_skip("bench.idle.wake", STUB);
        SESSION_IDS.iter().for_each(|id| c.skip(id, STUB));
        weekly(&mut c).await;
        // The weekly skip plus every `SESSION_IDS` this fixture files.
        assert_eq!(c.steps.len(), 1 + SESSION_IDS.len() + 1);
        assert!(c.steps.iter().all(|s| s.skipped && !s.ok), "a skip read as a sample");
        assert_eq!(c.failed(), 0);
        assert_eq!(run_state(true, false, &c.steps), RunState::Skipped);
    }
}
