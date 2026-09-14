//! Stage 6's intercept journey, hourly: one environment, one workspace that takes over one of its
//! services, and every vantage point that service can be dialled from.
//!
//! **Every id here ends in a DIAL whose bytes are checked.** The bug this journey exists for is one
//! where every object was perfect and the packets were dropped — a step that asserts an object's
//! state and not an answer is not a probe of this mechanism at all.
//!
//! Split out of `environment.rs` when the three ids became twelve: the fast journey and this one
//! share the environment SHAPE and nothing else.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::api::ListParams;
use kube::ResourceExt;
use serde_json::Value;

use kloudlite_workspaces::crd::{env_namespace, ws_namespace};
use kloudlite_workspaces::k8s;
use kloudlite_workspaces::slo::catalogue::Suite;

use super::environment::{my_space, sts_ready, EXEC_CEILING, IMAGE, PORT, QUOTA_GB, SERVICE};
use super::{api, call, get, poll_json, post, raw};
use crate::ctx::Ctx;

/// Every id this journey owns. It is the skip list a run that cannot get here files, and a missing
/// id reads as passed — so it must name all of them, not only the ones a given path reaches.
const INTERCEPT_IDS: [&str; 13] = [
    "env.space.bench",
    "env.intercept",
    "env.intercept.proxy.up",
    "env.intercept.delivered",
    "env.intercept.remap",
    "env.intercept.peer",
    "env.intercept.bench",
    "env.intercept.proxy.restart",
    "env.intercept.release",
    "env.intercept.fallback",
    "env.intercept.refused",
    "env.intercept.udp.refused",
    "env.intercept.tools.refused",
];

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
/// busybox `nc`, which is in the workspace IMAGE, never a package: the workspace is created with
/// `"packages": []`, so its nix profile holds the base set and nothing else. An earlier version
/// used `bun` and skipped every intercept id on the fleet with "failed to run command 'bun'" —
/// bun was only ever present in a workspace whose owner had installed it.
///
/// One connection per `nc -l`, so the loop restarts it; the dial side polls, which covers the gap.
/// No Content-Length: the answer ends when the connection closes, which is what `nc -w 3` reads.
///
/// Idempotent by design: `env.intercept.proxy.restart` kills the pod and runs it again, and a
/// second copy of the loop would fight the first for the port rather than fail visibly.
const LISTENER: &str = r#"body='HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nslo-intercept\n'
pkill -f "nc -l -p 3000" 2>/dev/null
nohup sh -c "while true; do printf '$body' | nc -l -p 3000; done" > /tmp/slo-intercept.log 2>&1 &
i=0
while [ $i -lt 20 ]; do
  if printf 'GET / HTTP/1.0\r\n\r\n' | nc -w 3 127.0.0.1 3000 2>/dev/null | grep -q slo-intercept; then
    echo listening
    exit 0
  fi
  i=$((i + 1))
  sleep 1
done
cat /tmp/slo-intercept.log
exit 1"#;

/// The catalogue's 120 s for `env.intercept`: the POST, the proxy pod pulled and Ready, and the
/// endpoints behind the ClusterIP naming it.
const INTERCEPT_CEILING: Duration = Duration::from_secs(120);
/// `env.intercept.proxy.up`'s own 90 s, against a service that is already in force.
const PROXY_UP_CEILING: Duration = Duration::from_secs(90);
/// The remap is two dials against a converged intercept; nothing has to happen for it.
const REMAP_CEILING: Duration = Duration::from_secs(60);
/// A pod delete, the kubelet's restart, and the target Service picking up the new address.
const RESTART_CEILING: Duration = Duration::from_secs(180);
/// The release is the intercept in reverse: the proxy deleted, the StatefulSet back up cold.
const RELEASE_CEILING: Duration = Duration::from_secs(180);
/// 180 s for the fallback, which is the same path plus `INTERCEPT_GRACE_SECS` and a cold start.
const FALLBACK_CEILING: Duration = Duration::from_secs(180);
/// Refusals, and nothing to converge.
const REFUSED_CEILING: Duration = Duration::from_secs(30);
/// A workspace create with its pod pulled and running, matching `experience_ws`'s own wait.
const WS_CEILING: Duration = Duration::from_secs(300);
/// `env.space.bench`: a wake (up to 90 s) plus the reconcile, against the catalogue's 120 s.
const SPACE_BENCH_CEILING: Duration = Duration::from_secs(150);
/// The bench's own dial, once it is awake and following the environment.
const BENCH_DIAL_CEILING: Duration = Duration::from_secs(120);
/// How long teardown waits for the proxy to go before calling it left behind.
const TEARDOWN_CEILING: Duration = Duration::from_secs(60);

