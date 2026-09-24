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
/// How long `status.idleSince` gets to land after the pod is gone: one reconcile pass writes both.
const IDLE_STAMP: Duration = Duration::from_secs(30);
/// A start's wait, from `kl-connect bench`'s own `BENCH_START_WAIT`.
const START_WAIT: Duration = Duration::from_secs(90);
/// `bench.session.roundtrip`: target 60 s.
const ROUNDTRIP_CEILING: Duration = Duration::from_secs(60);
/// `bench.shell.roundtrip`: target 15 s.
const SHELL_CEILING: Duration = Duration::from_secs(20);
/// `bench.shell.workspace`: target 20 s; the bench resolves the workspace's tool server first,
/// and the id now walks a named session twice plus its listing and its kill.
const SHELL_WS_CEILING: Duration = Duration::from_secs(45);
/// `bench.delegate`: target 600 s — the whole top -> main -> sub chain, including a clone,
/// a real turn and a push back.
const DELEGATE_CEILING: Duration = Duration::from_secs(600);
pub const STUB: &str = "bench image is the stub";
pub const NO_DELETE_GRANT: &str = "no pod-delete grant for the probe";
/// The ids that need a live `harness-bench`, in journey order. The two shell ids need no model,
/// but they need the same bench, so they skip with the same reasons.
const SESSION_IDS: [&str; 8] = [
    "bench.session.roundtrip",
    "shell.up",
    "shell.fenced",
    "shell.no_tools",
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

/// The `bench` container's readiness as the pod reports it; `None` while there is no status for
/// it yet. The agent reads the same field.
fn bench_ready(pod: &Pod) -> Option<bool> {
    pod.status.as_ref()?.container_statuses.as_ref()?.iter().find(|c| c.name == kloudlite_workspaces::k8s::BENCH_CONTAINER).map(|c| c.ready)
}

/// The SHELL container's readiness, the same way.
fn shell_ready(pod: &Pod) -> Option<bool> {
    pod.status.as_ref()?.container_statuses.as_ref()?.iter().find(|c| c.name == kloudlite_workspaces::k8s::SHELL_CONTAINER).map(|c| c.ready)
}

/// How long a shell may take to come up before its probes give up. The shell waits on the Nix
/// profile — an evaluation and a fetch on a cold node — and since it no longer GATES the pod's
/// readiness (2026-09-18) the pod is `Ready` well before ttyd is listening. So the probes wait for
/// the shell container itself rather than dialling into a port nothing holds yet.
const SHELL_WAIT: Duration = Duration::from_secs(150);

/// Block until the bench pod's shell container reports ready, or say why it did not.
///
/// A pod with no shell container at all is an ERROR, not a wait: that is the sidecar missing from
/// the pod spec, which is exactly what these ids exist to catch.
async fn await_shell(c: &Ctx) -> Result<()> {
    let Some(k) = c.kube.clone() else { return Ok(()) };
    let owner = c.cfg.probe_user.clone();
    let pods: Api<Pod> = Api::namespaced(k, &crd::ws_namespace(&owner, &owner));
    let name = super::bench_pod(c, None).await?;
    let start = Instant::now();
    loop {
        // Read on every pass and reported only on the way out, so the message names what the pod
        // last said rather than a guess: `None` is no pod, `Some(None)` a pod with no shell
        // container status yet, `Some(Some(false))` a shell still waiting on the profile.
        let seen = match pods.get_opt(&name).await? {
            Some(pod) => match shell_ready(&pod) {
                Some(true) => return Ok(()),
                other => Some(other),
            },
            None => None,
        };
        if start.elapsed() >= SHELL_WAIT {
            bail!("the shell container was not ready after {} s (readiness {seen:?})", SHELL_WAIT.as_secs());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
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
                // The pod is named by the bench's workspace id; there is no constant for it.
                let pod_name = super::bench_pod(c, None).await?;
                // One budget for the whole chain: readiness false, then the pod gone, then idle.
                let deadline = Instant::now() + Duration::from_secs(idle) + IDLE_GRACE;
                // The idle SIGNAL is the `bench` container's readiness, not the pod's exit code —
                // `harness-bench` keeps serving now that the pod's restartPolicy is the
                // workspace's `Always` (bins/agent/src/controller/workspace/bench.rs). So the
                // chain is asserted in the order the agent walks it: readiness false is what it
                // believes, deleting the pod is what it does, `status.idleSince` (the facade's
                // `idle` phase) is what it records.
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
                        // What decides this: `clients > 0` is a socket something left open (a
                        // sibling group's dial), `busy` a turn or process still running. Plain
                        // HTTP resets no clock.
                        let health = async { anyhow::Ok(through(forward(c).await?.1, "/healthz").await?.1) }.await;
                        let health = health.unwrap_or_else(|e| format!("unreadable: {e:#}"));
                        let what = if saw_not_ready { "the bench container went unready and its pod still exists" } else { "the bench container never went unready" };
                        bail!("{what}; /healthz {}", super::clip(&health));
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
                // patches `wakeAt` and answers 202, and the client re-asks until 201.
                wake(c).await?;
                wait_phase(c, "ready", START_WAIT).await?;
                let (_child, port) = forward(c).await?;
                let (status, _) = through(port, "/healthz").await?;
                if status != 200 {
                    bail!("a woken bench answered /healthz {status}");
                }
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
            let (status, body) = through_with(port, reqwest::Method::POST, &format!("/workspaces/{ws}/session"), Some(json!({}))).await?;
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
            let (status, body) = through_with(port, reqwest::Method::POST, &format!("/sessions/{top}/send"), Some(json!({"text": text}))).await?;
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

const TOOL: &str = "bench.workspace.tool_roundtrip";
const SHELL_WS: &str = "bench.shell.workspace";

/// The session ids this pod walks: a grouped hourly run leaves `TOOL` to group 0.
fn skip_sessions(c: &mut Ctx, why: &str) {
    if c.walks("bench.delegate") {
        c.skip("bench.delegate", why);
    }
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
    // ONCE, before the first dial and outside every step: the shell waits on the Nix profile and
    // no longer gates the pod's readiness (2026-09-18), so the pod is `Ready` while ttyd is still
    // being fetched and `shell.up` and `bench.shell.roundtrip` timed out at 20 s and 45 s on a
    // cold node. Waiting here keeps each ceiling measuring the DIAL, which is what they are the
    // target for, rather than the profile build, which is `ws.packages.*`'s to measure.
    own_workspace(c).await;
    // The bench resolves a workspace-scoped shell or tool with its pod token (`harness/pi/
    // kloudlite.ts` answers "sign in on the Kloudlite desktop app" without one), and only a live
    // CLI login mints that token: the hourly 2026-09-23 22:19 IST run failed all three ids on it.
    let login = if c.state.ux_workspace.is_some() && NEEDS_WORKSPACE.iter().any(|id| c.walks(id)) {
        super::bench_tool::arm(c)
            .await
            .map_err(|why| tracing::warn!(error = %why, "slo.bench.tool_token.not_armed"))
            .ok()
    } else {
        None
    };
    if c.walks("bench.shell.roundtrip") || c.walks("shell.up") || c.walks("shell.fenced") || c.walks("shell.no_tools") {
        if let Err(e) = await_shell(c).await {
            tracing::warn!(error = %format!("{e:#}"), "slo.bench.shell.not_ready");
        }
    }
    if c.walks("bench.shell.roundtrip") {
        shell_roundtrip(c).await;
    }
    if c.walks("shell.up") {
        shell_up(c).await;
    }
    if c.walks("shell.fenced") {
        shell_fenced(c).await;
    }
    if c.walks("shell.no_tools") {
        shell_no_tools(c).await;
    }
    if let Some(login) = login {
        if let Err(e) = super::bench_tool::revoke_login(c, &login).await {
            tracing::warn!(error = %format!("{e:#}"), "slo.bench.tool_token.login.revoke");
        }
    }
    // What both `/events` sockets saw, filled by the round trip for `bench.two_clients` to judge.
    type Seen = Option<(Vec<String>, Vec<String>)>;
    let seen: Arc<Mutex<Seen>> = Default::default();
    let no_model: Arc<Mutex<Option<String>>> = Default::default();
    let created: Arc<Mutex<Option<String>>> = Default::default();
    let (seen_w, no_model_w, created_w) = (seen.clone(), no_model.clone(), created.clone());
    c.step("bench.session.roundtrip", ROUNDTRIP_CEILING, move |c| {
        async move {
            let (_child, port) = forward(c).await?;
            let (status, row) = through_with(port, reqwest::Method::POST, "/sessions", None).await?;
            if status != 201 {
                bail!("POST /sessions answered {status}: {}", super::clip(&row));
            }
            let sid = serde_json::from_str::<Value>(&row)?["id"].as_str().context("session row missing id")?.to_string();
            *created_w.lock().unwrap() = Some(sid.clone());
            // Both sockets are open before the send, so each must see the whole turn. Aborted on
            // drop, so a timeout or an early `?` never leaks socket B.
            let url = format!("ws://127.0.0.1:{port}/events");
            let (a, _) = tokio_tungstenite::connect_async(url.as_str()).await.context("socket A")?;
            let (b, _) = tokio_tungstenite::connect_async(url.as_str()).await.context("socket B")?;
            let sid_a = sid.clone();
            let sid_b = sid.clone();
            let mut watch_b = AbortOnDrop(tokio::spawn(collect_rows(b, sid_b)));
            let mut watch_a = AbortOnDrop(tokio::spawn(collect_rows(a, sid_a)));
            let turn = one_turn(port, &sid, PROMPT, &no_model_w);
            turn.await?;
            let ea = (&mut watch_a.0).await?;
            let eb = (&mut watch_b.0).await?;
            *seen_w.lock().unwrap() = Some((ea, eb));
            Ok(())
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
        (false, Some((a, b))) if a.is_empty() && b.is_empty() => c.skip("bench.two_clients", "no events were recorded"),
        (false, Some((a, b))) => {
            c.step("bench.two_clients", Duration::from_secs(5), move |_| async move { same_events(&a, &b) }.boxed()).await;
        }
        (false, None) => c.skip("bench.two_clients", "the round trip failed"),
    }
    let sid = created.lock().unwrap().take();
    let thread = if no_model.is_none() && c.walks(TOOL) {
        let thread = tool_roundtrip(c).await;
        shell_workspace(c).await;
        thread
    } else if c.walks(SHELL_WS) {
        // The shell needs no model, so a missing provider key never skips it.
        shell_workspace(c).await;
        None
    } else {
        None
    };
    drop_sessions(c, sid.into_iter().chain(thread)).await;
}

/// Every `type:"row"` frame this socket sees for `sid`, until (and including) that session's
/// `turn.end` row, as the row's own JSON text. The socket is a plain `/events` firehose — no
/// per-session subscribe — so every frame not naming this session is dropped.
async fn collect_rows<S>(mut ws: tokio_tungstenite::WebSocketStream<S>, sid: String) -> Vec<String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut out = Vec::new();
    while let Some(Ok(Message::Text(t))) = ws.next().await {
        let Ok(v) = serde_json::from_str::<Value>(&t) else { continue };
        if v["type"] != "row" || v["session"].as_str() != Some(sid.as_str()) {
            continue;
        }
        let is_end = v["row"]["kind"] == "turn.end";
        out.push(v["row"].to_string());
        if is_end {
            break;
        }
    }
    out
}

/// One shell over the bench's `/pty`, to its end: the protocol's first frame is the resize, input
/// goes as binary frames, output comes back as binary frames, and the server ends with one text
/// control frame. Returns what the shell printed and its exit code.
/// One shell exchange through the bench's `/pty` splice, which is a TRANSPARENT pipe to the
/// sidecar's ttyd (`harness/bench/src/pty.ts`, `spliceShell`). So this speaks ttyd's own
/// protocol, not the retired tool-server PTY's:
///
/// | direction | opcode | payload |
/// |---|---|---|
/// | client → | (first frame) | `{"AuthToken":"","columns":N,"rows":N}` |
/// | client → | `0` | input bytes |
/// | → client | `0` | output bytes |
/// | → client | `1` | the title |
/// | → client | `2` | ttyd's preferences JSON |
///
/// Two things this got wrong until 2026-09-18, both of which made every shell id time out while
/// the shell itself was perfectly healthy: it sent a bare JSON resize and unprefixed input, which
/// ttyd reads as opcode bytes of its own; and it waited for a JSON `exit` control frame, which
/// nothing in this path ever sends — ttyd closes the socket instead (verified against the live
/// bench: frames `BIN op=1, op=2, op=0`, marker received, `CLOSE code 1000`, no exit frame). The
/// probe therefore held the socket open until its ceiling, which is the 24 s ttyd logged.
///
/// The EXIT STATUS is gone with it: ttyd reports none. The caller's script prints its own status
/// marker instead, which is also what a person reads off a terminal.
pub(crate) async fn pty_shell(port: u16, scope: &str, input: &str) -> Result<(String, i64)> {
    let (mut ws, _) = tokio_tungstenite::connect_async_with_config(
        tokio_tungstenite::tungstenite::handshake::client::Request::builder()
            .uri(format!("ws://127.0.0.1:{port}/pty?scope={scope}"))
            .header("Host", format!("127.0.0.1:{port}"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", tokio_tungstenite::tungstenite::handshake::client::generate_key())
            // ttyd refuses a socket that does not ask for its subprotocol, and the splice carries
            // the handshake through.
            .header("Sec-WebSocket-Protocol", "tty")
            .body(())?,
        None,
        false,
    )
    .await
    .context("pty socket")?;
    // The auth frame is FIRST and carries the size; the token is empty because the NetworkPolicy
    // is the fence here, not ttyd's own auth (spec §2.3).
    ws.send(Message::text(json!({"AuthToken": "", "columns": 100, "rows": 30}).to_string())).await?;
    let mut framed = vec![b'0'];
    framed.extend_from_slice(input.as_bytes());
    ws.send(Message::binary(framed)).await?;
    let mut out = String::new();
    while let Some(msg) = ws.next().await {
        match msg.context("pty frame")? {
            // Opcode `0` is OUTPUT; the title (`1`) and the preferences (`2`) say nothing about
            // what the shell did. Not necessarily UTF-8 on a boundary — this is a judgement, not
            // a terminal.
            Message::Binary(b) => {
                if let Some((b'0', rest)) = b.split_first() {
                    out.push_str(&String::from_utf8_lossy(rest));
                }
            }
            // The splice's own control frame, and the only place an error is reported now.
            Message::Text(t) => {
                let v: Value = serde_json::from_str(&t).with_context(|| format!("pty control frame {}", super::clip(&t)))?;
                if let Some(e) = v["error"].as_str() {
                    bail!("the shell did not start: {e}");
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    // A clean close is how a ttyd shell ends; `0` keeps `judge_shell`'s shape for callers.
    Ok((out, 0))
}





/// The shell ran what it was given and left cleanly. `want` is looked for in the OUTPUT, and the
/// PTY echoes the typed line back too — which is why both probes send a line that does not itself
/// contain what is asserted.
fn judge_shell(out: &str, want: &str, code: i64) -> Result<()> {
    if !out.contains(want) {
        bail!("the shell never printed {want}: {}", super::clip(out));
    }
    // ttyd reports no exit status (it is not in its protocol), so `pty_shell` answers 0 and the
    // OUTPUT is the whole judgement — a script that wants its status asserted prints it. Kept as
    // a parameter rather than removed so a caller that does have one is still checked.
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


/// `shell.up`: the SHELL SIDECAR answers, and it sees only the home (spec §2.5).
///
/// One id, both pod kinds: the bench's own shell and the run's workspace shell are the same
/// container image with the same mounts, and a failure in either is the same defect. `pwd` says
/// the shell opens in the home; the workspaces root being ABSENT is what says the code is not
/// there — the sidecar mounts the home and the profile and nothing else.
async fn shell_up(c: &mut Ctx) {
    let ws = tool_workspace(c.state.ux_workspace.clone(), c.state.ux_ready).ok();
    c.step("shell.up", SHELL_CEILING, move |c| {
        async move {
            let (_child, port) = forward(c).await?;
            let (out, code) = pty_shell(port, "bench", "pwd; exit 0\n").await?;
            judge_shell(&out, kloudlite_workspaces::k8s::HOME_DIR, code)?;
            let Some(ws) = ws else {
                // The workspace half is a stronger assertion than the bench half; say it was not
                // made rather than passing on half the id.
                bail!("the stage's workspace was never created, so only the bench shell was checked");
            };
            // `ls` of the source directory: the sidecar has no such mount, so the shell must not
            // find the workspace's code there. An empty answer and an error are both correct.
            let (out, _) = pty_shell(port, &ws, &format!("ls {}; pwd; exit 0\n", kloudlite_workspaces::k8s::WORKSPACE_DIR)).await?;
            if out.contains(&format!("{}/", kloudlite_workspaces::k8s::WORKSPACE_DIR)) {
                bail!("the shell can see the workspaces root: {}", super::clip(&out));
            }
            if !out.contains(kloudlite_workspaces::k8s::HOME_DIR) {
                bail!("the workspace shell did not open in the home: {}", super::clip(&out));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `shell.no_tools`: the tool server REFUSES the shell, and the refusal is a 401.
///
/// Deliberately not "the connection is refused" (spec §2.5): the two containers share the pod's
/// network namespace, so a dial of 127.0.0.1:7788 from the shell CONNECTS. What stops it is that
/// every tool-server request needs the bench token and the shell has none. Stated here so nobody
/// later "fixes" the probe by expecting a refused connection.
async fn shell_no_tools(c: &mut Ctx) {
    let ws = match tool_workspace(c.state.ux_workspace.clone(), c.state.ux_ready) {
        Ok(ws) => ws,
        Err(why) => return c.skip("shell.no_tools", why),
    };
    c.step("shell.no_tools", SHELL_CEILING, move |c| {
        async move {
            let (_child, port) = forward(c).await?;
            let script = format!(
                "curl -s -o /dev/null -w '%{{http_code}}\\n' --max-time 5 http://127.0.0.1:{}/tools; exit 0\n",
                kloudlite_workspaces::k8s::IDE_PORT
            );
            let (out, code) = pty_shell(port, &ws, &script).await?;
            judge_shell(&out, "401", code).map_err(|e| {
                anyhow!("{e:#} — the tool server must answer the token-less shell 401, never serve it")
            })
        }
        .boxed()
    })
    .await;
}

/// `shell.fenced`: nothing outside the fence dials 7790.
///
/// From the PROBE pod, which is not the person's bench and is in another namespace: the
/// `allow-bench-tools` policy admits the bench pod alone, so this connect must fail. A connect
/// that SUCCEEDS is the whole finding — the shell has no auth of its own, the fence is the auth.
async fn shell_fenced(c: &mut Ctx) {
    let Some(k) = c.kube.clone() else {
        return c.skip("shell.fenced", "no kubeconfig");
    };
    let owner = c.cfg.probe_user.clone();
    c.step("shell.fenced", SHELL_CEILING, move |c| {
        async move {
            let pod = super::bench_pod(c, None).await?;
            let ns = kloudlite_workspaces::crd::ws_namespace(&owner, &owner);
            let ip = kube::Api::<Pod>::namespaced(k.clone(), &ns)
                .get_opt(&pod)
                .await?
                .and_then(|p| p.status.and_then(|s| s.pod_ip))
                .context("the bench pod has no address")?;
            let addr = format!("{ip}:{}", kloudlite_workspaces::k8s::SHELL_PORT);
            // A real socket, never a shell's `/dev/tcp`: the DIAL is the assertion, and a policy
            // that dropped the packet must read as a timeout rather than as a shell's exit code.
            match tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(&addr)).await {
                Err(_) => Ok(()),
                Ok(Err(_)) => Ok(()),
                Ok(Ok(_)) => bail!("{addr} accepted a connection from outside the fence"),
            }
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
    // The shell sidecar opens in its own scratch home: it never mounts the worktree volume, which
    // IS the workspace's home since 2026-09-22.
    let want = kloudlite_workspaces::k8s::HOME_DIR.to_string();
    c.step(SHELL_WS, SHELL_WS_CEILING, move |c| {
            async move {
                let (_child, port) = forward(c).await?;
                let (out, code) = pty_shell(port, &ws, "pwd; exit 0\n").await?;
                judge_shell(&out, &want, code)?;
                // The PROMPT is the product here: starship's character is what says the splice landed
                // in the workspace's own zsh rather than the `/bin/sh` the PTY used to fall back to.
                judge_shell(&out, "❯", code)?;
                // NO reattach assertion any more (spec §2.3): a terminal is a ttyd socket to the
                // shell sidecar, named sessions and replay are retired, and "a dropped connection
                // is a new shell". What this id still holds is the one thing that survived: the
                // splice lands in the WORKSPACE's own shell, at its own directory, with its prompt.
                Ok(())
            }
            .boxed()
        })
    .await;
    // No session sweep any more: a terminal has no name and no life beyond its socket.
    if let Err(e) = super::bench_tool::revoke_login(c, &login).await {
        tracing::warn!(error = %format!("{e:#}"), "slo.bench.shell.login.revoke");
    }
}

/// Say yes to every proposal the bench is holding, until the caller drops this.
///
/// Since 2026-09-17 every `kl_*` write is a QUESTION: the extension publishes it and blocks on
/// `/proposals/{id}/wait` for up to ten minutes, and an unanswered question is a no. A probe that
/// prompts a turn into a platform write is the person in that conversation, so it answers — spawned
/// beside the turn rather than after it, because the turn does not end until the answer lands.
/// `bench.workspace.tool_roundtrip`: target 180 s.
const TOOL_CEILING: Duration = Duration::from_secs(180);

fn tool_prompt(marker: &str) -> String {
    format!("Use the bash tool exactly once to run: echo {marker}. Then reply with exactly the word done.")
}

/// One turn, driven the way the new engine actually runs one: `POST /sessions/{sid}/send`, then
/// poll `GET /sessions/{sid}/messages` until a `turn.end` row for the turn number that send
/// returned. An `error` containing `API_KEY` is the tenant having no provider key — recorded in
/// `no_model` so the caller demotes instead of failing; any other `error` fails outright; an empty
/// or absent `answer` on a clean end fails too. Callers' own step ceilings bound the wait.
async fn one_turn(port: u16, sid: &str, prompt: &str, no_model: &Mutex<Option<String>>) -> Result<()> {
    let (status, body) = through_with(port, reqwest::Method::POST, &format!("/sessions/{sid}/send"), Some(json!({"text": prompt}))).await?;
    if status != 200 {
        bail!("POST /sessions/{sid}/send answered {status}: {}", super::clip(&body));
    }
    let turn = serde_json::from_str::<Value>(&body)?["turn"].as_u64().context("send answered no `turn`")?;
    loop {
        let (status, body) = through(port, &format!("/sessions/{sid}/messages")).await?;
        if status != 200 {
            bail!("GET /sessions/{sid}/messages answered {status}: {}", super::clip(&body));
        }
        let doc: Value = serde_json::from_str(&body).context("parsing messages")?;
        let rows = doc["rows"].as_array().context("messages answer has no `rows`")?;
        let end = rows.iter().find(|r| r["kind"] == "turn.end" && r["turn"].as_u64() == Some(turn));
        if let Some(end) = end {
            if let Some(err) = end["error"].as_str() {
                if err.contains("API_KEY") {
                    *no_model.lock().unwrap() = Some(err.to_string());
                    return Ok(());
                }
                bail!("the turn ended in error: {}", super::clip(err));
            }
            let answer = end["answer"].as_str().unwrap_or_default();
            if answer.trim().is_empty() {
                bail!("the assistant reply is empty");
            }
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
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
            if status != 200 && status != 201 {
                bail!("POST /workspaces/{ws}/session answered {status}: {}", super::clip(&row));
            }
            // Two tries: a model may answer without calling the tool; a second miss is a failure.
            let mut last = Ok(());
            for _ in 0..2 {
                one_turn(port, &sid, &tool_prompt(&marker), &nm).await?;
                // Read by the workspace route, which is the thread file under the bench folder's
                // own `workspaces/{ws}/` — `~/.bench`, never a `/bench` mount.
                let (status, body) = through(port, &format!("/workspaces/{ws}/messages")).await?;
                if status != 200 {
                    bail!("GET /workspaces/{ws}/messages answered {status}");
                }
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

/// The ids of this group that need a live workspace besides the bench.
const NEEDS_WORKSPACE: [&str; 2] = ["shell.up", "shell.no_tools"];

/// A workspace of this pod's own for `NEEDS_WORKSPACE`, when `ws.packages.add` (group 0) did not
/// make one here. Grouping split the two after `shell.up` was written against the shared one, and
/// every grouped hourly run since failed it as "never created" (hourly 2026-09-23 19:53 IST).
/// Plain, no packages: these ids judge the shell and the tool server, not the profile.
async fn own_workspace(c: &mut Ctx) {
    if c.state.ux_workspace.is_some() || !NEEDS_WORKSPACE.iter().any(|id| c.walks(id)) {
        return;
    }
    let name = format!("{}-b", c.prefix());
    match super::experience_ws::create(c, &name, serde_json::json!({})).await {
        Ok(id) => {
            c.state.ux_workspace = Some(id);
            c.state.ux_ready = true;
        }
        Err(e) => tracing::warn!(error = %format!("{e:#}"), "slo.bench.workspace.not_created"),
    }
}

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

/// Deletes every session this stage opened, best-effort: teardown never fails the run, only warns.
async fn drop_sessions(c: &Ctx, ids: impl IntoIterator<Item = String>) {
    let Ok((_child, port)) = forward(c).await else { return };
    for sid in ids {
        if let Err(e) = delete_session(port, &sid, TEARDOWN_BOUND).await {
            tracing::warn!(session = %sid, error = %format!("{e:#}"), "slo.bench.session.teardown");
        }
    }
}

const NO_MODEL: &str = "no model credential in the probe tenant";
/// Asks for no tool, so the turn is one model reply and nothing runs on the bench.
const PROMPT: &str = "Reply with exactly the word pong. Do not use any tools.";

fn same_events(a: &[String], b: &[String]) -> Result<()> {
    if a.is_empty() {
        bail!("socket A saw no events");
    }
    if let Some(i) = (0..a.len().max(b.len())).find(|&i| a.get(i) != b.get(i)) {
        bail!("the sockets diverge at event {i} of {} vs {}", a.len(), b.len());
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

// `ws.terminal.persists` is RETIRED (spec §2.3, 2026-09-17): the tool server has no PTY any more
// and a terminal is a ttyd socket to the shell sidecar, so nothing survives a restart by design —
// "a dropped connection is a new shell". `shell.up` is what covers a terminal now.

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

    /// A ttyd-shaped server: it answers the subprotocol, expects the auth frame first, expects
    /// input prefixed with `0`, replies with `1` (title) and `2` (prefs) before any output, and
    /// ENDS BY CLOSING — there is no exit frame anywhere in this path. Every one of those cost a
    /// timeout on a healthy shell before 2026-09-18.
    #[tokio::test]
    async fn a_shell_exchange_speaks_ttyds_frames_and_ends_on_the_close() {
        use futures::SinkExt;
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<Mutex<Vec<String>>> = Default::default();
        let heard = seen.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            // `result_large_err`: the error type is tungstenite's own handshake response, fixed by
            // the callback's signature — there is nothing here to box.
            #[allow(clippy::result_large_err)]
            let on_handshake = |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                                mut res: tokio_tungstenite::tungstenite::handshake::server::Response| {
                // ttyd refuses a socket that does not ask for `tty`; the probe must offer it.
                let offered = req.headers().get("sec-websocket-protocol").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
                heard.lock().unwrap().push(format!("subprotocol:{offered}"));
                res.headers_mut().insert("sec-websocket-protocol", "tty".parse().unwrap());
                Ok::<_, tokio_tungstenite::tungstenite::handshake::server::ErrorResponse>(res)
            };
            let mut ws = tokio_tungstenite::accept_hdr_async(stream, on_handshake)
            .await
            .unwrap();
            while let Some(Ok(msg)) = ws.next().await {
                match msg {
                    Message::Text(t) => heard.lock().unwrap().push(format!("auth:{t}")),
                    Message::Binary(b) => {
                        heard.lock().unwrap().push(format!("input:{}", String::from_utf8_lossy(&b)));
                        // Title and preferences first, as ttyd does, then the output, then close.
                        let _ = ws.send(Message::binary(b"1a title".to_vec())).await;
                        let _ = ws.send(Message::binary(b"2{\"prefs\":1}".to_vec())).await;
                        let _ = ws.send(Message::binary(b"0kl-ok\r\n".to_vec())).await;
                        let _ = ws.close(None).await;
                        return;
                    }
                    _ => {}
                }
            }
        });
        let (out, code) = pty_shell(port, "bench", "printf ok\n").await.unwrap();
        // The close is the end, not an exit frame — waiting for one is what held the socket open
        // for the whole ceiling while the shell had already answered.
        assert_eq!(code, 0);
        assert!(out.contains("kl-ok"), "the output frame was not read: {out:?}");
        assert!(!out.contains("a title") && !out.contains("prefs"), "a non-output frame reached the judgement: {out:?}");
        let seen = seen.lock().unwrap().clone();
        assert!(seen.iter().any(|s| s == "subprotocol:tty"), "{seen:?}");
        assert!(seen.iter().any(|s| s.starts_with("auth:") && s.contains("AuthToken") && s.contains("columns")), "{seen:?}");
        assert!(seen.iter().any(|s| s.starts_with("input:0")), "input must carry ttyd's `0` opcode: {seen:?}");
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
    fn events_from_two_sockets_must_match_in_order() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(same_events(&s(&["a", "b", "end"]), &s(&["a", "b", "end"])).is_ok());
        assert!(same_events(&s(&["a", "b", "end"]), &s(&["b", "a", "end"])).unwrap_err().to_string().contains("event 0"));
        assert!(same_events(&s(&["a", "end"]), &s(&["a"])).is_err());
        assert!(same_events(&[], &[]).is_err());
    }

    #[test]
    fn ceilings_are_at_least_their_targets() {
        use kloudlite_workspaces::slo::catalogue::find;
        for (id, cap) in [("bench.shell.roundtrip", SHELL_CEILING), (SHELL_WS, SHELL_WS_CEILING), ("bench.workspace.tool_roundtrip", TOOL_CEILING), ("bench.session.roundtrip", ROUNDTRIP_CEILING), ("bench.start.p95", START_CEILING), ("bench.tunnel", TUNNEL_CEILING), ("bench.idle.wake", WAKE_CEILING), ("bench.delegate", DELEGATE_CEILING)] {
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
        assert!(tool_prompt(m).contains(m));

        // The sign-in answer is an ordinary failed round trip now that the probe mints the token.
        let login = json!({"role": "toolResult", "toolCallId": "t1", "isError": false, "content": [{"type": "text", "text": "sign in on the Kloudlite desktop app"}]});
        assert!(tool_ran(&json!({"messages": [call, login]}).to_string(), m).is_err());
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
