//! Stage 6 · Environment: one environment with one service, and the attachment that makes it
//! reachable from a workspace by bare name.
//!
//! Worst case 420 s if every step times out (120 + 20 + 20 + 20 + 90 + 30 + 120); see
//! `workspace.rs`'s note
//! on how the three stages' sums sit against the fast suite's 900 s deadline.
//!
//! `env.dns`, `env.attach` and `env.detach` are all resolver questions asked from INSIDE a pod,
//! because that is the only place the answer means anything: CoreDNS answering the api process
//! says nothing about what a service in the environment's namespace can reach.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;
use serde_json::Value;

use kloudlite_workspaces::slo::catalogue::Suite;

use super::{api, call, get, poll_json, post, raw};
use crate::ctx::Ctx;

const CREATE_CEILING: Duration = Duration::from_secs(120);
const DNS_CEILING: Duration = Duration::from_secs(20);
const ATTACH_CEILING: Duration = Duration::from_secs(20);
// The catalogue allows 90 s for an environment push; the ceiling may never be under its own
// target, or a breach and a cut-off step become the same sample.
const PUSH_CEILING: Duration = Duration::from_secs(90);
/// The clone's own ceiling — the catalogue's 120 s for `env.clone.p95`, which is twice a
/// workspace clone's because an environment copies live bytes and then waits for every service.
const CLONE_CEILING: Duration = Duration::from_secs(120);
/// `env.exec.ok` is one command in one pod, exactly like `ws.exec.ok`.
const SVC_EXEC_CEILING: Duration = Duration::from_secs(30);

/// One lookup inside a pod. The loop below is what waits out an attachment taking effect; a single
/// exec that needs more than this is a wedged API server, not a slow resolver.
const EXEC_CEILING: Duration = Duration::from_secs(10);

/// The one service. `redis:7-alpine` because it is small, starts in a second and answers on a port
/// — the journey needs a name to resolve, not a database to use.
const SERVICE: &str = "redis";
const IMAGE: &str = "redis:7-alpine";
const PORT: u16 = 6379;

const QUOTA_GB: u64 = 1;

/// Every id after the create, in journey order.
const AFTER_CREATE: [&str; 6] =
    ["env.exec.ok", "env.dns", "env.attach", "env.detach", "env.push.p95", "env.clone.p95"];

pub async fn run(c: &mut Ctx) {
    fast(c).await;
    // Its OWN environment and workspace, so it neither depends on the fast journey's having
    // worked nor leaves the one stage 7 stops and starts in a state stage 7 did not ask for.
    intercepts(c).await;
}

async fn fast(c: &mut Ctx) {
    if !create(c).await {
        for id in AFTER_CREATE {
            c.skip(id, "the environment never became ready");
        }
        return;
    }
    let Some(env) = c.state.environment.clone() else {
        for id in AFTER_CREATE {
            c.skip(id, "the create answered no environment id");
        }
        return;
    };
    exec_ok(c, &env).await;
    dns(c, &env).await;
    attach(c, &env).await;
    push(c, &env).await;
    clone(c, &env).await;
}

