//! Stage 6's intercept journey, hourly: one TEAM, one environment of that team, one workspace that
//! takes over one of its services, and every vantage point that service can be dialled from.
//!
//! **Every id here ends in a DIAL whose bytes are checked.** The bug this journey exists for is one
//! where every object was perfect and the packets were dropped — a step that asserts an object's
//! state and not an answer is not a probe of this mechanism at all.
//!
//! The journey is a TEAM's because `env.intercept.peer` has to be askable: a space may follow only
//! its own team's environment (`api::me::set_my_environment`), so a personal environment can have
//! no second follower and the id that exists for the bug could only ever skip. Both probe owners
//! join one `run-{id}-icept` team, both choose its environment for their space, and both of their
//! workspaces live in it.
//!
//! - `refusals.rs` — the three refusals, none of which needs the intercept in force.
//! - `delivery.rs` — the intercept and every vantage point it is dialled from.
//!
//! This file keeps what they share: the names, the dials, the readers of the environment document
//! and of the cluster, standing it up and taking it down.

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

use super::environment::{sts_ready, EXEC_CEILING, IMAGE, PORT, QUOTA_GB, SERVICE};
use super::{api, call, get, poll_json, post, raw};
use crate::ctx::Ctx;

mod delivery;
mod refusals;

/// Every id this journey owns, and group 1 of the hourly Job with it — one list, in the
/// catalogue, because the console partitions a group run's journey by the same ids.
pub(crate) use kloudlite_workspaces::slo::catalogue::INTERCEPT_IDS;

/// The service the workspace takes over. A SECOND service, not `SERVICE`: an intercept scales the
/// real StatefulSet to 0, so intercepting the only service would leave the namespace with no pod
/// left to dial from — and the dial has to come from inside the environment, because that is
/// whose traffic an intercept redirects.
pub(super) const TARGET: &str = "echo";
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
/// `node`, which is in the workspace IMAGE, never a package: the workspace is created with
/// `"packages": []`, so its nix profile holds the base set and nothing else. An earlier version
/// used `bun` and skipped every intercept id on the fleet with "failed to run command 'bun'" —
/// bun was only ever present in a workspace whose owner had installed it. Before that it was
/// busybox `nc`, which the ALPINE image had and the debian one does not.
///
/// One http server rather than a `nc -l` loop, so there is no window between connections; the
/// readiness dial below is bash's `/dev/tcp`, for the same reason the listener is not netcat.
///
/// Idempotent by design: `env.intercept.proxy.restart` kills the pod and runs it again, and a
/// second copy of the loop would fight the first for the port rather than fail visibly.
///
/// The previous listener is stopped by its saved pid and `pkill -x node` (process NAME), never
/// `pkill -f`:
/// this whole script is the `sh -c` argument, so ANY command-line pattern for the listener matches
/// the shell running it — and the exec killed itself (exit 143) before the listener started. Every
/// intercept id skipped on 2026-09-15, twice.
const LISTENER: &str = r#"[ -f /tmp/slo-intercept.pid ] && kill "$(cat /tmp/slo-intercept.pid)" 2>/dev/null
pkill -x node 2>/dev/null
nohup node -e "require('http').createServer((q,r)=>r.end('slo-intercept\n')).listen(3000,'0.0.0.0')" > /tmp/slo-intercept.log 2>&1 &
echo $! > /tmp/slo-intercept.pid
i=0
while [ $i -lt 20 ]; do
  if timeout 3 bash -c 'exec 3<>/dev/tcp/127.0.0.1/3000 && printf "GET / HTTP/1.0\r\n\r\n" >&3 && cat <&3' 2>/dev/null | grep -q slo-intercept; then
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
/// Every inner poll stops this far short of its step's own ceiling, so a breach names which half
/// missed rather than being cut off by the step's timeout with nothing said.
const SLACK: Duration = Duration::from_secs(5);

/// The team, environment and workspace every id here shares.
pub(super) struct Journey {
    team: String,
    env: String,
    ws: String,
    /// Why this team has no bench, if it has none. The two bench ids skip with it rather than
    /// failing: a bench that could not be created measures nothing about an intercept.
    bench: Option<String>,
}

impl Journey {
    /// The probe owner's space in THIS team — the one a workspace of it follows.
    fn space(&self, c: &Ctx) -> String {
        api(c, &format!("/v1/me/environments/{}", self.team))
    }

