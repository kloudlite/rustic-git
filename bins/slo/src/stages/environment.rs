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

use super::{api, poll_json, post};
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
async fn service_ready(c: &Ctx, env: &str, cap: Duration) -> Result<()> {
    let Some(k) = c.kube.as_ref() else { return Ok(()) };
    let ns = kloudlite_workspaces::crd::env_namespace(env);
    let sts: kube::Api<k8s_openapi::api::apps::v1::StatefulSet> = kube::Api::namespaced(k.clone(), &ns);
    let start = std::time::Instant::now();
    loop {
        let ready = sts.get(SERVICE).await.ok().and_then(|s| s.status).and_then(|st| st.ready_replicas).unwrap_or(0);
        if ready >= 1 {
            return Ok(());
        }
        if start.elapsed() >= cap {
            return Err(anyhow!("{SERVICE}'s pod never became ready"));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// `env.create.p95`: the create and the wait for `ready` — same reason `ws.create.p95` waits.
async fn create(c: &mut Ctx) -> bool {
    let name = format!("{}-env", c.prefix());
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
    c.step("env.create.p95", CREATE_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, "/v1/environments");
        async move {
            let doc = post(c, &url, &jwt, body).await.context("could not create the environment")?;
            let id = doc
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("the answer carried no environment id"))?
                .to_string();
            c.state.environment = Some(id.clone());
            let env = api(c, &format!("/v1/environments/{id}"));
            poll_json(c, &env, &jwt, CREATE_CEILING, |v| {
                v.get("state").and_then(Value::as_str) == Some("running")
            })
            .await?;
            // `running` is the environment's own word; the service pod behind it is what `env.dns`
            // execs into, so the create is not done until that StatefulSet reports a ready replica.
            service_ready(c, &id, CREATE_CEILING).await
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
            match resolves(c, &env, DNS_CEILING).await? {
                true => Ok(()),
                false => Err(anyhow!("`{SERVICE}` does not resolve and answer inside the environment")),
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