/// `env.exec.ok`: a command inside a running service pod — `ws.exec.ok`'s twin, and the smallest
/// thing that says the environment is a place code runs rather than an object reporting `running`.
async fn exec_ok(c: &mut Ctx, env: &str) {
    if c.kube.is_none() {
        return c.skip("env.exec.ok", "no kubeconfig");
    }
    let env = env.to_string();
    c.step("env.exec.ok", SVC_EXEC_CEILING, move |c| {
        async move {
            let k = c.kube.as_ref().ok_or_else(|| anyhow!("no kubeconfig"))?;
            let ns = kloudlite_workspaces::crd::env_namespace(&env);
            let pod = format!("{SERVICE}-0");
            let (code, out, err) =
                crate::kube::exec(k, &ns, &pod, None, &["sh", "-c", "echo slo"], SVC_EXEC_CEILING).await?;
            if code != 0 || out.trim() != "slo" {
                return Err(anyhow!("exec exited {code}: {}", err.trim()));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `env.clone.p95`: `ws.clone.p95`'s twin on a RUNNING source — no stop first, because an
/// environment clone copies the source's live subvolume and that is the shape a person clicks.
/// (`env.clone`, hourly, is the stopped-source variant.)
///
/// The copy's own id goes into `extra_volumes`: a fresh environment's Volume is named after the
/// environment, and teardown's prefix sweep sees the environment but not the volume behind it.
async fn clone(c: &mut Ctx, env: &str) {
    let name = format!("{}-envclone", c.prefix());
    let env = env.to_string();
    c.step("env.clone.p95", CLONE_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/environments/{env}/clone"));
        let body = serde_json::json!({ "name": name });
        async move {
            let doc = post(c, &url, &jwt, body).await.context("could not clone the environment")?;
            let id = doc
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("the clone answered no environment id"))?
                .to_string();
            // Only the volume is recorded: `env_clone` is stage 14's field, and the environment
            // itself carries the run prefix, which teardown's sweep already finds.
            c.state.extra_volumes.push(id.clone());
            let read = api(c, &format!("/v1/environments/{id}"));
            poll_json(c, &read, &jwt, CLONE_CEILING, |v| {
                v.get("state").and_then(Value::as_str) == Some("running")
            })
            .await
            .context("the clone never became running")?;
            // Same reason the create waits on the StatefulSet: `running` is the record, and the
            // SLI says "with its services ready".
            service_ready(c, &id, CLONE_CEILING).await
        }
        .boxed()
    })
    .await;
}

/// Wait until `SERVICE`'s StatefulSet in this environment reports a ready replica. Without a
/// kubeconfig there is nothing to read, and the caller has already measured the record.
pub(super) async fn service_ready(c: &Ctx, env: &str, cap: Duration) -> Result<()> {
    sts_ready(c, env, SERVICE, cap).await
}

/// The same for one named service — the intercept journey's environment has two.
async fn sts_ready(c: &Ctx, env: &str, svc: &str, cap: Duration) -> Result<()> {
    let Some(k) = c.kube.as_ref() else { return Ok(()) };
    let ns = kloudlite_workspaces::crd::env_namespace(env);
    let sts: kube::Api<k8s_openapi::api::apps::v1::StatefulSet> = kube::Api::namespaced(k.clone(), &ns);
    let start = std::time::Instant::now();
    loop {
        let ready = sts.get(svc).await.ok().and_then(|s| s.status).and_then(|st| st.ready_replicas).unwrap_or(0);
        if ready >= 1 {
            return Ok(());
        }
        if start.elapsed() >= cap {
            return Err(anyhow!("{svc}'s pod never became ready"));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The journey's environment shape — one `redis` service — created under `name` and waited for
/// until it is `running` with that service's pod ready. `env.create.p95` measures this; the
/// weekly's `env.cross.node` stands up one of its own with it, since stage 8 has deleted the
/// journey's by then.
pub(super) async fn create_running(c: &mut Ctx, name: &str) -> Result<String> {
    let body = serde_json::json!({
        "name": name,
        "region": c.cfg.region,
        "quota_gb": QUOTA_GB,
        "services": [{
            "name": SERVICE,
            "image": IMAGE,
            "command": [],
            "env": {},
            "mounts": [],
            // The ClusterIP Service `env.dns` resolves exists only because a port is declared
            // (`k8s.rs`: a service with no ports gets no ClusterIP at all).
            "ports": [PORT],
        }],
    });
    let jwt = c.probe_jwt.clone();
    let url = api(c, "/v1/environments");
    let doc = post(c, &url, &jwt, body).await.context("could not create the environment")?;
    let id = doc
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("the answer carried no environment id"))?
        .to_string();
    let env = api(c, &format!("/v1/environments/{id}"));
    poll_json(c, &env, &jwt, CREATE_CEILING, |v| v.get("state").and_then(Value::as_str) == Some("running")).await?;
    // `running` is the environment's own word; the service pod behind it is what `env.dns`
    // execs into, so the create is not done until that StatefulSet reports a ready replica.
    service_ready(c, &id, CREATE_CEILING).await?;
    Ok(id)
}

/// `env.create.p95`: the create and the wait for `ready` — same reason `ws.create.p95` waits.
async fn create(c: &mut Ctx) -> bool {
    let name = format!("{}-env", c.prefix());
    c.step("env.create.p95", CREATE_CEILING, move |c| {
        async move {
            let id = create_running(c, &name).await?;
            c.state.environment = Some(id);
            Ok(())
        }
        .boxed()
    })
    .await
}

/// `env.dns`: a sibling resolves the service by bare name inside the environment's namespace.
///
/// Asked from the service's own pod, which is the only pod guaranteed to exist in that namespace,
/// and it is a sibling of itself for resolver purposes — the record it looks up is the ClusterIP
/// Service, not its own address.
async fn dns(c: &mut Ctx, env: &str) {
    if c.kube.is_none() {
        return c.skip("env.dns", "no kubeconfig");
    }
    let env = env.to_string();
    c.step("env.dns", DNS_CEILING, move |c| {
        async move {
            // Polled to the ceiling, never asked once: the service pod is Running a beat before
            // redis accepts, and one early `ping` read as "DNS is broken" failed half the runs.
            let started = std::time::Instant::now();
            loop {
                let cap = DNS_CEILING.saturating_sub(started.elapsed());
                if resolves(c, &env, cap.max(Duration::from_secs(2))).await.unwrap_or(false) {
                    return Ok(());
                }
                if started.elapsed() + Duration::from_secs(2) >= DNS_CEILING {
                    return Err(anyhow!("`{SERVICE}` does not resolve and answer inside the environment"));
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
        .boxed()
    })
    .await;
}

/// Whether `redis` resolves from the environment's own service pod AND answers on its port.
///
/// The connection is the half that makes this an SLI about service-to-service traffic rather than
/// about CoreDNS: a name that resolves to a ClusterIP nothing routes to renders as "my services
/// cannot reach each other", and a resolver-only check passes straight through it. `redis-cli
/// ping` is the smallest round trip that says the ClusterIP is live, and it is in the image the
/// environment already runs.
///
/// `getent` first, `nslookup` as the fallback: alpine has both, from musl and from busybox, and
/// which one a base image ships has changed under us before.
async fn resolves(c: &Ctx, env: &str, cap: Duration) -> Result<bool> {
    let k = c.kube.as_ref().ok_or_else(|| anyhow!("no kubeconfig"))?;
    let ns = kloudlite_workspaces::crd::env_namespace(env);
    // `{service}-0`: one StatefulSet per service, one replica, so the ordinal is always zero.
    let pod = format!("{SERVICE}-0");
    let script = format!(
        "(getent hosts {SERVICE} || nslookup {SERVICE}) >/dev/null && redis-cli -h {SERVICE} -p {PORT} ping"
    );
    let (code, out, _) = crate::kube::exec(k, &ns, &pod, None, &["sh", "-c", &script], cap).await?;
    // PONG, not merely exit 0: `redis-cli` answers zero for a connection it never made on some
    // builds, and the word is what says the ClusterIP carried traffic.
    Ok(code == 0 && out.trim().eq_ignore_ascii_case("pong"))
}

/// `env.attach` and `env.detach`: the attachment takes effect, and stops having effect, INSIDE the
/// workspace pod — a `/etc/resolv.conf` the agent renders in place, which is what makes both work
/// without restarting the pod.
///
/// One function for both because they are one experiment: detaching proves nothing unless the same
/// lookup resolved a moment earlier, and attaching proves nothing that a permanently-open resolver
/// would not also pass.
async fn attach(c: &mut Ctx, env: &str) {
    let (Some(ws), true) = (c.state.workspace.clone(), c.kube.is_some()) else {
        let why = if c.kube.is_none() { "no kubeconfig" } else { "no workspace" };
        c.skip("env.attach", why);
        c.skip("env.detach", why);
        return;
    };
    let (e, w) = (env.to_string(), ws.clone());
    let attached = c
        .step("env.attach", ATTACH_CEILING, move |c| {
            let jwt = c.probe_jwt.clone();
            let url = api(c, &format!("/v1/workspaces/{w}/attach"));
            let body = serde_json::json!({ "environment": e });
            async move {
                post(c, &url, &jwt, body).await.context("could not attach")?;
                until(c, &w, true, ATTACH_CEILING).await
            }
            .boxed()
        })
        .await;
    if !attached {
        // The failure was counted where it happened: a detach that was never an attach measures
        // nothing about detaching.
        return c.skip("env.detach", "the workspace was never attached");
    }
    c.step("env.detach", ATTACH_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/workspaces/{ws}/detach"));
        async move {
            post(c, &url, &jwt, Value::Null).await.context("could not detach")?;
            until(c, &ws, false, ATTACH_CEILING).await
        }
        .boxed()
    })
    .await;
}

/// Poll the workspace pod's own resolver until `want` matches what it can see.
async fn until(c: &Ctx, ws: &str, want: bool, cap: Duration) -> Result<()> {
    let start = std::time::Instant::now();
    loop {
        // A failed exec is not "does not resolve": the pod may be mid-restart, and reading that as
        // a detach having taken effect would pass this SLO through a broken workspace.
        let script = format!("getent hosts {SERVICE} || nslookup {SERVICE}");
        let (code, _, _) = super::workspace::ws_exec(c, ws, &script, EXEC_CEILING).await?;
        if (code == 0) == want {
            return Ok(());
        }
        if start.elapsed() >= cap {
            let what = if want { "never resolved" } else { "still resolves" };
            return Err(anyhow!("`{SERVICE}` {what} in the workspace after {} ms", cap.as_millis()));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// `env.push.p95`: the environment's own snapshot, waited to `ready` like the workspace's.
async fn push(c: &mut Ctx, env: &str) {
    // An environment's Volume carries its own id, exactly as a workspace's does.
    c.state.env_volume = Some(env.to_string());
    let env = env.to_string();
    c.step("env.push.p95", PUSH_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/environments/{env}/push"));
        let history = api(c, &format!("/v1/volumes/{env}/history"));
        async move {
            let doc = post(c, &url, &jwt, serde_json::json!({})).await.context("could not push")?;
            let snap = doc
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("the push answered no snapshot id"))?
                .to_string();
            // Both, so teardown can delete them by name: the environment's Volume outlives the
            // environment for as long as this snapshot references it.
            c.state.env_snapshot = Some(snap.clone());
            poll_json(c, &history, &jwt, PUSH_CEILING, |v| super::workspace::row_ready(v, &snap))
                .await
                .context("the snapshot never turned ready")
        }
        .boxed()
    })
    .await;
}

// ── intercepts (hourly) ───────────────────────────────────────────────────────

/// The three ids `intercepts` owns. Hourly, and a fast run walks none of them — the same shape
/// stage 2 uses for `git.branch.delete`, and for the same reason: this journey stands up a second
/// environment and a second workspace, which is too much to pay every five minutes.
const INTERCEPT_IDS: [&str; 3] =
    ["env.intercept", "env.intercept.fallback", "env.intercept.refused"];

/// The service the workspace takes over. A SECOND service, not `SERVICE`: an intercept scales the
/// real StatefulSet to 0, so intercepting the only service would leave the namespace with no pod
/// left to dial from — and the dial has to come from inside the environment, because that is
/// whose traffic an intercept redirects.
const TARGET: &str = "echo";
/// `redis-server --port 8080`, so `TARGET`'s OWN answer is `PONG` and nothing else in the
/// environment ever says `MARKER`. One image for both services keeps the pull warm.
const TARGET_PORT: u16 = 8080;
/// Where the workspace listens — deliberately NOT `TARGET_PORT`. The remap is the ordinary case,
/// not an edge one: the process a person is debugging listens where their dev server listens.
const WS_PORT: u16 = 3000;
/// What the workspace's listener answers with.
const MARKER: &str = "slo-intercept";

/// Started with `nohup … &` so it outlives the exec that starts it, then dialled from inside the
/// pod until it answers: intercepting a port nothing listens on would measure the intercept as
/// broken when the listener is what never came up.
///
/// `bun` twice rather than curl or `/dev/tcp`: it is the one runtime the workspace image is
/// guaranteed to have, and `sh` here is not necessarily bash.
const LISTENER: &str = r#"nohup bun -e 'Bun.serve({ port: 3000, hostname: "0.0.0.0", fetch: () => new Response("slo-intercept") })' > /tmp/slo-intercept.log 2>&1 &
i=0
while [ $i -lt 20 ]; do
  if bun -e 'const r = await fetch("http://127.0.0.1:3000"); process.exit((await r.text()) === "slo-intercept" ? 0 : 1)' > /dev/null 2>&1; then
    echo listening
    exit 0
  fi
  i=$((i + 1))
  sleep 1
done
cat /tmp/slo-intercept.log
exit 1"#;

/// The catalogue's 120 s for `env.intercept`: the POST, the StatefulSet down to 0, the Service
/// losing its selector and kube-proxy programming the slice.
const INTERCEPT_CEILING: Duration = Duration::from_secs(120);
/// 180 s for the fallback, which is the same path in reverse plus `INTERCEPT_GRACE_SECS` and a
/// cold start of the real service.
const FALLBACK_CEILING: Duration = Duration::from_secs(180);
/// Two refusals and nothing else — no convergence to wait for.
const REFUSED_CEILING: Duration = Duration::from_secs(30);
/// A workspace create with its pod pulled and running, matching `experience_ws`'s own wait.
const WS_CEILING: Duration = Duration::from_secs(300);

/// One dial from the environment's own `SERVICE` pod, answering whatever came back on stdout.
///
/// `env.dns`'s vantage point exactly — a sibling service inside the environment's namespace,
/// which is the only place the answer means anything.
async fn dial(c: &Ctx, env: &str, script: &str) -> Result<String> {
    let k = c.kube.as_ref().ok_or_else(|| anyhow!("no kubeconfig"))?;
    let ns = kloudlite_workspaces::crd::env_namespace(env);
    let pod = format!("{SERVICE}-0");
    let (_, out, _) = crate::kube::exec(k, &ns, &pod, None, &["sh", "-c", script], EXEC_CEILING).await?;
    Ok(out)
}

/// An HTTP GET at `TARGET`'s own name and port — the address callers dial, unchanged by the
/// intercept. Only the workspace answers this with `MARKER`; the real redis answers an error.
fn workspace_dial() -> String {
    format!(r"printf 'GET / HTTP/1.0\r\n\r\n' | nc -w 3 {TARGET} {TARGET_PORT}")
}

/// `TARGET`'s own word, which only the real service can say.
fn service_dial() -> String {
    format!("redis-cli -h {TARGET} -p {TARGET_PORT} ping")
}

/// Poll `script` from inside the environment until its answer contains `want`.
async fn answers(c: &Ctx, env: &str, script: &str, want: &str, cap: Duration) -> Result<()> {
    let start = std::time::Instant::now();
    let mut last = String::new();
    loop {
        last = dial(c, env, script).await.unwrap_or(last);
        if last.contains(want) {
            return Ok(());
        }
        if start.elapsed() + Duration::from_secs(3) >= cap {
            let seen: String = last.trim().chars().take(160).collect();
            return Err(anyhow!("{TARGET}:{TARGET_PORT} never answered {want:?} in {} ms; last answer: {seen:?}", cap.as_millis()));
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// `env.intercept`, `env.intercept.refused` and `env.intercept.fallback`, in that order.
///
/// The refusals sit BETWEEN the other two on purpose: the 422 needs a workspace that is attached
/// and running, and the fallback is the step that stops it. Two of them share one environment,
/// one workspace and one listener because standing those up is the whole cost.
async fn intercepts(c: &mut Ctx) {
    if c.suite != Suite::Hourly {
        return;
    }
    if c.kube.is_none() {
        return return_skip(c, "no kubeconfig");
    }
    let (env, ws) = match stand_up(c).await {
        Ok(pair) => pair,
        Err(e) => return return_skip(c, &format!("{e:#}")),
    };
    let held = intercept(c, &env, &ws).await;
    refused(c, &env, &ws).await;
    fallback(c, &env, &ws, held).await;
    // Best effort, exactly like `experience_ws`'s own cleanup: teardown's `run-{run_id}` prefix
    // sweep finds both by name anyway, and deleting here only keeps the run from holding a second
    // environment's worth of quota while the rest of the journey runs.
    for url in [api(c, &format!("/v1/workspaces/{ws}")), api(c, &format!("/v1/environments/{env}"))] {
        if let Err(e) = call(c, reqwest::Method::DELETE, &url, &c.probe_jwt, None).await {
            tracing::warn!(op = "delete", url = %super::path_of(&url), error = %format!("{e:#}"), "slo.intercept.cleanup");
        }
    }
}

/// Every id skipped once with the same reason — a hole that reads as "the run could not get to
/// this" rather than as a green sample or as no sample at all.
fn return_skip(c: &mut Ctx, why: &str) {
    for id in INTERCEPT_IDS {
        c.skip(id, why);
    }
}

/// The environment, the attached workspace and the listener — everything the three ids share, and
/// none of it measured: a create that takes two minutes is not what `env.intercept`'s 120 s is
/// the ceiling for.
async fn stand_up(c: &mut Ctx) -> Result<(String, String)> {
    let env = create_intercept_env(c, &format!("{}-icept", c.prefix())).await?;
    // A fresh environment's Volume carries the environment's own id, and teardown's prefix sweep
    // sees the environment but not the volume behind it.
    c.state.extra_volumes.push(env.clone());
    let ws = super::experience_ws::create(c, &format!("{}-iceptws", c.prefix()), serde_json::json!({ "packages": [] }))
        .await
        .context("could not create the intercepting workspace")?;
    let url = api(c, &format!("/v1/workspaces/{ws}/attach"));
    post(c, &url, &c.probe_jwt, serde_json::json!({ "environment": env }))
        .await
        .context("could not attach the intercepting workspace")?;
    let (code, out, err) = super::workspace::ws_exec(c, &ws, LISTENER, WS_CEILING).await?;
    if code != 0 {
        return Err(anyhow!("the workspace listener never came up ({code}): {} {}", out.trim(), err.trim()));
    }
    Ok((env, ws))
}

/// This journey's own environment: `SERVICE` to dial FROM, `TARGET` to intercept.
async fn create_intercept_env(c: &Ctx, name: &str) -> Result<String> {
    let body = serde_json::json!({
        "name": name,
        "region": c.cfg.region,
        "quota_gb": QUOTA_GB,
        "services": [
            { "name": SERVICE, "image": IMAGE, "command": [], "env": {}, "mounts": [], "ports": [PORT] },
            {
                "name": TARGET,
                "image": IMAGE,
                "command": ["redis-server", "--port", TARGET_PORT.to_string()],
                "env": {},
                "mounts": [],
                "ports": [TARGET_PORT],
            },
        ],
    });
    let doc = post(c, &api(c, "/v1/environments"), &c.probe_jwt, body)
        .await
        .context("could not create the intercept environment")?;
    let id = doc
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("the create answered no environment id"))?
        .to_string();
    let url = api(c, &format!("/v1/environments/{id}"));
    poll_json(c, &url, &c.probe_jwt, CREATE_CEILING, |v| v.get("state").and_then(Value::as_str) == Some("running")).await?;
    sts_ready(c, &id, SERVICE, CREATE_CEILING).await?;
    sts_ready(c, &id, TARGET, CREATE_CEILING).await?;
    Ok(id)
}

/// `env.intercept`: the environment's traffic to `TARGET:8080` is delivered to the workspace on
/// 3000, dialled by `TARGET`'s own name and port from a sibling pod.
async fn intercept(c: &mut Ctx, env: &str, ws: &str) -> bool {
    let (e, w) = (env.to_string(), ws.to_string());
    c.step("env.intercept", INTERCEPT_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/environments/{e}/intercepts"));
        let body = serde_json::json!({
            "service": TARGET,
            "workspace": w,
            "ports": [{ "service": TARGET_PORT, "workspace": WS_PORT }],
        });
        async move {
            post(c, &url, &jwt, body).await.context("could not intercept the service")?;
            // Polled, never asked once: the StatefulSet goes to 0, the Service loses its selector
            // and kube-proxy programs the slice, and none of that is synchronous with the 202.
            answers(c, &e, &workspace_dial(), MARKER, INTERCEPT_CEILING).await
        }
        .boxed()
    })
    .await
}

/// `env.intercept.fallback`: the workspace is STOPPED through `/v1` — never released by hand,
/// which would not exercise this path at all — and the real service comes back on its own.
///
/// Both halves are asserted, because they are one rule: what is IN FORCE goes away, and the WISH
/// stays. A fallback that also cleared `spec.intercepts` would silently throw away what the
/// person asked for, and the traffic assertion alone passes straight through that.
async fn fallback(c: &mut Ctx, env: &str, ws: &str, held: bool) {
    if !held {
        return c.skip("env.intercept.fallback", "the service was never intercepted");
    }
    let (e, w) = (env.to_string(), ws.to_string());
    c.step("env.intercept.fallback", FALLBACK_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let stop = api(c, &format!("/v1/workspaces/{w}/stop"));
        let read = api(c, &format!("/v1/environments/{e}"));
        async move {
            post(c, &stop, &jwt, Value::Null).await.context("could not stop the intercepting workspace")?;
            answers(c, &e, &service_dial(), "PONG", FALLBACK_CEILING).await?;
            let doc = get(c, &read, &jwt).await.context("could not read the environment back")?;
            if let Some(by) = intercepted_by(&doc, TARGET) {
                return Err(anyhow!("{TARGET} answers for itself again and still reports intercepted by {by}"));
            }
            if !wished(&doc, TARGET) {
                return Err(anyhow!("stopping the workspace dropped the intercept from the environment's spec"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// What is IN FORCE for `svc`, from the environment document's own service status — `service_status`,
/// never `services`, which is spec and carries no status at all.
fn intercepted_by(doc: &Value, svc: &str) -> Option<String> {
    doc.get("service_status")?
        .as_array()?
        .iter()
        .find(|s| s.get("name").and_then(Value::as_str) == Some(svc))?
        .get("intercepted_by")?
        .as_str()
        .map(str::to_string)
}

/// Whether the WISH for `svc` is still on the environment.
fn wished(doc: &Value, svc: &str) -> bool {
    doc.get("intercepts")
        .and_then(Value::as_array)
        .is_some_and(|v| v.iter().any(|i| i.get("service").and_then(Value::as_str) == Some(svc)))
}

/// `env.intercept.refused`: the two guards that stop an intercept pointing traffic somewhere
/// nobody authorised — a workspace that is not attached here, and a port the service does not
/// declare. The status AND the sentence, because a 409 that names nothing leaves a person guessing.
async fn refused(c: &mut Ctx, env: &str, ws: &str) {
    // The fast journey's own workspace, which stage 6 has already detached: a workspace that is
    // the caller's (so the route gets past its 404) and is attached somewhere else, or nowhere.
    let Some(other) = c.state.workspace.clone() else {
        return c.skip("env.intercept.refused", "no second workspace to refuse");
    };
    let (e, w) = (env.to_string(), ws.to_string());
    c.step("env.intercept.refused", REFUSED_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/environments/{e}/intercepts"));
        let unattached = serde_json::json!({ "service": TARGET, "workspace": other, "ports": [] });
        // A port the service does not declare, on the workspace that IS attached — so the only
        // thing wrong with the request is the port.
        let bad_port = serde_json::json!({
            "service": TARGET,
            "workspace": w,
            "ports": [{ "service": 9999, "workspace": WS_PORT }],
        });
        async move {
            for (body, want, names) in [
                (unattached, reqwest::StatusCode::CONFLICT, "not attached"),
                (bad_port, reqwest::StatusCode::UNPROCESSABLE_ENTITY, "9999"),
            ] {
                let (status, text) = raw(c, reqwest::Method::POST, &url, &jwt, Some(body), &[]).await?;
                if status != want {
                    return Err(anyhow!("the intercept answered {status}, not {want}: {}", text.trim()));
                }
                if !text.contains(names) {
                    return Err(anyhow!("the refusal does not say what was wrong ({names:?}): {}", text.trim()));
                }
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;

    /// The listener script and the constants the intercept is written against are one statement:
    /// a remapped port that the listener does not actually listen on would make every run fail
    /// with "never answered", pointing at the intercept rather than at this file.
    #[test]
    fn the_listener_listens_on_the_port_the_intercept_maps_to() {
        assert!(LISTENER.contains(&format!("port: {WS_PORT}")), "{LISTENER}");
        assert!(LISTENER.contains(&format!("127.0.0.1:{WS_PORT}")), "{LISTENER}");
        // Served and asserted on, both by name: the log file happens to carry the marker too.
        assert!(LISTENER.contains(&format!("new Response(\"{MARKER}\")")), "{LISTENER}");
        assert!(LISTENER.contains(&format!("=== \"{MARKER}\"")), "{LISTENER}");
        // The whole point of the id: the environment dials the service's port, never the
        // workspace's.
        assert_ne!(WS_PORT, TARGET_PORT);
        assert!(workspace_dial().contains(&format!("{TARGET} {TARGET_PORT}")));
        assert!(service_dial().contains(&format!("-p {TARGET_PORT}")));
    }

    /// Both halves of the fallback rule, read off the shape the api answers.
    #[test]
    fn the_wish_and_what_is_in_force_are_read_separately() {
        let doc = serde_json::json!({
            "service_status": [{ "name": TARGET, "ready": true, "intercepted_by": "ws-1" }],
            "intercepts": [{ "service": TARGET, "workspace": "ws-1" }],
        });
        assert_eq!(intercepted_by(&doc, TARGET).as_deref(), Some("ws-1"));
        assert!(wished(&doc, TARGET));
        // The state a released intercept leaves: nothing in force, the wish untouched.
        let doc = serde_json::json!({
            "service_status": [{ "name": TARGET, "ready": true }],
            "intercepts": [{ "service": TARGET, "workspace": "ws-1" }],
        });
        assert_eq!(intercepted_by(&doc, TARGET), None);
        assert!(wished(&doc, TARGET));
        // And what a regression to clearing the wish looks like.
        assert!(!wished(&serde_json::json!({ "service_status": [], "intercepts": [] }), TARGET));
    }

    /// The three ids are the hourly suite's alone: a fast run files NO sample for any of them,
    /// and an hourly run that cannot get to them skips each exactly once.
    #[tokio::test]
    async fn the_intercept_ids_belong_to_the_hourly_suite_only() {
        let app = || axum::Router::new().fallback(axum::routing::get(|| async { axum::http::StatusCode::NOT_FOUND }));
        let mut c = testkit::ctx_against(app()).await;
        c.kube = None;
        intercepts(&mut c).await;
        assert!(!c.steps.iter().any(|s| INTERCEPT_IDS.contains(&s.slo_id.as_str())), "a fast run reported an hourly id");

        let mut c = testkit::ctx_against(app()).await;
        c.kube = None;
        c.suite = Suite::Hourly;
        intercepts(&mut c).await;
        for id in INTERCEPT_IDS {
            let rows: Vec<_> = c.steps.iter().filter(|s| s.slo_id == id).collect();
            assert_eq!(rows.len(), 1, "{id} was not reported exactly once");
            assert!(rows[0].skipped && rows[0].detail == "no kubeconfig", "{:?}", rows[0]);
        }
    }

    /// The gate above and the catalogue's own `walks()` are two statements of one rule.
    #[test]
    fn the_catalogue_walks_the_intercept_ids_in_exactly_the_hourly_journey() {
        for suite in [Suite::Fast, Suite::Hourly, Suite::Weekly, Suite::Monthly] {
            let ids: Vec<&str> = kloudlite_workspaces::slo::catalogue::journey(suite)
                .into_iter()
                .flat_map(|(_, ids)| ids)
                .collect();
            for id in INTERCEPT_IDS {
                assert_eq!(ids.contains(&id), suite == Suite::Hourly, "{id} in {suite:?}'s journey disagrees with the stage's own gate");
            }
        }
    }
}