    /// Where both owners' workspaces for this journey live: one namespace per person per team.
    fn ws_ns(&self, owner: &str) -> String {
        ws_namespace(owner, &self.team)
    }
}

/// Which bench a `/v1/bench*` call is about is a QUERY parameter, never a body field
/// (`api::bench`: `Query<TeamQuery>` on the read and on `/session`) — a body `team` is silently
/// ignored and the caller's PERSONAL bench answers instead, which is a different object in a
/// different namespace. `POST /v1/bench` is the one exception: a create takes it in the body.
fn bench_url(c: &Ctx, path: &str, team: &str) -> String {
    api(c, &format!("/v1/bench{path}?team={team}"))
}

/// `env.intercept`, its nine neighbours and the three refusals, in journey order.
///
/// The refusals come FIRST on purpose: the "no environment" case clears the space for a moment,
/// which would release an intercept already in force, and the 422s need the workspace running.
/// Everything after that shares one team, one environment, one workspace and one listener, because
/// standing those up is the whole cost.
pub async fn run(c: &mut Ctx) {
    if c.suite != Suite::Hourly {
        return;
    }
    if c.kube.is_none() {
        return skip_all(c, "no kubeconfig");
    }
    let j = match stand_up(c).await {
        Ok(j) => j,
        Err(e) => return skip_all(c, &format!("{e:#}")),
    };
    space_bench(c, &j).await;
    refusals::refused(c, &j).await;
    refusals::tools_refused(c, &j).await;
    // Not a pass and not a hole: see `refusals::udp_refused`.
    refusals::udp_refused(c);
    let held = delivery::intercept(c, &j).await;
    delivery::proxy_up(c, &j, held).await;
    delivery::delivered(c, &j, held).await;
    delivery::remap(c, &j, held).await;
    let peer_ws = delivery::peer(c, &j, held).await;
    delivery::bench_reaches(c, &j, held).await;
    delivery::proxy_restart(c, &j, held).await;
    delivery::release(c, &j, held).await;
    delivery::fallback(c, &j, held).await;
    teardown(c, &j, peer_ws).await;
}

/// Every id skipped once with the same reason — a hole that reads as "the run could not get to
/// this" rather than as a green sample or as no sample at all.
fn skip_all(c: &mut Ctx, why: &str) {
    for id in INTERCEPT_IDS {
        c.skip(id, why);
    }
}

/// The reason every id downstream of the intercept skips with. One `const`, because a skip must
/// name the id exactly once and the sentence is the same wherever it is filed.
const NOT_HELD: &str = "the service was never intercepted";

// ── standing it up ────────────────────────────────────────────────────────────

/// The team, the environment, the workspace and the listener — everything the ids share, and none
/// of it measured: a create that takes two minutes is not what `env.intercept`'s 120 s is for.
async fn stand_up(c: &mut Ctx) -> Result<Journey> {
    let team = format!("{}-icept", c.prefix());
    make_team(c, &team).await?;
    let env = create_intercept_env(c, &team).await?;
    // A fresh environment's Volume carries the environment's own id, and teardown's prefix sweep
    // sees the environment but not the volume behind it.
    c.state.extra_volumes.push(env.clone());
    // BOTH owners follow it. The second is what makes `env.intercept.peer` a question at all; the
    // first is what lets the intercept be authorised in the first place.
    for jwt in [c.probe_jwt.clone(), c.other_jwt.clone()] {
        call(c, reqwest::Method::PUT, &api(c, &format!("/v1/me/environments/{team}")), &jwt, Some(serde_json::json!({ "environment": env })))
            .await
            .context("a space could not choose the intercept environment")?;
    }
    let ws = create_as(c, &c.probe_jwt.clone(), &format!("{}-iceptws", c.prefix()), &team).await?;
    // A fresh team has NO bench, and every other `/v1/bench` route answers 404 until one is made
    // (`my_bench` -> `found`): without this both bench ids failed every hour on "no bench".
    let bench = make_bench(c, &team).await.err().map(|e| format!("{e:#}"));
    let j = Journey { team, env, ws, bench };
    listen(c, &j.ws_ns(&c.probe_user), &j.ws).await?;
    Ok(j)
}

