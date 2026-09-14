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
    let resp = reqwest::Client::new().get(format!("http://127.0.0.1:{port}{path}")).header("host", "bench").send().await?;
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
    // ponytail: the four journeys need a harness-bench RPC WebSocket client the probe does not
    // carry yet; skipped (never passed) until the real image ships and the client lands with it.
    SESSION_IDS.iter().for_each(|id| c.skip(id, "the probe has no harness-bench RPC client yet"));
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
    fn ceilings_are_at_least_their_targets() {
        use kloudlite_workspaces::slo::catalogue::find;
        for (id, cap) in [("bench.start.p95", START_CEILING), ("bench.tunnel", TUNNEL_CEILING), ("bench.idle.wake", WAKE_CEILING)] {
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