/// `env.intercept`, its nine neighbours and the two refusals, in journey order.
///
/// The refusals come FIRST on purpose: the "no environment" case clears the space for a moment,
/// which would release an intercept already in force, and the 422s need the workspace running.
/// Everything after that shares one environment, one workspace and one listener, because standing
/// those up is the whole cost.
pub async fn run(c: &mut Ctx) {
    if c.suite != Suite::Hourly {
        return;
    }
    if c.kube.is_none() {
        return skip_all(c, "no kubeconfig");
    }
    let (env, ws) = match stand_up(c).await {
        Ok(pair) => pair,
        Err(e) => return skip_all(c, &format!("{e:#}")),
    };
    space_bench(c, &env).await;
    refused(c, &env, &ws).await;
    tools_refused(c, &env, &ws).await;
    // Not a pass and not a hole: see `udp_refused`.
    udp_refused(c);
    let held = intercept(c, &env, &ws).await;
    proxy_up(c, &env, held).await;
    delivered(c, &env, held).await;
    remap(c, &env, &ws, held).await;
    peer(c, &env, held).await;
    bench_reaches(c, &env, held).await;
    proxy_restart(c, &env, &ws, held).await;
    release(c, &env, &ws, held).await;
    fallback(c, &env, &ws, held).await;
    teardown(c, &env, &ws).await;
}

/// Every id skipped once with the same reason — a hole that reads as "the run could not get to
/// this" rather than as a green sample or as no sample at all.
fn skip_all(c: &mut Ctx, why: &str) {
    for id in INTERCEPT_IDS {
        c.skip(id, why);
    }
}

// ── standing it up ────────────────────────────────────────────────────────────

/// The environment, the attached workspace and the listener — everything the ids share, and none
/// of it measured: a create that takes two minutes is not what `env.intercept`'s 120 s is for.
async fn stand_up(c: &mut Ctx) -> Result<(String, String)> {
    let env = create_intercept_env(c, &format!("{}-icept", c.prefix())).await?;
    // A fresh environment's Volume carries the environment's own id, and teardown's prefix sweep
    // sees the environment but not the volume behind it.
    c.state.extra_volumes.push(env.clone());
    let ws = super::experience_ws::create(c, &format!("{}-iceptws", c.prefix()), serde_json::json!({ "packages": [] }))
        .await
        .context("could not create the intercepting workspace")?;
    call(c, reqwest::Method::PUT, &my_space(c), &c.probe_jwt, Some(serde_json::json!({ "environment": env })))
        .await
        .context("could not choose the intercept environment for the space")?;
    listen(c, &ws).await?;
    Ok((env, ws))
}

/// Start (or restart) the workspace's listener and wait until it answers itself.
async fn listen(c: &Ctx, ws: &str) -> Result<()> {
    let (code, out, err) = super::workspace::ws_exec(c, ws, LISTENER, WS_CEILING).await?;
    if code != 0 {
        return Err(anyhow!("the workspace listener never came up ({code}): {} {}", out.trim(), err.trim()));
    }
    Ok(())
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
    poll_json(c, &url, &c.probe_jwt, super::environment::CREATE_CEILING, |v| {
        v.get("state").and_then(Value::as_str) == Some("running")
    })
    .await?;
    sts_ready(c, &id, SERVICE, super::environment::CREATE_CEILING).await?;
    sts_ready(c, &id, TARGET, super::environment::CREATE_CEILING).await?;
    Ok(id)
}

// ── dialling ──────────────────────────────────────────────────────────────────

/// One dial from the environment's own `SERVICE` pod, answering whatever came back on stdout.
///
/// `env.dns`'s vantage point exactly — a sibling service inside the environment's namespace,
/// which is the only place the answer means anything.
async fn dial(c: &Ctx, env: &str, script: &str) -> Result<String> {
    let k = c.kube.as_ref().ok_or_else(|| anyhow!("no kubeconfig"))?;
    let ns = env_namespace(env);
    let pod = format!("{SERVICE}-0");
    let (_, out, _) = crate::kube::exec(k, &ns, &pod, None, &["sh", "-c", script], EXEC_CEILING).await?;
    Ok(out)
}

/// A bare HTTP GET, written for busybox `nc`: the one client present in every image this journey
/// dials from.
fn http_get(host: &str, port: u16) -> String {
    format!(r"printf 'GET / HTTP/1.0\r\n\r\n' | nc -w 3 {host} {port}")
}