/// The run's own team, with the second probe owner in it.
///
/// There is no add-a-member route — `PATCH /v1/teams/{slug}/members/{email}` changes the role of
/// somebody who is already one and 404s otherwise — so joining is by invitation, exactly as
/// `experience_teams::membership` does it. Both tokens are this run's own, so the accept needs
/// nobody's mailbox.
async fn make_team(c: &mut Ctx, slug: &str) -> Result<()> {
    let body = serde_json::json!({ "slug": slug, "name": "kloudlite slo intercept", "region": c.cfg.region });
    post(c, &api(c, "/v1/teams"), &c.probe_jwt.clone(), body).await.context("could not create the intercept team")?;
    let invite = serde_json::json!({ "email": c.other_email.clone(), "role": "member" });
    let issued = post(c, &api(c, &format!("/v1/teams/{slug}/invites")), &c.probe_jwt.clone(), invite)
        .await
        .context("could not invite the second owner")?;
    let token = issued
        .get("token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow!("the invitation carried no token"))?
        .to_string();
    post(c, &api(c, &format!("/v1/invites/{token}/accept")), &c.other_jwt.clone(), Value::Null)
        .await
        .context("the second owner could not join the team")?;
    Ok(())
}

/// The team's bench, which nothing else creates. The team and the region go in the BODY here —
/// `create_bench` is the one bench route that reads them from one (`NewBench`) — and every call
/// after it names the team in the query instead.
async fn make_bench(c: &Ctx, team: &str) -> Result<()> {
    let body = serde_json::json!({ "team": team, "region": c.cfg.region });
    post(c, &api(c, "/v1/bench"), &c.probe_jwt, body).await.context("could not create the team's bench")?;
    Ok(())
}

/// Start (or restart) a workspace's listener and wait until it answers itself.
async fn listen(c: &Ctx, ns: &str, ws: &str) -> Result<()> {
    let (code, out, err) = ws_exec(c, ns, ws, LISTENER, WS_CEILING).await?;
    if code != 0 {
        return Err(anyhow!("the workspace listener never came up ({code}): {} {}", out.trim(), err.trim()));
    }
    Ok(())
}

/// One exec in a workspace pod of any namespace. `workspace::ws_exec` is the probe owner's
/// PERSONAL namespace alone; this journey's workspaces are a team's, and one of them is somebody
/// else's. As `kl`, for the same reason that one is: what the probe measures is what a person sees.
async fn ws_exec(c: &Ctx, ns: &str, id: &str, script: &str, cap: Duration) -> Result<(i32, String, String)> {
    let k = c.kube.as_ref().ok_or_else(|| anyhow!("no kubeconfig"))?;
    let user = k8s::SSH_USER;
    crate::kube::exec(k, ns, id, Some(super::workspace::WS_CONTAINER), &["su", user, "-s", "/bin/sh", "-c", script], cap).await
}

/// A workspace of this journey's TEAM, for whichever owner holds `jwt`.
async fn create_as(c: &Ctx, jwt: &str, name: &str, team: &str) -> Result<String> {
    let body = serde_json::json!({
        "name": name,
        "region": c.cfg.region,
        "quota_gb": QUOTA_GB,
        "packages": [],
        "team": team,
    });
    let doc = post(c, &api(c, "/v1/workspaces"), jwt, body).await.with_context(|| format!("could not create {name}"))?;
    let id = super::id_of(&doc)?;
    let url = api(c, &format!("/v1/workspaces/{id}"));
    poll_json(c, &url, jwt, WS_CEILING, |v| super::state_is(v, "ready")).await.with_context(|| format!("{name} never became ready"))?;
    Ok(id)
}

/// This journey's own environment, owned by the TEAM: `SERVICE` to dial FROM, `TARGET` to intercept.
async fn create_intercept_env(c: &Ctx, team: &str) -> Result<String> {
    let body = serde_json::json!({
        "name": format!("{}-icept", c.prefix()),
        "owner": team,
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
    let cap = super::environment::CREATE_CEILING;
    poll_json(c, &url, &c.probe_jwt, cap, |v| v.get("state").and_then(Value::as_str) == Some("running")).await?;
    sts_ready(c, &id, SERVICE, cap).await?;
    sts_ready(c, &id, TARGET, cap).await?;
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

/// A bare HTTP GET for the ENVIRONMENT's own pods, written for busybox `nc`: the service image is
/// alpine's and carries it.
fn http_get(host: &str, port: u16) -> String {
    format!(r"printf 'GET / HTTP/1.0\r\n\r\n' | nc -w 3 {host} {port}")
}

/// The same GET from a WORKSPACE, over bash's `/dev/tcp` under `timeout`. The workspace image is
/// debian-slim now and has no netcat at all; `bash` and `timeout` come from the nix profile's base
/// set, so they are there whatever the base image is (the same reason `controller::ping` moved).
pub(super) fn http_get_ws(host: &str, port: u16) -> String {
    format!(r#"timeout 5 bash -c 'exec 3<>/dev/tcp/{host}/{port} && printf "GET / HTTP/1.0\r\n\r\n" >&3 && cat <&3'"#)
}

/// A GET the BENCH can run: its image ships `node` and no curl, wget or nc (fleet, 2026-09-15).
/// Passed to `node -e` as one argv element, so no shell quoting is involved; the body goes to
/// stdout and anything else — refusal, timeout — prints nothing, which the marker check fails on.
fn node_get(url: &str) -> String {
    format!(
        "const r=require('http').get('{url}',{{timeout:5000}},s=>s.pipe(process.stdout));r.on('timeout',()=>r.destroy());r.on('error',e=>console.error(e.message))"
    )
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

/// The body `/v1` takes for an intercept. One shape, so a refusal and the real thing differ only
/// in the value being tested.
fn intercept_body(ws: &str, service_port: u16, ws_port: u16) -> Value {
    serde_json::json!({
        "service": TARGET,
        "workspace": ws,
        "ports": [{ "service": service_port, "workspace": ws_port }],
    })
}

// ── the bench ─────────────────────────────────────────────────────────────────

/// `env.space.bench`: the probe owner's bench in THIS TEAM is a pod of the same space, so the
/// choice reaches its `/etc/resolv.conf` too. Read as a file rather than a lookup: the bench image
/// is the harness's, and the platform mount is the thing under test. Woken first — an idle bench
/// has no pod.
async fn space_bench(c: &mut Ctx, j: &Journey) {
    if let Some(why) = &j.bench {
        return c.skip("env.space.bench", &why.clone());
    }
    let want = format!("{}.svc.", env_namespace(&j.env));
    let (team, ns) = (j.team.clone(), j.ws_ns(&c.probe_user));
    c.step("env.space.bench", SPACE_BENCH_CEILING, move |c| {
        async move {
            let session = bench_url(c, "/session", &team);
            let _ = raw(c, reqwest::Method::POST, &session, &c.probe_jwt.clone(), None, &[]).await;
            bench_ready(c, &team, Duration::from_secs(90)).await?;
            let start = std::time::Instant::now();
            loop {
                let out = bench_exec(c, &team, &ns, &["cat", "/etc/resolv.conf"]).await.unwrap_or_default();
                if out.contains(&want) {
                    return Ok(());
                }
                if start.elapsed() + SLACK >= SPACE_BENCH_CEILING {
                    return Err(anyhow!("the bench's resolv.conf never searched {want}: {:?}", out.lines().next().unwrap_or("")));
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
        .boxed()
    })
    .await;
}

/// Wait for the TEAM bench to report `ready`. `bench::wait_phase` reads the caller's PERSONAL
/// bench (`GET /v1/bench` with no team), which is a different object entirely.
async fn bench_ready(c: &Ctx, team: &str, cap: Duration) -> Result<()> {
    let url = bench_url(c, "", team);
    let start = std::time::Instant::now();
    loop {
        let phase = get(c, &url, &c.probe_jwt).await.ok().and_then(|d| d.get("phase").and_then(Value::as_str).map(str::to_string));
        if phase.as_deref() == Some("ready") {
            return Ok(());
        }
        if start.elapsed() >= cap {
            return Err(anyhow!("the team bench is {phase:?}, never \"ready\" within {} s", cap.as_secs()));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// One exec in the team bench's pod — the bench's own path, the one `env.space.bench` reads its
/// `resolv.conf` through. The pod is named by the bench's workspace id, so it is asked for
/// (`stages::bench_pod`) rather than assumed.
/// An argv, not a script: the bench image is the harness's, and a shell there is not a promise.
async fn bench_exec(c: &Ctx, team: &str, ns: &str, argv: &[&str]) -> Result<String> {
    let k = c.kube.as_ref().ok_or_else(|| anyhow!("no kubeconfig"))?;
    let pod = super::bench_pod(c, Some(team)).await?;
    let (_, out, _) = crate::kube::exec(k, ns, &pod, Some(k8s::BENCH_CONTAINER), argv, EXEC_CEILING).await?;
    Ok(out)
}

// ── reading the environment back ──────────────────────────────────────────────

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
///
/// A namespace that is GONE is clean and says so (`Ok(None)`); any other error is an unknown, never
/// an empty list, because "the API server said no" and "there is no proxy" are not the same answer.
async fn intercept_pods(c: &Ctx, ns: &str) -> Result<Option<Vec<String>>> {
    let k = c.kube.clone().ok_or_else(|| anyhow!("no kubeconfig"))?;
    let pods: kube::Api<Pod> = kube::Api::namespaced(k, ns);
    let lp = ListParams::default().labels(&format!("{}=intercept", k8s::KIND_LABEL));
    match pods.list(&lp).await {
        Ok(list) => Ok(Some(list.iter().map(ResourceExt::name_any).collect())),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(None),
        Err(e) => Err(anyhow!("could not list the pods of {ns}: {e}")),
    }
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
/// team, the environment and both workspaces by name anyway — but the order matters: the intercept
/// is DELETED FIRST, because a release is what takes the proxy down and an environment delete that
/// raced one has left a proxy pod running on the fleet before; and the team goes LAST, because
/// `delete_team` is refused while it still holds a workspace. The team's bench has no route of its
/// own to go by: `delete_members_now` asks the api to delete both owners' pairs now instead of after the
/// seven-day grace, and the namespace follows on a later keys beat.
///
/// Nothing is deleted by a name that is not `run-{id}`-prefixed, so a crashed run is swept by the
/// next one and a run can never delete another's live objects.
async fn teardown(c: &mut Ctx, j: &Journey, peer_ws: Option<String>) {
    let jwt = c.probe_jwt.clone();
    let drop = api(c, &format!("/v1/environments/{}/intercepts/{TARGET}", j.env));
    warn_on_err(c, reqwest::Method::DELETE, &drop, &jwt).await;
    // Both space choices: a space is named by its owner and its team, so no prefix sweep sees it.
    for who in [jwt.clone(), c.other_jwt.clone()] {
        warn_on_err(c, reqwest::Method::DELETE, &j.space(c), &who).await;
    }
    warn_on_err(c, reqwest::Method::DELETE, &api(c, &format!("/v1/workspaces/{}", j.ws)), &jwt).await;
    if let Some(id) = &peer_ws {
        warn_on_err(c, reqwest::Method::DELETE, &api(c, &format!("/v1/workspaces/{id}")), &c.other_jwt.clone()).await;
    }
    warn_on_err(c, reqwest::Method::DELETE, &api(c, &format!("/v1/environments/{}", j.env)), &jwt).await;
    // A team with an orphaned workspace is worse than a leaked team, so the drain is what decides:
    // on any doubt the team stays and the next run's `sweep_teams` takes it. BOTH owners' drains
    // are waited on — `/v1/workspaces?team=` lists only the CALLER's, so the probe owner's drain
    // says nothing about the follower's, and a best-effort delete that failed would otherwise take
    // the team with an orphan still in it.
    let drained = match super::drain_team(c, &j.team, &jwt).await {
        Ok(()) => peer_gone(c, peer_ws.as_deref()).await,
        Err(e) => Err(e),
    };
    match drained {
        Ok(()) => {
            warn_on_err(c, reqwest::Method::DELETE, &api(c, &format!("/v1/teams/{}", j.team)), &jwt).await;
            delete_members_now(c, &j.team).await;
        }
        Err(e) => tracing::warn!(kind = "team", op = "drain", name = %j.team, error = %format!("{e:#}"), "slo.teardown.failed"),
    }
    no_proxy_left(c, j).await;
}

/// Both probe owners' pairs in the run's now-deleted team, cleaned now rather than after the seven-day
/// grace — hourly run teams would otherwise pile up benches and namespaces. Only this run's owners
/// and team. Deletion still needs the region's `memberRemovalDeletes`; without it this only marks.
pub(crate) async fn delete_members_now(c: &Ctx, team: &str) {
    let admin = c.admin_jwt();
    for who in [c.probe_user.clone(), c.other_user.clone()] {
        let url = api(c, &format!("/v1/teams/{team}/members/{who}/delete-now"));
        match post(c, &url, &admin, serde_json::json!({ "person": who, "team": team })).await {
            Ok(v) if v["deletes_enabled"] == false => tracing::info!(kind = "team", name = %team, "slo.teardown.marked_only"),
            Ok(_) => {}
            Err(e) => tracing::warn!(kind = "team", op = "delete_now", name = %team, error = %format!("{e:#}"), "slo.teardown.failed"),
        }
    }
}

/// The follower's workspace is GONE, read as its own owner. `drain_team` lists
/// `/v1/workspaces?team={slug}` with the probe owner's token and that listing is caller-scoped, so
/// the second owner's workspace is invisible to it — a team deleted on the strength of that alone
/// would strand a subvolume under an owner that no longer resolves.
async fn peer_gone(c: &Ctx, id: Option<&str>) -> Result<()> {
    let Some(id) = id else { return Ok(()) };
    let url = api(c, &format!("/v1/workspaces/{id}"));
    let start = std::time::Instant::now();
    loop {
        let (status, _) = raw(c, reqwest::Method::GET, &url, &c.other_jwt, None, &[]).await?;
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        if start.elapsed() >= super::TEAM_DRAIN {
            return Err(anyhow!("the follower's workspace {id} is still there ({status})"));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Asserted, not hoped for: a proxy the deletes did not collect is a pod nobody is billed for and
/// no listing shows. Every namespace this journey could have rendered one into.
async fn no_proxy_left(c: &Ctx, j: &Journey) {
    let mut watch: Vec<String> = vec![env_namespace(&j.env)];
    watch.extend([j.ws_ns(&c.probe_user), j.ws_ns(&c.other_user)]);
    let start = std::time::Instant::now();
    loop {
        let mut pending: Vec<(String, String)> = Vec::new();
        for ns in &watch {
            // Polled, because the deletes above are WISHES: the proxy goes with the namespace, on
            // Kubernetes' own clock. A namespace that is gone is clean; an error is not.
            match intercept_pods(c, ns).await {
                Ok(None) => {}
                Ok(Some(pods)) => pending.extend(pods.into_iter().map(|p| (ns.clone(), p))),
                Err(e) => pending.push((ns.clone(), format!("unreadable: {e:#}"))),
            }
        }
        if pending.is_empty() {
            return;
        }
        if start.elapsed() >= TEARDOWN_CEILING {
            return tracing::warn!(kind = "intercept", left = ?pending, "slo.teardown.failed");
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
    use std::sync::{Arc, Mutex};

    /// The restart wait answers only for a replacement: the old uid, or a new pod not yet Running
    /// and Ready, keeps it waiting.
    #[test]
    fn only_a_new_ready_pod_is_restarted() {
        use k8s_openapi::api::core::v1::{PodCondition, PodStatus};
        let pod = |uid: &str, phase: &str, ready: &str| Pod {
            metadata: kube::api::ObjectMeta { uid: Some(uid.into()), ..Default::default() },
            status: Some(PodStatus {
                phase: Some(phase.into()),
                conditions: Some(vec![PodCondition { type_: "Ready".into(), status: ready.into(), ..Default::default() }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(!delivery::restarted(None, Some("a")));
        assert!(!delivery::restarted(Some(&pod("a", "Running", "True")), Some("a")));
        assert!(!delivery::restarted(Some(&pod("b", "Pending", "False")), Some("a")));
        assert!(!delivery::restarted(Some(&pod("b", "Running", "False")), Some("a")));
        assert!(delivery::restarted(Some(&pod("b", "Running", "True")), Some("a")));
    }

    /// The listener script and the constants the intercept is written against are one statement:
    /// a remapped port that the listener does not actually listen on would make every run fail
    /// with "never answered", pointing at the intercept rather than at this file.
    #[test]
    fn the_bench_dial_is_node_with_a_timeout_and_no_shell_client() {
        let js = node_get("http://svc.ns.svc.cluster.local:8080/");
        assert!(js.starts_with("const r=require('http').get('http://svc.ns.svc.cluster.local:8080/',"), "{js}");
        assert!(js.contains("timeout:5000") && js.contains("r.destroy()"), "{js}");
        for absent in ["curl", "wget", "nc ", "\"", "{{"] {
            assert!(!js.contains(absent), "{js}");
        }
    }

    #[test]
    fn the_listener_listens_on_the_port_the_intercept_maps_to() {
        assert!(LISTENER.contains(&format!(".listen({WS_PORT},")), "{LISTENER}");
        assert!(LISTENER.contains(&format!("127.0.0.1/{WS_PORT}")), "{LISTENER}");
        // Served and asserted on, both by name: the log file happens to carry the marker too.
        assert!(LISTENER.contains(&format!("{MARKER}\\n'")), "{LISTENER}");
        assert!(LISTENER.contains(&format!("grep -q {MARKER}")), "{LISTENER}");
        // The runtime this leans on is the image's, not a package: the workspace is created with
        // an empty package list, and an earlier `bun` here skipped every id on the fleet.
        assert!(!LISTENER.contains("bun"), "{LISTENER}");
        // And not netcat: the alpine workspace image had busybox's, debian-slim has none at all.
        assert!(!LISTENER.contains("nc -"), "{LISTENER}");
        // Run again after the pod is killed, so it must not stack a second copy on the port.
        assert!(LISTENER.contains("pkill"), "{LISTENER}");
        // A pattern that matches its own `sh -c` argv kills the exec running it.
        assert!(!LISTENER.contains("pkill -f"), "{LISTENER}");
        assert!(LISTENER.contains("pkill -x node"), "{LISTENER}");
        assert!(LISTENER.contains("echo $! > /tmp/slo-intercept.pid"), "{LISTENER}");
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

    /// Every `/v1/bench` call this journey makes names the team in the QUERY, and the recorder is
    /// what says so: the api reads it with `Query<TeamQuery>`, so a body `team` is silently ignored
    /// and the caller's PERSONAL bench answers — a different object, in a different namespace, in
    /// which nothing this journey did is visible. That was a real regression (fix round 2).
    #[tokio::test]
    async fn every_bench_call_names_the_team_in_the_query() {
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let rec = seen.clone();
        let app = axum::Router::new().fallback(axum::routing::any(move |uri: axum::http::Uri| {
            let rec = rec.clone();
            async move {
                rec.lock().expect("recorder").push(uri.to_string());
                axum::http::StatusCode::NOT_FOUND
            }
        }));
        let mut c = testkit::ctx_against(app).await;
        let team = "run-hourly-1-icept";
        // The create, which is the ONE call that carries the team in a body, and the two reads.
        let _ = make_bench(&c, team).await;
        let _ = raw(&c, reqwest::Method::POST, &bench_url(&c, "/session", team), &c.probe_jwt.clone(), None, &[]).await;
        c.retry_delay = Duration::from_millis(1);
        let _ = bench_ready(&c, team, Duration::from_millis(1)).await;
        let calls = seen.lock().expect("recorder").clone();
        let bench: Vec<&String> = calls.iter().filter(|u| u.starts_with("/v1/bench")).collect();
        assert!(bench.len() >= 3, "{calls:?}");
        for u in &bench {
            // The create is `/v1/bench` with no query; every other call must name the team.
            let is_create = u.as_str() == "/v1/bench";
            assert!(is_create || u.contains(&format!("?team={team}")), "{u} does not name the team in its query");
        }
        assert!(bench.iter().any(|u| u.as_str() == format!("/v1/bench/session?team={team}")), "{calls:?}");
        assert!(bench.iter().any(|u| u.as_str() == format!("/v1/bench?team={team}")), "{calls:?}");
    }

    /// The journey is a TEAM's, and every namespace it touches follows from that: two people in one
    /// team have two namespaces, and neither is the personal one the fast journey uses.
    #[test]
    fn both_owners_workspaces_live_in_the_teams_namespaces() {
        let j = Journey { team: "run-hourly-1-icept".into(), env: "env-1".into(), ws: "ws-1".into(), bench: None };
        let (mine, theirs) = (j.ws_ns("slo-hourly"), j.ws_ns("slo-hourly-other"));
        assert_ne!(mine, theirs);
        assert!(mine.starts_with("wt-"), "{mine}");
        assert_ne!(mine, ws_namespace("slo-hourly", ""));
        // And the team's own name is what teardown's prefix sweep sees.
        assert!(j.team.starts_with("run-"), "{}", j.team);
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
