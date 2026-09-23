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
const SESSION_IDS: [&str; 13] = [
    "bench.session.roundtrip",
    "shell.up",
    "shell.fenced",
    "shell.no_tools",
    "bench.no_hands",
    "bench.pkg_needs_workspace",
    "bench.tools.own_hands",
    "bench.proposal.asked",
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
    seed_model_key(c).await;
}

/// Put the probe tenant's model key where its bench reads provider keys from.
///
/// A bench keeps `auth.json` under `PI_CODING_AGENT_DIR` — `~/.bench/pi` since
/// 2026-09-18 — inside its own volume, and NOTHING else writes that file: the desktop's Settings
/// is the only other writer and no probe runs it. Before this, every bench probe that needed a
/// model turn skipped on `NO_MODEL` forever, or passed on a key somebody had put there by hand
/// and lost the moment the volume moved.
///
/// Idempotent, and untimed on purpose: seeding is a precondition of the bench probes, not one of
/// the samples they report. A failure is logged and left to the probes that then skip, because a
/// run that could not seed a key has nothing to say about the model path either way.
///
/// The key travels in the request BODY, never a path segment and never an error string — the same
/// rule the route itself documents.
pub(crate) async fn seed_model_key(c: &mut Ctx) {
    let Some((provider, key)) = c.cfg.model_key.clone() else {
        // Loud, not silent: an unset credential and a working one looked identical in the run
        // report, and every model probe then failed with pi's own "No API key found" while the
        // seeding step said nothing at all (hourly 08:05, 2026-09-18).
        return tracing::warn!("slo.bench.model_key.unset: KLOUDLITE_SLO_MODEL_KEY is not set, so every model turn will be refused");
    };
    let out = async {
        let (_child, port) = forward(c).await?;
        let (status, _) = through_with(port, reqwest::Method::PUT, &format!("/providers/{provider}"), Some(json!({ "apiKey": key }))).await?;
        // 204 is the route's answer; anything else is reported WITHOUT the body, which is the one
        // place a mistyped key could otherwise be echoed back into a log.
        if status != 204 {
            bail!("the bench refused the model key for {provider}: {status}");
        }
        // Read it back: a 204 says the route ran, not that pi will find a key for the model this
        // bench runs. `configured` is a boolean per provider and carries no byte of the key, so
        // the check is safe to make and safe to log.
        let (status, body) = through(port, "/providers").await?;
        if status != 200 {
            bail!("the bench would not list its providers: {status}");
        }
        let rows: Value = serde_json::from_str(&body).context("the providers listing is not JSON")?;
        let configured = rows
            .as_array()
            .map(|rs| rs.iter().any(|r| r["id"] == provider.as_str() && r["configured"] == true))
            .unwrap_or(false);
        if !configured {
            bail!("the bench still reports no key for {provider} after the write");
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    match out {
        // The provider id, never the key: `listProviders` answers `configured` per provider, which
        // is the one read-back that says the seed took without any byte of the key coming back.
        Ok(()) => tracing::info!(%provider, "slo.bench.model_key.seeded"),
        Err(e) => tracing::warn!(%provider, error = %format!("{e:#}"), "slo.bench.model_key.failed"),
    }
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
    // The pod was deleted and recreated by the idle/wake above, so the key is seeded HERE, after
    // the wake and before any session: the bench's `.bench/pi` travels with the volume, but a run
    // whose tenant never had a key would otherwise reach the model turns with none.
    seed_model_key(c).await;
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
    if c.walks("bench.no_hands") {
        no_hands(c).await;
    }
    if c.walks("bench.pkg_needs_workspace") {
        pkg_needs_workspace(c).await;
    }
    if c.walks("bench.tools.own_hands") {
        own_hands(c).await;
    }
    if c.walks("bench.proposal.asked") {
        proposal_asked(c).await;
    }
    if c.walks("agent.tree.run") {
        agent_tree_run(c).await;
    }
    if let Some(login) = login {
        if let Err(e) = super::bench_tool::revoke_login(c, &login).await {
            tracing::warn!(error = %format!("{e:#}"), "slo.bench.tool_token.login.revoke");
        }
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

/// `bench.tools.own_hands`: a bench session's hands are its OWN workspace's, and nobody else's.
///
/// Not "no hands at all" — that was one ruling earlier on 2026-09-17 and it was superseded: a bench
/// session has read/write/edit/bash/grep/find/ls and `process`, all running on its own workspace
/// container's tool server (`127.0.0.1:7788`), with pi's builtins off so nothing can run in the
/// BENCH container. Another workspace is reached only by `ask`, a queue.
///
/// Its own session rather than the round trip's, because this needs no model: the tenant holding no
/// provider key skips every prompted id, and "the model ran with a shell in the wrong place" is
/// exactly the regression that must still be caught on that run. Read from the bench itself — pi's
/// RPC has no tool listing, so `GET /sessions/{id}/tools` answers from the argv and env its child
/// is spawned with.
async fn own_hands(c: &mut Ctx) {
    c.step("bench.tools.own_hands", Duration::from_secs(30), move |c| {
        async move {
            let (_child, port) = forward(c).await?;
            let (status, row) = through_with(port, reqwest::Method::POST, "/sessions", None).await?;
            if status != 201 {
                bail!("POST /sessions answered {status}: {}", super::clip(&row));
            }
            let sid = serde_json::from_str::<Value>(&row)?["id"].as_str().context("session row missing id")?.to_string();
            let (status, body) = through(port, &format!("/sessions/{sid}/tools")).await?;
            let _ = delete_session(port, &sid, Duration::from_secs(10)).await;
            if status != 200 {
                bail!("GET /sessions/{sid}/tools answered {status}: {}", super::clip(&body));
            }
            judge_tools(&body)
        }
        .boxed()
    })
    .await;
}

/// The eight tools that would act on the bench's OWN machine. Since slice 2 a bench session must
/// carry NONE of them: "a bench session has no hands" (spec §3.1) — a workspace change is a
/// message into that workspace's session, never something driven from the bench container, which
/// is nobody's machine. This probe used to demand all eight and so failed on a bench that was
/// working exactly as designed (hourly 08:05, 2026-09-18).
const OWN_HANDS: [&str; 8] = ["read", "write", "edit", "bash", "grep", "find", "ls", "process"];

/// The rest of the always-on set (spec §13/§14): one way to reach a workspace or an agent, and the
/// four tools the session steers itself with. Deliberately NOT any `kl_*`: since 2026-09-17 every
/// platform tool is registered but INACTIVE until `tool_search` turns it on, so demanding one here
/// asserted the old catalogue and failed the id on a bench that was working exactly as designed.
/// `tool_search` itself is what must be there — it is the door to all of them.
const ALWAYS_ON: [&str; 5] = ["ask", "plan", "skill", "tool_search", "memory"];


fn judge_tools(body: &str) -> Result<()> {
    let doc: Value = serde_json::from_str(body)?;
    let tools: Vec<String> = doc["tools"]
        .as_array()
        .context("no `tools` array")?
        .iter()
        .map(|t| t.as_str().unwrap_or_default().to_string())
        .collect();
    // The inversion is the assertion: hands are what a bench must NOT have.
    if let Some(hand) = OWN_HANDS.iter().find(|h| tools.iter().any(|t| &t == h)) {
        bail!("a bench session has hands of its own: `{hand}` in {}", tools.join(", "));
    }
    for want in ALWAYS_ON.iter() {
        if !tools.iter().any(|t| t == want) {
            bail!("a bench session cannot steer itself: no `{want}` in {}", tools.join(", "));
        }
    }
    // A direct tool onto ANOTHER workspace's tool server is the shape the owner ruled out: work
    // there is queued into that workspace's own session, never driven from here.
    if let Some(direct) = tools.iter().find(|t| t.starts_with("kl_ws_")) {
        bail!("a bench session drives another workspace directly through `{direct}`");
    }
    // No tool server AT ALL, which is the other half of having no hands: a bench session that was
    // handed an address could reach a filesystem, whatever its tool list says (spec §3.1).
    if let Some(at) = doc["toolsAddress"].as_str() {
        bail!("a bench session was given a tool server at {at}");
    }
    if doc["builtinTools"] != Value::Bool(false) {
        bail!("pi's builtins are on: they would run in the bench container");
    }
    Ok(())
}

/// `bench.proposal.asked`: a change the person did not agree to does not happen.
///
/// The whole of §9 in one sample: the model is told to create a workspace, the bench holds the
/// QUESTION rather than the call, the probe answers NO, and then the two things that matter — the
/// tool said it was declined, and `/v1` has no such workspace. A gate that asks and then acts
/// anyway is exactly the failure this exists to catch, so the api read is the assertion, not the
/// transcript.
const PROPOSAL_CEILING: Duration = Duration::from_secs(120);
/// `agent.tree.run`: two model turns (the dispatch and the close), the subagent's own turn between
/// them, a tree cut and a tree collected. The catalogue target is 300 s; this is that plus room for
/// the step to say WHY rather than being cut off.
const AGENT_TREE_CEILING: Duration = Duration::from_secs(320);

async fn proposal_asked(c: &mut Ctx) {
    let name = format!("{}-proposal", c.prefix());
    let no_model: Arc<Mutex<Option<String>>> = Default::default();
    let nm = no_model.clone();
    let region = c.cfg.region.clone();
    c.step("bench.proposal.asked", PROPOSAL_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let list = super::api(c, "/v1/workspaces");
        async move {
            let (_child, port) = forward(c).await?;
            let (status, row) = through_with(port, reqwest::Method::POST, "/sessions", None).await?;
            if status != 201 {
                bail!("POST /sessions answered {status}: {}", super::clip(&row));
            }
            let sid = serde_json::from_str::<Value>(&row)?["id"].as_str().context("session row missing id")?.to_string();
            // Through `tool_search`, because every `kl_*` tool is DEFERRED now (spec §13): naming
            // one directly asks for a tool the session has not turned on, and the turn would end
            // with no proposal to answer.
            let prompt = format!(
                "Call tool_search once with query \"create workspace\", then call the tool it names for creating a workspace exactly once with name \"{name}\" and region \"{region}\". Whatever it answers, then reply with exactly the word done."
            );
            // The turn does not end until the question is answered, so the answer is given from
            // here WHILE it runs — the same shape `answering` has, with the opposite answer and
            // one assertion in the middle: the question named this workspace.
            let seen: Arc<Mutex<Option<String>>> = Default::default();
            let asked = seen.clone();
            let want = name.clone();
            let _declining = AbortOnDrop(tokio::spawn(async move {
                loop {
                    if let Ok((200, body)) = through(port, "/proposals").await {
                        if let Ok(rows) = serde_json::from_str::<Vec<Value>>(&body) {
                            for p in rows {
                                let (Some(id), Some(summary)) = (p["id"].as_str(), p["summary"].as_str()) else { continue };
                                if summary.contains(&want) {
                                    *asked.lock().unwrap() = Some(summary.to_string());
                                }
                                let _ = through_with(port, reqwest::Method::POST, &format!("/proposals/{id}"), Some(json!({"answer": "no"}))).await;
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }));
            let turn = one_turn(port, &sid, &prompt, &nm).await;
            let _ = delete_session(port, &sid, Duration::from_secs(10)).await;
            turn?;
            let summary = seen.lock().unwrap().clone();
            let Some(summary) = summary else {
                bail!("no proposal named {name}: a platform write ran without asking, or the model never called the tool");
            };
            if !summary.contains(&name) {
                bail!("the question does not name the workspace: {}", super::clip(&summary));
            }
            // The one that cannot be argued with: the api never heard of it.
            let made = get(c, &list, &jwt).await.context("could not list the workspaces")?;
            if made.as_array().is_some_and(|ws| ws.iter().any(|w| w["name"] == name.as_str())) {
                bail!("{name} was created although the person said no");
            }
            Ok(())
        }
        .boxed()
    })
    .await;
    // Cloned out of the guard before the await, like `exchanges` — a `MutexGuard` held across one
    // is a future the borrow checker will not take.
    let why = no_model.lock().unwrap().clone();
    if let Some(why) = why {
        c.demote_to_skip("bench.proposal.asked", &format!("{NO_MODEL}: {}", super::clip(&why)));
    }
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

/// `agent.tree.run`: the whole subagent lifecycle as a person drives it. Dispatch through the
/// bench's own `ask` tool, the tree exists in `/v1` AND in the workspace that serves it, the
/// agent's ide calls land in the tree and nowhere else, a report comes back, both the tree and the
/// session STAY once it is done, and only `ask_close` takes them.
///
/// "Both stay" is the half worth the probe. Nothing is dropped on completion by design (spec
/// §4.3): the person reads the transcript and the diff after a run that went wrong, and a tree
/// swept on DONE would take the evidence with it. That is a silent regression — the id would still
/// go green on dispatch-and-report — so it is asserted between the report and the close.
///
/// Driven from the bench rather than from `/v1` directly, because `/v1` cannot dispatch an agent:
/// the tool is the bench's, the model calls it, and what this measures is that path end to end.
async fn agent_tree_run(c: &mut Ctx) {
    const ID: &str = "agent.tree.run";
    let ws = match tool_workspace(c.state.ux_workspace.clone(), c.state.ux_ready) {
        Ok(ws) => ws,
        Err(why) => return c.skip(ID, why),
    };
    // The agent works in a tree of a workspace, so the workspace's own tool server has to be
    // serving before any of this means anything — the bench waits on it too (`GET /fs/stat`).
    // A tree name is `[a-z0-9-]{1,32}` (`crd::tree_name_ok`), and a run id is neither bounded to
    // that length nor guaranteed to be in that charset — so it is filtered and cut, not formatted.
    let name = tree_name(&c.prefix());
    let marker = format!("{}-agent", c.prefix());
    let no_model: Arc<Mutex<Option<String>>> = Default::default();
    let (nm, ws_id, tree) = (no_model.clone(), ws.clone(), name.clone());
    c.step(ID, AGENT_TREE_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let doc_url = super::api(c, &format!("/v1/workspaces/{ws_id}"));
        async move {
            let (_child, port) = forward(c).await?;
            let (status, row) = through_with(port, reqwest::Method::POST, "/sessions", None).await?;
            if status != 201 {
                bail!("POST /sessions answered {status}: {}", super::clip(&row));
            }
            let sid = serde_json::from_str::<Value>(&row)?["id"].as_str().context("session row missing id")?.to_string();
            // Every proposal answered yes: the dispatch itself is one, and so is the close.
            let _answering = answering(port);
            // `tool_search` first, like every other bench prompt: the `kl_*` and `ask` tools are
            // DEFERRED, so naming one directly asks for a tool the session has not turned on.
            let brief = format!(
                "Call tool_search once with query \"dispatch an agent\", then call the tool it names \
                 exactly once to start an agent named \"{tree}\" on workspace \"{ws_id}\", with the task: \
                 write a file called {marker}.txt containing the word {marker} in your working directory, \
                 then reply done. Wait for that agent to report, then reply with exactly the word done."
            );
            let turn = one_turn(port, &sid, &brief, &nm).await;
            // The session is the person's window on the agent; it goes at the end whatever
            // happened, but never before the assertions below have read it.
            let outcome = async {
                turn?;
                // 1. `/v1` lists the tree the dispatch cut, ready, with the name the model used.
                //    Read from the doc rather than from the bench, so a bench that invented a
                //    local record and never called `/v1` fails here.
                let doc = get(c, &doc_url, &jwt).await.context("could not read the workspace")?;
                let row = tree_row(&doc, &tree).ok_or_else(|| {
                    anyhow!("no tree named {tree} in the workspace doc: {}", super::clip(&doc.to_string()))
                })?;
                if row["ready"] != Value::Bool(true) {
                    bail!("the tree is not ready: {row}");
                }
                // 2. And the node really cut it: the file the agent was told to write is under
                //    `.agents/{tree}` and NOT in the workspace's own root. One assertion for both
                //    halves of §4.4 — the tree is real, and it is not main. A tree snapshots the
                //    whole home, so its copy of the workspace is `~/.agents/{tree}/workspace`.
                let f = format!("{marker}.txt");
                let home = kloudlite_workspaces::k8s::HOME_DIR;
                let (code, out, _) = super::workspace::ws_exec(
                    c,
                    &ws_id,
                    &format!("cat {home}/.agents/{tree}/workspace/{f} 2>&1; echo ---; cd \"$KL_WORKSPACE\" && ls {f} 2>&1"),
                    Duration::from_secs(20),
                )
                .await?;
                if code != 0 {
                    bail!("could not look inside the tree: exit {code}");
                }
                let (in_tree, in_main) = out.split_once("---").unwrap_or((&out, ""));
                if !in_tree.contains(&marker) {
                    bail!("the agent's file is not in its tree; {f} read {:?}", in_tree.trim());
                }
                if !in_main.contains("No such file") && !in_main.contains("cannot access") {
                    bail!("the agent wrote into the workspace root as well: {:?}", in_main.trim());
                }
                // 3. The report reached the calling session — a direct line, not a queue.
                let (status, body) = through(port, &format!("/sessions/{sid}/messages")).await?;
                if status != 200 {
                    bail!("GET /sessions/{sid}/messages answered {status}");
                }
                answered(&body, &nm)?;
                // 4. Nothing is dropped on completion: the tree is STILL there after the report.
                let doc = get(c, &doc_url, &jwt).await.context("could not re-read the workspace")?;
                if tree_row(&doc, &tree).is_none() {
                    bail!("the tree was swept when the agent finished; it must stay until it is closed");
                }
                // 5. And the close is what takes it — through the same tool, so the proposal the
                //    person answers is the one that deletes it.
                let close = format!(
                    "Call tool_search once with query \"close an agent\", then call the tool it names \
                     exactly once to close the agent named \"{tree}\". Then reply with exactly the word done."
                );
                one_turn(port, &sid, &close, &nm).await?;
                let gone = poll_until(Duration::from_secs(60), || async {
                    get(c, &doc_url, &jwt).await.map(|d| tree_row(&d, &tree).is_none()).unwrap_or(false)
                })
                .await;
                if !gone {
                    bail!("the tree survived the close");
                }
                Ok(())
            }
            .await;
            let _ = delete_session(port, &sid, TEARDOWN_BOUND).await;
            outcome
        }
        .boxed()
    })
    .await;
    // Bound out of the guard before the `if let`: holding a `MutexGuard` across the branch keeps
    // the borrow alive past `no_model`'s own scope.
    let why = no_model.lock().unwrap().clone();
    if let Some(why) = why {
        c.demote_to_skip(ID, &format!("{NO_MODEL}: {}", super::clip(&why)));
    }
}

/// A run's prefix as a legal tree name: lowercase, `[a-z0-9-]` only, at most 32. The tail rather
/// than the head, because a run id's entropy is at its end and two runs must not collide on one
/// workspace's `.agents/`.
fn tree_name(prefix: &str) -> String {
    let kept: String = prefix
        .to_ascii_lowercase()
        .chars()
        .filter(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || *ch == '-')
        .collect();
    let tail: String = kept.chars().rev().take(28).collect::<Vec<_>>().into_iter().rev().collect();
    // Never leading `-` and never empty: both are names `/v1` refuses with a 422, which would
    // report the probe's own bug as a platform failure.
    format!("a-{}", tail.trim_start_matches('-'))
}

/// One tree row of a workspace doc, by name. `status.trees` as `/v1` serves it — the list is short
/// (eight at most) so a scan is the whole lookup.
fn tree_row<'a>(doc: &'a Value, name: &str) -> Option<&'a Value> {
    doc["trees"].as_array()?.iter().find(|t| t["name"] == name)
}

/// Poll a condition to a deadline. The close is a proposal, then a `/v1` DELETE, then the agent's
/// own pass — three hops, so the row goes some seconds after the turn says it is done.
async fn poll_until<F, Fut>(bound: Duration, mut f: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + bound;
    while Instant::now() < deadline {
        if f().await {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    false
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

/// `bench.no_hands`: a bench session cannot run a command, and says so rather than trying.
///
/// The transcript is the assertion (spec §3.4): asked to `cat /etc/hostname` the session must call
/// no tool that acts — there is no `bash`, no `read`, no filesystem tool registered in the sessions
/// container in any mode. The `ALWAYS_ON` tools only steer the session, so calling them (a
/// `tool_search` for a shell, say) is allowed; any other tool result fails the id.
async fn no_hands(c: &mut Ctx) {
    let no_model: Arc<Mutex<Option<String>>> = Default::default();
    let nm = no_model.clone();
    c.step("bench.no_hands", ROUNDTRIP_CEILING, move |c| {
        async move {
            let (_child, port) = forward(c).await?;
            let (status, row) = through_with(port, reqwest::Method::POST, "/sessions", None).await?;
            if status != 201 {
                bail!("POST /sessions answered {status}: {}", super::clip(&row));
            }
            let sid = serde_json::from_str::<Value>(&row)?["id"].as_str().context("session row missing id")?.to_string();
            let turn = one_turn(port, &sid, "Run: cat /etc/hostname", &nm).await;
            let (_, body) = through(port, &format!("/sessions/{sid}/messages")).await?;
            let _ = delete_session(port, &sid, Duration::from_secs(10)).await;
            turn?;
            // Not "it answered something sensible" — that is a model's business. What this id holds
            // is that nothing RAN: a tool call in the transcript is the boundary being crossed.
            let doc: Value = serde_json::from_str(&body)?;
            let msgs = doc["messages"].as_array().context("messages answer has no messages")?;
            // `ALWAYS_ON` steers the session and touches nothing, and a model looking for a way to
            // run the command reaches for `tool_search` first (hourly 2026-09-23 19:53 IST): that
            // is the session asking, not crossing. Any other tool, or one with no name, is.
            if let Some(call) = msgs.iter().find(|m| crossed(m)) {
                bail!("a bench session ran a tool: {}", super::clip(&call.to_string()));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
    let why = no_model.lock().unwrap().clone();
    if let Some(why) = why {
        c.demote_to_skip("bench.no_hands", &format!("{NO_MODEL}: {}", super::clip(&why)));
    }
}

/// `bench.pkg_needs_workspace`: there is no "on the bench" to install onto.
///
/// A package request always names a workspace and becomes a proposal on THAT workspace's spec
/// (spec §3.1). Asked to install with no workspace named, the tool refuses with the sentence and
/// nothing is proposed — so the assertion is the empty proposal list, not the model's prose.
async fn pkg_needs_workspace(c: &mut Ctx) {
    let no_model: Arc<Mutex<Option<String>>> = Default::default();
    let nm = no_model.clone();
    c.step("bench.pkg_needs_workspace", ROUNDTRIP_CEILING, move |c| {
        async move {
            let (_child, port) = forward(c).await?;
            let (status, row) = through_with(port, reqwest::Method::POST, "/sessions", None).await?;
            if status != 201 {
                bail!("POST /sessions answered {status}: {}", super::clip(&row));
            }
            let sid = serde_json::from_str::<Value>(&row)?["id"].as_str().context("session row missing id")?.to_string();
            let turn = one_turn(port, &sid, "Install jq.", &nm).await;
            let (_, open) = through(port, "/proposals").await?;
            let _ = delete_session(port, &sid, Duration::from_secs(10)).await;
            turn?;
            // A package install that reached a PROPOSAL means the tool accepted a request with no
            // workspace in it — the refusal is meant to happen before anyone is asked anything.
            if !proposal_ids(&open).is_empty() {
                bail!("a package install with no workspace was proposed: {}", super::clip(&open));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
    let why = no_model.lock().unwrap().clone();
    if let Some(why) = why {
        c.demote_to_skip("bench.pkg_needs_workspace", &format!("{NO_MODEL}: {}", super::clip(&why)));
    }
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
fn answering(port: u16) -> AbortOnDrop<()> {
    AbortOnDrop(tokio::spawn(async move {
        loop {
            if let Ok((200, body)) = through(port, "/proposals").await {
                for id in proposal_ids(&body) {
                    let _ = through_with(port, reqwest::Method::POST, &format!("/proposals/{id}"), Some(json!({"answer": "yes"}))).await;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }))
}

/// The ids in a `GET /proposals` body. A body that is not the listing names nothing — never an
/// error: this runs beside a turn whose own failure is the sample.
fn proposal_ids(body: &str) -> Vec<String> {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|p| p["id"].as_str().map(str::to_string))
        .collect()
}

/// `bench.exchange.both_views` has no bound (availability only), so this caps one tool call plus a
/// one-word reply: the round trip's own 60 s target, doubled for the extra model step.
const EXCHANGE_CEILING: Duration = Duration::from_secs(120);
/// `bench.workspace.tool_roundtrip`: target 180 s.
const TOOL_CEILING: Duration = Duration::from_secs(180);

/// `ask` is what writes an exchange now (`Bench.ask` records one before it queues the task), so the
/// prompt calls that — and the task is a greeting, which changes nothing wherever it lands and
/// leaves no teardown owed.
fn exchange_prompt(target: &str) -> String {
    // `ask` (spec §13) replaced `kl_workspace_ask`/`kl_agent`, and it is what WRITES the exchange
    // this id reads back. A prompt naming a `kl_*` tool would name a DEFERRED one now — inactive
    // until `tool_search` turns it on — so the turn would end without the row.
    format!("Call the tool ask exactly once with to \"{target}\" and task \"say hello\". Whatever it answers, then reply with exactly the word done.")
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
    // The run's own workspace when there is one: `ask` OPENS the target's session before it
    // records the exchange (`Bench.ask`), so a name nothing resolves to may throw before there is
    // a row to read back. The synthetic name stays as the fallback — a stage that never made a
    // workspace still files a sample rather than a skip.
    let target = c.state.ux_workspace.clone().unwrap_or_else(|| format!("{}-exchange", c.prefix()));
    let no_model: Arc<Mutex<Option<String>>> = Default::default();
    let nm = no_model.clone();
    c.step("bench.exchange.both_views", EXCHANGE_CEILING, move |c| {
        async move {
            let (_child, port) = forward(c).await?;
            // `ask` is a gated write, and its tool call does not return until somebody answers
            // the question — so the answer has to come from here, while the turn is still running.
            let _answering = answering(port);
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
            // A workspace session's `kl_env_*`/`kl_environment_*` are gated the same way; the
            // prompt below asks for `bash` only, but a model that reaches for one of those would
            // otherwise hang this turn to the ten-minute cap rather than failing it.
            let _answering = answering(port);
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
/// A message that says a tool outside `ALWAYS_ON` ran — `bench.no_hands`'s boundary.
fn crossed(m: &Value) -> bool {
    (m["role"] == "toolResult" || m["toolCallId"].is_string())
        && !m["toolName"].as_str().is_some_and(|n| ALWAYS_ON.contains(&n))
}

/// The ids of this group that need a live workspace besides the bench.
const NEEDS_WORKSPACE: [&str; 3] = ["shell.up", "shell.no_tools", "agent.tree.run"];

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

// `ws.terminal.persists` is RETIRED (spec §2.3, 2026-09-17): the tool server has no PTY any more
// and a terminal is a ttyd socket to the shell sidecar, so nothing survives a restart by design —
// "a dropped connection is a new shell". `shell.up` is what covers a terminal now.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::run_state;
    use kloudlite_workspaces::history::slo::RunState;

    /// The name the dispatch asks `/v1` for has to pass `crd::tree_name_ok`, or the probe reports
    /// its own 422 as a platform failure. Checked against that predicate itself, not against a
    /// copy of the rule.
    #[test]
    fn a_runs_tree_name_is_one_v1_accepts() {
        for prefix in ["run-abc123", "run-ABC-123", "run-", "run-0123456789012345678901234567890123456789", "x"] {
            let n = tree_name(prefix);
            assert!(kloudlite_workspaces::crd::tree_name_ok(&n), "{prefix:?} became {n:?}");
        }
        // The tail is kept, so two runs that share a prefix still differ.
        assert_ne!(tree_name("run-aaaa1111"), tree_name("run-aaaa2222"));
    }

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
        assert!(!crossed(&serde_json::json!({"role": "toolResult", "toolName": "tool_search"})));
        assert!(crossed(&serde_json::json!({"role": "toolResult", "toolName": "kl_exec"})));
        assert!(crossed(&serde_json::json!({"role": "toolResult"})));
        assert!(!crossed(&serde_json::json!({"role": "assistant"})));
        assert!(tool_workspace(None, true).is_err());
        assert!(tool_workspace(Some("w".into()), false).unwrap_err().contains("ws.packages.add"));
        assert_eq!(tool_workspace(Some("w".into()), true).unwrap(), "w");
        // `ask`, never a `kl_*` name: those are deferred until `tool_search` turns them on.
        let ex = exchange_prompt("run-abc-exchange");
        assert!(tool_prompt(m).contains(m) && ex.contains("tool ask") && ex.contains("run-abc-exchange"));
        assert!(!ex.contains("kl_"), "{ex}");

        // The sign-in answer is an ordinary failed round trip now that the probe mints the token.
        let login = json!({"role": "toolResult", "toolCallId": "t1", "isError": false, "content": [{"type": "text", "text": "sign in on the Kloudlite desktop app"}]});
        assert!(tool_ran(&json!({"messages": [call, login]}).to_string(), m).is_err());

        let no_model = Mutex::new(None);
        let nokey = json!({"messages": [{"role": "user", "content": "x"}, {"role": "assistant", "content": [], "stopReason": "error", "errorMessage": "No API key found for deepseek"}]});
        assert!(answered(&nokey.to_string(), &no_model).is_err());
        assert!(no_model.lock().unwrap().is_some(), "a missing key must be recorded for the demote");
    }


    /// `GET /proposals` is a listing of ids; anything else names nothing, and never fails the turn
    /// it runs beside.
    #[test]
    fn proposal_ids_are_read_from_the_listing_and_nothing_else() {
        let body = json!([
            {"id": "p-1", "session": "s-1", "tool": "kl_workspace_create", "summary": "Create workspace x"},
            {"id": "p-2", "session": "s-1", "tool": "kl_workspace_stop", "summary": "Stop workspace y"},
        ])
        .to_string();
        assert_eq!(proposal_ids(&body), ["p-1", "p-2"]);
        assert!(proposal_ids("[]").is_empty());
        assert!(proposal_ids("bench unreachable").is_empty());
        assert!(proposal_ids(&json!({"error": "no"}).to_string()).is_empty());
    }

    /// The ruling this id exists for, both halves: the eight own-machine tools and the always-on
    /// five must be there, and they must run on the bench's OWN workspace tool server with pi's
    /// builtins off. A list that satisfies the names while running in the bench container is the
    /// regression. No `kl_*` is required: every one of them is deferred until `tool_search`.
    #[test]
    fn own_hands_refuses_a_bench_session_that_has_any() {
        // What a correct bench session answers since slice 2: no hands, no tool server, no
        // builtins — only the tools it steers ITSELF with.
        let ok = json!({
            "tools": ["ask", "ask_close", "plan", "skill", "tool_search", "memory", "question"],
            "builtinTools": false,
        });
        assert!(judge_tools(&ok.to_string()).is_ok(), "a hands-free bench session must pass");

        // Each of the eight is a failure ON ITS OWN: one is enough to run something in the bench
        // container, which is nobody's machine.
        for hand in OWN_HANDS {
            let mut v = ok.clone();
            let mut tools = ok["tools"].as_array().unwrap().clone();
            tools.push(json!(hand));
            v["tools"] = json!(tools);
            assert!(judge_tools(&v.to_string()).is_err(), "`{hand}` on a bench session passed");
        }

        // The steering set is still required: without `tool_search` no platform tool can be
        // reached at all, and without `ask` there is no way to reach a workspace.
        let without = |name: &str| {
            let mut v = ok.clone();
            v["tools"] = json!(ok["tools"].as_array().unwrap().iter().filter(|t| *t != name).collect::<Vec<_>>());
            judge_tools(&v.to_string())
        };
        assert!(without("ask").is_err());
        assert!(without("tool_search").is_err(), "without it no platform tool can be reached at all");
        assert!(without("memory").is_err());
        assert!(without("plan").is_err());

        // An ADDRESS is hands by another name, whatever the tool list says.
        let mut addressed = ok.clone();
        addressed["toolsAddress"] = json!("127.0.0.1:7788");
        assert!(judge_tools(&addressed.to_string()).is_err(), "a bench session with a tool server passed");

        let mut builtins = ok.clone();
        builtins["builtinTools"] = json!(true);
        assert!(judge_tools(&builtins.to_string()).is_err(), "pi's builtins in the bench container passed");

        let mut driving = ok.clone();
        let mut tools = ok["tools"].as_array().unwrap().clone();
        tools.push(json!("kl_ws_exec"));
        driving["tools"] = json!(tools);
        assert!(judge_tools(&driving.to_string()).is_err(), "a direct tool onto another workspace passed");
    }

    #[tokio::test]
    async fn a_stub_bench_and_the_reschedule_drill_reach_the_run_row_as_skipped() {
        let mut c = crate::testkit::ctx().await;
        c.step("bench.idle.wake", Duration::from_secs(1), |_| async { Ok(()) }.boxed()).await;
        c.demote_to_skip("bench.idle.wake", STUB);
        SESSION_IDS.iter().for_each(|id| c.skip(id, STUB));
        weekly(&mut c).await;
        // The weekly skip plus every `SESSION_IDS` this fixture files — five more since the shell
        // sidecar arrived (`shell.*`, `bench.no_hands`, `bench.pkg_needs_workspace`) and one fewer
        // for the retired `ws.terminal.persists`.
        assert_eq!(c.steps.len(), 1 + SESSION_IDS.len() + 1);
        assert!(c.steps.iter().all(|s| s.skipped && !s.ok), "a skip read as a sample");
        assert_eq!(c.failed(), 0);
        assert_eq!(run_state(true, false, &c.steps), RunState::Skipped);
    }
}
