//! Stage 6 · Environment: one environment with one service, and the attachment that makes it
//! reachable from a workspace by bare name.
//!
//! Worst case 290 s if every step times out (120 + 20 + 20 + 20 + 90); see `workspace.rs`'s note
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
const AFTER_CREATE: [&str; 4] = ["env.dns", "env.attach", "env.detach", "env.push.p95"];

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
    dns(c, &env).await;
    attach(c, &env).await;
    push(c, &env).await;
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
            .await
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
                false => Err(anyhow!("`{SERVICE}` does not resolve inside the environment")),
            }
        }
        .boxed()
    })
    .await;
}

/// Whether `redis` resolves from the environment's own service pod.
///
/// `getent` first, `nslookup` as the fallback: alpine has both, from musl and from busybox, and
/// which one a base image ships has changed under us before.
async fn resolves(c: &Ctx, env: &str, cap: Duration) -> Result<bool> {
    let k = c.kube.as_ref().ok_or_else(|| anyhow!("no kubeconfig"))?;
    let ns = kloudlite_git_workspaces::crd::env_namespace(env);
    // `{service}-0`: one StatefulSet per service, one replica, so the ordinal is always zero.
    let pod = format!("{SERVICE}-0");
    let script = format!("getent hosts {SERVICE} || nslookup {SERVICE}");
    let (code, _, _) = crate::kube::exec(k, &ns, &pod, None, &["sh", "-c", &script], cap).await?;
    Ok(code == 0)
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
            let doc = post(c, &url, &jwt, Value::Null).await.context("could not push")?;
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