/// An HTTP GET at `TARGET`'s own name and port — the address callers dial, unchanged by the
/// intercept. Only the workspace answers this with `MARKER`; the real redis answers an error.
fn workspace_dial() -> String {
    http_get(TARGET, TARGET_PORT)
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
            return Err(anyhow!(
                "{TARGET}:{TARGET_PORT} never answered {want:?} in {} ms; last answer: {seen:?}",
                cap.as_millis()
            ));
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

// ── the intercept itself ──────────────────────────────────────────────────────

/// `env.intercept`: the environment's traffic to `TARGET:8080` is delivered to the workspace on
/// 3000, dialled by `TARGET`'s own name and port from a sibling pod.
async fn intercept(c: &mut Ctx, env: &str, ws: &str) -> bool {
    let (e, w) = (env.to_string(), ws.to_string());
    c.step("env.intercept", INTERCEPT_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/environments/{e}/intercepts"));
        async move {
            post(c, &url, &jwt, intercept_body(&w, TARGET_PORT, WS_PORT)).await.context("could not intercept the service")?;
            // Polled, never asked once: the proxy pod has to be pulled and Ready and the ClusterIP's
            // endpoints have to name it, and none of that is synchronous with the answer.
            answers(c, &e, &workspace_dial(), MARKER, INTERCEPT_CEILING).await
        }
        .boxed()
    })
    .await
}

fn intercept_body(ws: &str, service_port: u16, ws_port: u16) -> Value {
    serde_json::json!({
        "service": TARGET,
        "workspace": ws,
        "ports": [{ "service": service_port, "workspace": ws_port }],
    })
}

/// `env.intercept.proxy.up`: the environment's own document says the proxy is `ready`, AND the
/// service answers with the workspace's marker.
///
/// The dial is half the id on purpose: `proxy: "ready"` is a state, and this whole journey exists
/// because a perfect set of objects delivered no packets. A state-only version of this step would
/// have passed on the mechanism the proxy replaced.
async fn proxy_up(c: &mut Ctx, env: &str, held: bool) {
    if !held {
        return c.skip("env.intercept.proxy.up", NOT_HELD);
    }
    let e = env.to_string();
    c.step("env.intercept.proxy.up", PROXY_UP_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let read = api(c, &format!("/v1/environments/{e}"));
        async move {
            poll_json(c, &read, &jwt, PROXY_UP_CEILING, |v| proxy_state(v, TARGET).as_deref() == Some("ready"))
                .await
                .context("the proxy never reported ready")?;
            let out = dial(c, &e, &workspace_dial()).await?;
            if !out.contains(MARKER) {
                return Err(anyhow!("the proxy reports ready and {TARGET}:{TARGET_PORT} answered {:?}", super::clip(out.trim())));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `env.intercept.delivered`: the plain question, asked from inside the environment.
async fn delivered(c: &mut Ctx, env: &str, held: bool) {
    if !held {
        return c.skip("env.intercept.delivered", NOT_HELD);
    }
    let e = env.to_string();
    c.step("env.intercept.delivered", INTERCEPT_CEILING, move |c| {
        async move { answers(c, &e, &workspace_dial(), MARKER, INTERCEPT_CEILING).await }.boxed()
    })
    .await;
}

/// `env.intercept.remap`: the workspace listens on 3000, the environment dials 8080, and 3000 is
/// NOT what the environment reaches — the remap is real, not two ports that happen to agree.
async fn remap(c: &mut Ctx, env: &str, ws: &str, held: bool) {
    if !held {
        return c.skip("env.intercept.remap", NOT_HELD);
    }
    let (e, w) = (env.to_string(), ws.to_string());
    c.step("env.intercept.remap", REMAP_CEILING, move |c| {
        async move {
            // The listener is where the mapping says it is, judged from inside the workspace.
            let (_, out, err) = super::workspace::ws_exec(c, &w, &http_get("127.0.0.1", WS_PORT), EXEC_CEILING).await?;
            if !out.contains(MARKER) {
                return Err(anyhow!("the workspace does not answer on {WS_PORT}: {:?} {}", super::clip(out.trim()), err.trim()));
            }
            // The workspace's OWN port, dialled at the service's name, reaches nothing: only the
            // ports the intercept maps are carried.
            let leaked = dial(c, &e, &http_get(TARGET, WS_PORT)).await.unwrap_or_default();
            if leaked.contains(MARKER) {
                return Err(anyhow!("{TARGET}:{WS_PORT} answered the workspace's marker, so the ports were never remapped"));
            }
            // And the address a caller actually dials still answers. The id ends in the dial.
            answers(c, &e, &workspace_dial(), MARKER, REMAP_CEILING).await
        }
        .boxed()
    })
    .await;
}

/// `env.intercept.peer` — **the bug, as a probe.**
///
/// This would have failed every hour since spaces shipped: the follower's packets were DNAT'd to a
/// pod in another owner's namespace, whose ingress policy admitted only the environment's.
///
/// The second owner has to FOLLOW the environment for the question to be the one worth asking, and
/// a space may use only its own team's environments (`api::me::set_my_environment`). This journey's
/// environment is the probe owner's personal one, so today the follow is refused and the id SKIPS
/// with what the api said — never passes. It runs as written the day this environment is a team's.
async fn peer(c: &mut Ctx, env: &str, held: bool) {
    if !held {
        return c.skip("env.intercept.peer", NOT_HELD);
    }
    // Outside the step: standing the follower up is not what the 120 s is the ceiling for, and a
    // follow the platform refuses is a skip rather than a failed sample.
    let space = api(c, &format!("/v1/me/environments/{}", c.probe_user));
    let body = serde_json::json!({ "environment": env });
    let followed = raw(c, reqwest::Method::PUT, &space, &c.other_jwt.clone(), Some(body), &[]).await;
    match followed {
        Ok((status, text)) if !status.is_success() => {
            return c.skip(
                "env.intercept.peer",
                &format!("the second owner cannot follow this environment ({status}): {}", super::clip(text.trim())),
            );
        }
        Err(e) => return c.skip("env.intercept.peer", &format!("could not make the second owner follow: {e:#}")),
        _ => {}
    }
    let name = format!("{}-peerws", c.prefix());
    let peer_ws = match create_as(c, &c.other_jwt.clone(), &name).await {
        Ok(id) => id,
        Err(e) => return c.skip("env.intercept.peer", &format!("the follower's workspace never came up: {e:#}")),
    };
    let (e, ns) = (peer_ws, env_namespace(env));
    c.step("env.intercept.peer", INTERCEPT_CEILING, move |c| {
        async move {
            // Fully qualified: the follower is being asked about REACHABILITY, not about whose
            // resolv.conf carries which search domain.
            let script = http_get(&format!("{TARGET}.{ns}.svc.cluster.local"), TARGET_PORT);
            let start = std::time::Instant::now();
            loop {
                let out = peer_exec(c, &e, &script).await.unwrap_or_default();
                if out.contains(MARKER) {
                    return Ok(());
                }
                if start.elapsed() + Duration::from_secs(3) >= INTERCEPT_CEILING {
                    return Err(anyhow!("the follower never reached the intercepted service: {:?}", super::clip(out.trim())));
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
        .boxed()
    })
    .await;
}

/// A workspace for the SECOND owner. `experience_ws::create` is the probe owner's alone (it holds
/// their jwt), and the follower's whole point is that it is somebody else's.
async fn create_as(c: &Ctx, jwt: &str, name: &str) -> Result<String> {
    let body = serde_json::json!({ "name": name, "region": c.cfg.region, "quota_gb": QUOTA_GB, "packages": [] });
    let doc = post(c, &api(c, "/v1/workspaces"), jwt, body).await.context("could not create the follower's workspace")?;
    let id = super::id_of(&doc)?;
    let url = api(c, &format!("/v1/workspaces/{id}"));
    poll_json(c, &url, jwt, WS_CEILING, |v| super::state_is(v, "ready")).await.context("it never became ready")?;
    Ok(id)
}

/// One exec in the SECOND owner's workspace pod — their namespace, not the probe owner's.
async fn peer_exec(c: &Ctx, id: &str, script: &str) -> Result<String> {
    let k = c.kube.as_ref().ok_or_else(|| anyhow!("no kubeconfig"))?;
    let ns = ws_namespace(&c.other_user, "");
    let user = k8s::SSH_USER;
    let (_, out, _) =
        crate::kube::exec(k, &ns, id, Some(super::workspace::WS_CONTAINER), &["su", user, "-s", "/bin/sh", "-c", script], EXEC_CEILING)
            .await?;
    Ok(out)
}

/// `env.intercept.bench`: the probe owner's bench — a pod of the same space, one layer further from
/// a workspace — reaches the intercepted service. The bench's own exec path, the one
/// `env.space.bench` reads its resolv.conf through.
///
/// Three clients tried in turn because the bench image is the HARNESS's, not the workspace's, and
/// which of them it ships has changed under us before; the answer is judged on the marker either
/// way, so a bench with none of them fails loudly rather than passing on an exit code.
async fn bench_reaches(c: &mut Ctx, env: &str, held: bool) {
    if !held {
        return c.skip("env.intercept.bench", NOT_HELD);
    }
    let host = format!("{TARGET}.{}.svc.cluster.local", env_namespace(env));
    c.step("env.intercept.bench", BENCH_DIAL_CEILING, move |c| {
        async move {
            let script = format!(
                "curl -s -m 5 http://{host}:{TARGET_PORT}/ || wget -qO- -T 5 http://{host}:{TARGET_PORT}/ || {}",
                http_get(&host, TARGET_PORT)
            );
            let start = std::time::Instant::now();
            loop {
                let out = bench_exec(c, &script).await.unwrap_or_default();
                if out.contains(MARKER) {
                    return Ok(());
                }
                if start.elapsed() + Duration::from_secs(5) >= BENCH_DIAL_CEILING {
                    return Err(anyhow!("the bench never reached the intercepted service: {:?}", super::clip(out.trim())));
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
        .boxed()
    })
    .await;
}

async fn bench_exec(c: &Ctx, script: &str) -> Result<String> {
    let k = c.kube.as_ref().ok_or_else(|| anyhow!("no kubeconfig"))?;
    let ns = ws_namespace(&c.probe_user, &c.probe_user);
    let (_, out, _) =
        crate::kube::exec(k, &ns, k8s::BENCH_POD, Some(k8s::BENCH_CONTAINER), &["sh", "-c", script], EXEC_CEILING).await?;
    Ok(out)
}

/// `env.intercept.proxy.restart`: the INTERCEPTING WORKSPACE's pod is killed, and delivery comes
/// back without the proxy being recreated.
///
/// The uid assertion is the whole claim of a proxy over baked-in addressing: the workspace's pod
/// returns on a different IP, and what absorbs it is the target Service the proxy dials by name.
/// A regression that re-rendered the proxy around the new address would still deliver — and would
/// be caught here alone.
async fn proxy_restart(c: &mut Ctx, env: &str, ws: &str, held: bool) {
    if !held {
        return c.skip("env.intercept.proxy.restart", NOT_HELD);
    }
    let before = match proxy_uid(c, env).await {
        Ok(uid) => uid,
        Err(e) => return c.skip("env.intercept.proxy.restart", &format!("the proxy pod could not be read: {e:#}")),
    };
    let (e, w) = (env.to_string(), ws.to_string());
    c.step("env.intercept.proxy.restart", RESTART_CEILING, move |c| {
        async move {
            let k = c.kube.clone().ok_or_else(|| anyhow!("no kubeconfig"))?;
            let pods: kube::Api<Pod> = kube::Api::namespaced(k, &ws_namespace(&c.probe_user, ""));
            pods.delete(&w, &Default::default()).await.context("could not delete the intercepting workspace's pod")?;
            // The pod comes back empty: the listener is a process, not a file.
            listen(c, &w).await.context("the listener never came back in the restarted pod")?;
            answers(c, &e, &workspace_dial(), MARKER, RESTART_CEILING).await?;
            let after = proxy_uid(c, &e).await.context("the proxy pod is gone after the restart")?;
            if after != before {
                return Err(anyhow!("the proxy pod was recreated ({before} -> {after}), so something addressed the workspace pod directly"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `env.intercept.release`: the wish is deleted, and all four things a release owes are true —
/// the real service answers, its StatefulSet is back, the proxy is gone, and no grant is left.
///
/// The policies are listed BY LABEL, never by guessing a name: a grant left behind under a name
/// this probe does not know is exactly the leak worth catching.
async fn release(c: &mut Ctx, env: &str, ws: &str, held: bool) {
    if !held {
        return c.skip("env.intercept.release", NOT_HELD);
    }
    let (e, w) = (env.to_string(), ws.to_string());
    c.step("env.intercept.release", RELEASE_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/environments/{e}/intercepts/{TARGET}"));
        async move {
            call(c, reqwest::Method::DELETE, &url, &jwt, None).await.context("could not release the intercept")?;
            answers(c, &e, &service_dial(), "PONG", RELEASE_CEILING).await?;
            sts_ready(c, &e, TARGET, RELEASE_CEILING).await.context("the real service never came back to a ready replica")?;
            let env_ns = env_namespace(&e);
            let left = intercept_pods(c, &env_ns).await?;
            if !left.is_empty() {
                return Err(anyhow!("the release left the proxy pod {left:?} in {env_ns}"));
            }
            for ns in [env_ns, ws_namespace(&c.probe_user, "")] {
                let grants = intercept_policies(c, &ns).await?;
                if !grants.is_empty() {
                    return Err(anyhow!("the release left the grant {grants:?} in {ns}"));
                }
            }
            // The wish is gone too — this is the one verb that removes it.
            let doc = get(c, &api(c, &format!("/v1/environments/{e}")), &jwt).await?;
            if wished(&doc, TARGET) {
                return Err(anyhow!("the intercept of {TARGET} by {w} is still wished after the release"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `env.intercept.fallback`: the workspace is STOPPED through `/v1` — never released by hand,
/// which would not exercise this path at all — and the real service comes back on its own.
///
/// Both halves are asserted, because they are one rule: what is IN FORCE goes away, and the WISH
/// stays. A fallback that also cleared `spec.intercepts` would silently throw away what the
/// person asked for, and the traffic assertion alone passes straight through that.
///
/// The intercept `release` removed is put back FIRST, outside the step: re-converging is not what
/// this ceiling is for.
async fn fallback(c: &mut Ctx, env: &str, ws: &str, held: bool) {
    if !held {
        return c.skip("env.intercept.fallback", NOT_HELD);
    }
    let url = api(c, &format!("/v1/environments/{env}/intercepts"));
    let again = async {
        post(c, &url, &c.probe_jwt.clone(), intercept_body(ws, TARGET_PORT, WS_PORT)).await?;
        answers(c, env, &workspace_dial(), MARKER, INTERCEPT_CEILING).await
    }
    .await;
    if let Err(e) = again {
        return c.skip("env.intercept.fallback", &format!("the intercept could not be put back after the release: {e:#}"));
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

/// The reason every id downstream of the intercept skips with. One `const`, because a skip must
/// name the id exactly once and the sentence is the same wherever it is filed.
const NOT_HELD: &str = "the service was never intercepted";

// ── refusals ──────────────────────────────────────────────────────────────────

/// `env.intercept.refused`: the two guards that stop an intercept pointing traffic somewhere
/// nobody authorised — a workspace whose space uses no environment, and a port the service does
/// not declare. The status AND the sentence, because a 409 that names nothing leaves a person
/// guessing. The first case clears the space and puts the choice back before the second.
async fn refused(c: &mut Ctx, env: &str, ws: &str) {
    let (e, w) = (env.to_string(), ws.to_string());
    c.step("env.intercept.refused", REFUSED_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/environments/{e}/intercepts"));
        let space = my_space(c);
        let unattached = serde_json::json!({ "service": TARGET, "workspace": w, "ports": [] });
        // A port the service does not declare, on a workspace whose space DOES use it — so the
        // only thing wrong with the request is the port.
        let bad_port = intercept_body(&w, 9999, WS_PORT);
        async move {
            call(c, reqwest::Method::DELETE, &space, &jwt, None).await.context("could not clear the space")?;
            let first =
                expect_refusal(c, &url, &jwt, unattached, reqwest::StatusCode::CONFLICT, "does not use this environment").await;
            call(c, reqwest::Method::PUT, &space, &jwt, Some(serde_json::json!({ "environment": e })))
                .await
                .context("could not choose the environment again")?;
            first?;
            expect_refusal(c, &url, &jwt, bad_port, reqwest::StatusCode::UNPROCESSABLE_ENTITY, "9999").await
        }
        .boxed()
    })
    .await;
}

/// `env.intercept.tools.refused`: a mapping onto the tool server's port is refused naming it.
///
/// The tool server listens on the pod IP with no auth of its own, so an intercept that mapped a
/// service's port onto 7788 would hand every pod in the environment an `exec` in the workspace.
async fn tools_refused(c: &mut Ctx, env: &str, ws: &str) {
    let (e, w) = (env.to_string(), ws.to_string());
    c.step("env.intercept.tools.refused", REFUSED_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/environments/{e}/intercepts"));
        let body = intercept_body(&w, TARGET_PORT, k8s::IDE_PORT);
        async move { expect_refusal(c, &url, &jwt, body, reqwest::StatusCode::UNPROCESSABLE_ENTITY, "7788").await }.boxed()
    })
    .await;
}

/// `env.intercept.udp.refused`: skipped, with the reason, until a service can declare a UDP port.
///
/// `model::Service` carries no protocol, so nothing in an environment can be UDP and the probe has
/// no way to provoke the refusal. A skip is visible in the report; a silent pass would report a
/// guard nobody has as kept.
fn udp_refused(c: &mut Ctx) {
    c.skip("env.intercept.udp.refused", "model::Service carries no protocol yet");
}

async fn expect_refusal(c: &Ctx, url: &str, jwt: &str, body: Value, want: reqwest::StatusCode, names: &str) -> Result<()> {
    let (status, text) = raw(c, reqwest::Method::POST, url, jwt, Some(body), &[]).await?;
    if status != want {
        return Err(anyhow!("the intercept answered {status}, not {want}: {}", text.trim()));
    }
    if !text.contains(names) {
        return Err(anyhow!("the refusal does not say what was wrong ({names:?}): {}", text.trim()));
    }
    Ok(())
}

// ── the bench's space, and reading the environment back ───────────────────────

/// `env.space.bench`: the probe owner's bench is a pod of the same space, so the choice reaches its
/// `/etc/resolv.conf` too. Read as a file rather than a lookup: the bench image is the harness's,
/// and the platform mount is the thing under test. Woken first — an idle bench has no pod.
async fn space_bench(c: &mut Ctx, env: &str) {
    let want = format!("{}.svc.", env_namespace(env));
    c.step("env.space.bench", SPACE_BENCH_CEILING, move |c| {
        async move {
            let _ = raw(c, reqwest::Method::POST, &api(c, "/v1/bench/session"), &c.probe_jwt.clone(), None, &[]).await;
            super::bench::wait_phase(c, "ready", Duration::from_secs(90)).await?;
            let start = std::time::Instant::now();
            loop {
                let out = bench_exec(c, "cat /etc/resolv.conf").await.unwrap_or_default();
                if out.contains(&want) {
                    return Ok(());
                }
                if start.elapsed() >= SPACE_BENCH_CEILING - Duration::from_secs(5) {
                    return Err(anyhow!("the bench's resolv.conf never searched {want}: {:?}", out.lines().next().unwrap_or("")));
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
        .boxed()
    })
    .await;
}

/// One service's status row, out of the environment document. `service_status` is the doc's key and
/// the rows inside it are the CRD's own type, so THEY are camelCase where every other key is not —
/// a snake_case read here silently answered `None` for everything (fixed 2026-09-15).
fn service_status<'a>(doc: &'a Value, svc: &str) -> Option<&'a Value> {
    doc.get("service_status")?.as_array()?.iter().find(|s| s.get("name").and_then(Value::as_str) == Some(svc))
}

/// What is IN FORCE for `svc`.
fn intercepted_by(doc: &Value, svc: &str) -> Option<String> {
    service_status(doc, svc)?.get("interceptedBy")?.as_str().map(str::to_string)
}

/// `starting` | `ready` | `failed`, as the environment's controller last recorded it.
fn proxy_state(doc: &Value, svc: &str) -> Option<String> {
    service_status(doc, svc)?.get("proxy")?.as_str().map(str::to_string)
}

/// Whether the WISH for `svc` is still on the environment.
fn wished(doc: &Value, svc: &str) -> bool {
    doc.get("intercepts")
        .and_then(Value::as_array)
        .is_some_and(|v| v.iter().any(|i| i.get("service").and_then(Value::as_str) == Some(svc)))
}

// ── what the cluster still holds ──────────────────────────────────────────────

/// Every proxy pod in `ns`, by the label an intercept renders them with — never by name, so a pod
/// left behind under a name this probe does not know is still found.
async fn intercept_pods(c: &Ctx, ns: &str) -> Result<Vec<String>> {
    let k = c.kube.clone().ok_or_else(|| anyhow!("no kubeconfig"))?;
    let pods: kube::Api<Pod> = kube::Api::namespaced(k, ns);
    let lp = ListParams::default().labels(&format!("{}=intercept", k8s::KIND_LABEL));
    Ok(pods.list(&lp).await.context("could not list the namespace's pods")?.iter().map(ResourceExt::name_any).collect())
}

/// Every grant an intercept could have left: the platform's policies all carry `kind=policy`, and
/// an intercept's are the ones named `intercept-*` in either namespace.
async fn intercept_policies(c: &Ctx, ns: &str) -> Result<Vec<String>> {
    let k = c.kube.clone().ok_or_else(|| anyhow!("no kubeconfig"))?;
    let pol: kube::Api<NetworkPolicy> = kube::Api::namespaced(k, ns);
    let lp = ListParams::default().labels(&format!("{}=policy", k8s::KIND_LABEL));
    Ok(pol
        .list(&lp)
        .await
        .context("could not list the namespace's network policies")?
        .iter()
        .map(ResourceExt::name_any)
        .filter(|n| n.starts_with("intercept-"))
        .collect())
}

/// The proxy pod's `metadata.uid` — the identity `env.intercept.proxy.restart` turns on.
async fn proxy_uid(c: &Ctx, env: &str) -> Result<String> {
    let k = c.kube.clone().ok_or_else(|| anyhow!("no kubeconfig"))?;
    let pods: kube::Api<Pod> = kube::Api::namespaced(k, &env_namespace(env));
    let p = pods.get(&k8s::proxy_pod_name(TARGET)).await.context("could not read the proxy pod")?;
    p.metadata.uid.ok_or_else(|| anyhow!("the proxy pod carries no uid"))
}

// ── teardown ──────────────────────────────────────────────────────────────────

/// The stage's own cleanup, and it may never leave a proxy behind.
///
/// Best effort like every other stage's, because teardown's `run-{run_id}` prefix sweep finds the
/// environment and both workspaces by name anyway — but the intercepts are DELETED FIRST: a release
/// is what takes the proxy down, and an environment delete that raced one has left a proxy pod
/// running on the fleet before. The second owner's space is named by its owner, not by the run, so
/// no prefix sweep can see it; it is cleared here.
async fn teardown(c: &mut Ctx, env: &str, ws: &str) {
    let jwt = c.probe_jwt.clone();
    let drop = api(c, &format!("/v1/environments/{env}/intercepts/{TARGET}"));
    warn_on_err(c, reqwest::Method::DELETE, &drop, &jwt).await;
    let peer_space = api(c, &format!("/v1/me/environments/{}", c.probe_user));
    warn_on_err(c, reqwest::Method::DELETE, &peer_space, &c.other_jwt.clone()).await;
    for url in [api(c, &format!("/v1/workspaces/{ws}")), api(c, &format!("/v1/environments/{env}"))] {
        warn_on_err(c, reqwest::Method::DELETE, &url, &jwt).await;
    }
    // Asserted, not hoped for: a proxy the environment delete did not collect is a pod nobody
    // is billed for and no listing shows. Logged as a teardown failure, the line the next run's
    // sweep is judged by.
    let ns = env_namespace(env);
    let start = std::time::Instant::now();
    loop {
        // Polled, because the environment delete is a WISH: the proxy goes with the namespace, on
        // Kubernetes' own clock. What is being caught is one that never goes, not one still going.
        let left = intercept_pods(c, &ns).await.unwrap_or_default();
        if left.is_empty() {
            return;
        }
        if start.elapsed() >= TEARDOWN_CEILING {
            return tracing::warn!(kind = "intercept", pods = ?left, ns = %ns, "slo.teardown.failed");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

async fn warn_on_err(c: &Ctx, method: reqwest::Method, url: &str, jwt: &str) {
    if let Err(e) = call(c, method, url, jwt, None).await {
        tracing::warn!(op = "delete", url = %super::path_of(url), error = %format!("{e:#}"), "slo.intercept.cleanup");
    }
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
        assert!(LISTENER.contains(&format!("nc -l -p {WS_PORT}")), "{LISTENER}");
        assert!(LISTENER.contains(&format!("127.0.0.1 {WS_PORT}")), "{LISTENER}");
        // Served and asserted on, both by name: the log file happens to carry the marker too.
        assert!(LISTENER.contains(&format!("{MARKER}\\n'")), "{LISTENER}");
        assert!(LISTENER.contains(&format!("grep -q {MARKER}")), "{LISTENER}");
        // The runtime this leans on is the image's, not a package: the workspace is created with
        // an empty package list, and an earlier `bun` here skipped every id on the fleet.
        assert!(!LISTENER.contains("bun"), "{LISTENER}");
        // Run again after the pod is killed, so it must not stack a second copy on the port.
        assert!(LISTENER.contains("pkill"), "{LISTENER}");
        // The whole point of the id: the environment dials the service's port, never the
        // workspace's.
        assert_ne!(WS_PORT, TARGET_PORT);
        assert!(workspace_dial().contains(&format!("{TARGET} {TARGET_PORT}")));
        assert!(service_dial().contains(&format!("-p {TARGET_PORT}")));
    }

    /// Both halves of the fallback rule and the proxy's state, read off the shape `/v1` answers —
    /// whose service rows are camelCase where the document around them is not.
    #[test]
    fn the_wish_and_what_is_in_force_are_read_separately() {
        let doc = serde_json::json!({
            "service_status": [{ "name": TARGET, "ready": true, "interceptedBy": "ws-1", "proxy": "ready" }],
            "intercepts": [{ "service": TARGET, "workspace": "ws-1" }],
        });
        assert_eq!(intercepted_by(&doc, TARGET).as_deref(), Some("ws-1"));
        assert_eq!(proxy_state(&doc, TARGET).as_deref(), Some("ready"));
        assert!(wished(&doc, TARGET));
        // A snake_case row is what an older probe read, and it answered `None` for a service that
        // WAS intercepted — which passed the fallback's "no longer in force" check vacuously.
        let snake = serde_json::json!({ "service_status": [{ "name": TARGET, "intercepted_by": "ws-1" }] });
        assert_eq!(intercepted_by(&snake, TARGET), None);
        // The state a released intercept leaves: nothing in force, the wish untouched.
        let doc = serde_json::json!({
            "service_status": [{ "name": TARGET, "ready": true, "proxy": "starting" }],
            "intercepts": [{ "service": TARGET, "workspace": "ws-1" }],
        });
        assert_eq!(intercepted_by(&doc, TARGET), None);
        assert_eq!(proxy_state(&doc, TARGET).as_deref(), Some("starting"));
        assert!(wished(&doc, TARGET));
        // And what a regression to clearing the wish looks like.
        assert!(!wished(&serde_json::json!({ "service_status": [], "intercepts": [] }), TARGET));
    }

    /// The tool server's port is the one the refusal is about, and it is read from the platform
    /// rather than typed twice: the catalogue row and `deploy/slo.md` both name 7788.
    #[test]
    fn the_tools_refusal_names_the_tool_server_port() {
        assert_eq!(k8s::IDE_PORT, 7788);
        assert_eq!(intercept_body("ws-1", TARGET_PORT, k8s::IDE_PORT)["ports"][0]["workspace"], 7788);
    }

    /// Every id is the hourly suite's alone: a fast run files NO sample for any of them, and an
    /// hourly run that cannot get to them skips each exactly once.
    #[tokio::test]
    async fn the_intercept_ids_belong_to_the_hourly_suite_only() {
        let app = || axum::Router::new().fallback(axum::routing::get(|| async { axum::http::StatusCode::NOT_FOUND }));
        let mut c = testkit::ctx_against(app()).await;
        c.kube = None;
        run(&mut c).await;
        assert!(!c.steps.iter().any(|s| INTERCEPT_IDS.contains(&s.slo_id.as_str())), "a fast run reported an hourly id");

        let mut c = testkit::ctx_against(app()).await;
        c.kube = None;
        c.suite = Suite::Hourly;
        run(&mut c).await;
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
