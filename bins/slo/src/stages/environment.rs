//! Stage 6 · Environment: one environment with one service, and the space choice that makes it
//! reachable by bare name from every workspace (and the bench) of the probe owner's space.
//!
//! Worst case 420 s if every step times out (120 + 20 + 20 + 20 + 90 + 30 + 120); see
//! `workspace.rs`'s note
//! on how the three stages' sums sit against the fast suite's 900 s deadline.
//!
//! The intercept journey this stage also walks on an hourly run is `env_intercept.rs`: it stands
//! up its own environment and workspace and shares only this file's environment SHAPE.
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

pub(super) const CREATE_CEILING: Duration = Duration::from_secs(120);
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
pub(super) const EXEC_CEILING: Duration = Duration::from_secs(10);

/// The one service. `redis:7-alpine` because it is small, starts in a second and answers on a port
/// — the journey needs a name to resolve, not a database to use.
pub(super) const SERVICE: &str = "redis";
pub(super) const IMAGE: &str = "redis:7-alpine";
pub(super) const PORT: u16 = 6379;

pub(super) const QUOTA_GB: u64 = 1;

/// Every id after the create, in journey order.
const AFTER_CREATE: [&str; 7] =
    ["env.exec.ok", "env.dns", "env.attach", "env.space.live", "env.detach", "env.push.p95", "env.clone.p95"];

/// The probe owner's personal space: every probe workspace is personal, so its team is the handle.
pub(super) fn my_space(c: &Ctx) -> String {
    api(c, &format!("/v1/me/environments/{}", c.probe_user))
}

pub async fn run(c: &mut Ctx) {
    // Each half only where this pod walks it: the intercept journey is its own hourly group.
    if c.walks("env.create.p95") {
        fast(c).await;
    }
    // Its OWN environment and workspace, so it neither depends on the fast journey's having
    // worked nor leaves the one stage 7 stops and starts in a state stage 7 did not ask for.
    if c.walks("env.intercept") {
        super::env_intercept::run(c).await;
    }
    // Stands nothing up of its own — it only asks about whatever the owner's builder already
    // is, which is why it costs nothing to also gate hourly-only alongside the intercepts.
    if c.walks("builder.hidden") {
        builder_hidden(c).await;
    }
    // Catalogued under `5 · Workspace` with its sibling, walked here: `ws.kl.env.switch` is the
    // first id that needs BOTH the run's workspace and its environment, and stage 5 has only one
    // of the two. Skipped by name rather than left unfiled — a missing id reads as passed.
    if c.walks(super::workspace::KL_ENV_ID) {
        match c.state.environment.clone() {
            Some(env) => super::workspace::kl_env_switch(c, &env).await,
            None if c.suite == Suite::Hourly => c.skip(super::workspace::KL_ENV_ID, "no environment"),
            None => {}
        }
    }
}

async fn fast(c: &mut Ctx) {
    let no_kube = c.kube.is_none();
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
    if no_kube {
        c.demote_to_skip("env.create.p95", NO_STS);
    }
    exec_ok(c, &env).await;
    dns(c, &env).await;
    attach(c, &env).await;
    push(c, &env).await;
    clone(c, &env).await;
    if no_kube {
        c.demote_to_skip("env.clone.p95", NO_STS);
    }
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
                // STDOUT is the assertion — the command echoes a word and the word is what is
                // compared — so a mismatch that named only stderr left a reader with nothing to
                // look at (2026-09-12).
                return Err(anyhow!("exec exited {code} with stdout {:?}, wanted \"slo\": {}", out.trim(), err.trim()));
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

/// The skip a step whose "with its services ready" half could not be attempted is demoted to.
/// The record half still ran; what is refused is calling that a kept promise (2026-09-12).
const NO_STS: &str = "no kubeconfig: the service's replica could not be confirmed ready";

/// The same for one named service — the intercept journey's environment has two.
pub(super) async fn sts_ready(c: &Ctx, env: &str, svc: &str, cap: Duration) -> Result<()> {
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

/// `env.attach`, `env.space.live` and `env.detach`: the SPACE's choice takes effect, and stops
/// having effect, INSIDE the workspace pods — a `/etc/resolv.conf` the agent renders in place, which
/// is what makes all three work without restarting a pod.
///
/// One function because they are one experiment: clearing proves nothing unless the same lookup
/// resolved a moment earlier. `env.space.live` asks the run's CLONE, a second workspace of the same
/// space that was already running before the choice was made.
async fn attach(c: &mut Ctx, env: &str) {
    let (Some(ws), true) = (c.state.workspace.clone(), c.kube.is_some()) else {
        let why = if c.kube.is_none() { "no kubeconfig" } else { "no workspace" };
        for id in ["env.attach", "env.space.live", "env.detach"] {
            c.skip(id, why);
        }
        return skip_clone_attach(c, why);
    };
    let (e, w) = (env.to_string(), ws.clone());
    let attached = c
        .step("env.attach", ATTACH_CEILING, move |c| {
            let jwt = c.probe_jwt.clone();
            let url = my_space(c);
            let body = serde_json::json!({ "environment": e });
            async move {
                call(c, reqwest::Method::PUT, &url, &jwt, Some(body)).await.context("could not choose the environment")?;
                until(c, &w, true, ATTACH_CEILING).await
            }
            .boxed()
        })
        .await;
    if !attached {
        // The failure was counted where it happened: a clear that was never a choice measures
        // nothing about clearing.
        c.skip("env.space.live", "the space never chose the environment");
        c.skip("env.detach", "the space never chose the environment");
        return skip_clone_attach(c, "the space never chose the environment");
    }
    match c.state.clone.clone() {
        Some(other) => {
            c.step("env.space.live", ATTACH_CEILING, move |c| async move { until(c, &other, true, ATTACH_CEILING).await }.boxed()).await;
        }
        None => c.skip("env.space.live", "no second workspace in the space"),
    }
    clone_attach_survives(c, env).await;
    c.step("env.detach", ATTACH_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = my_space(c);
        async move {
            call(c, reqwest::Method::DELETE, &url, &jwt, None).await.context("could not clear the environment")?;
            until(c, &ws, false, ATTACH_CEILING).await
        }
        .boxed()
    })
    .await;
}

/// The hourly id `attach` owns beside its three fast ones. `const`, not a literal in four places:
/// the skip paths below must name exactly the id the step files, or a run reports it twice.
const CLONE_ATTACH_ID: &str = "ws.clone.attach.survives";

/// Skipped only on an HOURLY run — a fast run files no sample for this id at all, the same gate
/// `intercepts` uses.
fn skip_clone_attach(c: &mut Ctx, why: &str) {
    if c.suite == Suite::Hourly {
        c.skip(CLONE_ATTACH_ID, why);
    }
}

/// `ws.clone.attach.survives`: the CLONE keeps the attach file the janitor once took from it.
///
/// A clone is a second worktree of its SOURCE's volume, so no `{pool}/vol/{clone}` is ever created
/// — and the janitor's keep-set used to be that directory listing, which read a running clone as
/// garbage and `remove_dir_all`ed `{pool}/attach/{clone}` an hour after it was created. The
/// `resolv.conf` the pod holds open by inode is what went with it, so the clone silently lost the
/// environment's names until its next reconcile.
///
/// The probe cannot wait out the sweep's 1 h age floor inside an hourly run, so it judges what the
/// sweep would destroy, from inside the clone: `/etc/resolv.conf` names the environment's namespace
/// AND the service resolves through it. Judged on the OUTPUT of both, never an exit code — an exec
/// that cannot read the file exits zero on some shells.
///
/// The other half the owner asked for — that no `janitor.attach.reclaimed` line names this clone
/// over the run — is NOT probed: those are tracing logs in ClickStack and the probe holds no
/// ClickStack credential (the same reason `edge.rs` asks the admin process rather than ClickHouse).
/// Query that line by id in ClickStack instead; a hit for an id this step passed for is the
/// regression.
async fn clone_attach_survives(c: &mut Ctx, env: &str) {
    if c.suite != Suite::Hourly {
        return;
    }
    let Some(clone) = c.state.clone.clone() else {
        return c.skip(CLONE_ATTACH_ID, "no second workspace in the space");
    };
    let ns = kloudlite_workspaces::crd::env_namespace(env);
    c.step(CLONE_ATTACH_ID, ATTACH_CEILING, move |c| {
        let (clone, ns) = (clone.clone(), ns.clone());
        async move {
            // `cat`, not `getent` alone: the mounted FILE is what the sweep deletes, and a resolver
            // that still answers from a cached inode would hide its absence for the life of the pod.
            let script = format!("cat /etc/resolv.conf; getent hosts {SERVICE} || nslookup {SERVICE}");
            let (_, out, err) = super::workspace::ws_exec(c, &clone, &script, EXEC_CEILING).await?;
            if !out.contains(&ns) {
                return Err(anyhow!("the clone's /etc/resolv.conf never named {ns}: {:?} {}", out, err.trim()));
            }
            if !out.contains(SERVICE) {
                return Err(anyhow!("`{SERVICE}` did not resolve inside the clone: {:?} {}", out, err.trim()));
            }
            Ok(())
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

/// `builder.hidden`, hourly only: the probe owner's builder Environment (`bld-{owner}`) is never
/// listed and its id answers 404 everywhere a person could otherwise reach it.
///
/// One step, five requests: the list omission and four verbs, because a builder that fails one of
/// the five and passes the rest is exactly as reachable as one that fails none — `spec.system`
/// hides it from the list AND from every id-addressed route, one property, not five to keep in
/// step with each other.
const BUILDER_CEILING: Duration = Duration::from_secs(30);

async fn builder_hidden(c: &mut Ctx) {
    if c.suite != Suite::Hourly {
        return;
    }
    let id = format!("bld-{}", c.probe_user);
    // The POSITIVE control (2026-09-12): five 404s prove nothing on their own — a route that was
    // unmounted, a base URL that was wrong or a token the tier rejected would answer exactly the
    // same way, and the id would report the builder hidden by a hole. The run's own environment is
    // read through the SAME route first, and it must be there.
    let visible = c.state.environment.clone();
    c.step("builder.hidden", BUILDER_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let id = id.clone();
        async move {
            if let Some(env) = &visible {
                let (status, text) = raw(c, reqwest::Method::GET, &api(c, &format!("/v1/environments/{env}")), &jwt, None, &[]).await?;
                if !status.is_success() {
                    return Err(anyhow!("the control read of {env} answered {status}, so the 404s below would say nothing: {}", text.chars().take(200).collect::<String>()));
                }
            }
            let listed = get(c, &api(c, "/v1/environments"), &jwt).await.context("could not list environments")?;
            if listed.as_array().is_some_and(|rows| {
                rows.iter().any(|r| r.get("id").and_then(Value::as_str) == Some(id.as_str()))
            }) {
                return Err(anyhow!("the builder is listed in GET /v1/environments"));
            }
            for (method, path) in [
                (reqwest::Method::GET, format!("/v1/environments/{id}")),
                (reqwest::Method::POST, format!("/v1/environments/{id}/start")),
                (reqwest::Method::POST, format!("/v1/environments/{id}/push")),
                (reqwest::Method::GET, format!("/v1/volumes/{id}/history")),
            ] {
                let (status, text) = raw(c, method.clone(), &api(c, &path), &jwt, None, &[]).await?;
                if status != reqwest::StatusCode::NOT_FOUND {
                    return Err(anyhow!("{method} {path} answered {status}, not 404: {}", text.chars().take(200).collect::<String>()));
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

    /// `builder.hidden` is the hourly suite's alone, the same shape as the intercept ids: a fast
    /// run files no sample for it, and an hourly run reports it exactly once whatever the fleet
    /// answers.
    #[tokio::test]
    async fn builder_hidden_is_hourly_only() {
        let mut c = testkit::ctx().await;
        builder_hidden(&mut c).await;
        assert!(!c.steps.iter().any(|s| s.slo_id == "builder.hidden"), "a fast run reported builder.hidden");

        let mut c = testkit::ctx().await;
        c.suite = Suite::Hourly;
        builder_hidden(&mut c).await;
        let rows: Vec<_> = c.steps.iter().filter(|s| s.slo_id == "builder.hidden").collect();
        assert_eq!(rows.len(), 1, "builder.hidden was not reported exactly once");
    }

    /// `ws.clone.attach.survives` is hourly too, and — unlike the intercept ids — it lives inside
    /// a FAST function, so the gate that keeps it out of a fast run is the one worth pinning.
    #[tokio::test]
    async fn the_clone_attach_id_belongs_to_the_hourly_suite_only() {
        let mut c = testkit::ctx().await;
        c.kube = None;
        attach(&mut c, "env-x").await;
        assert!(!c.steps.iter().any(|s| s.slo_id == CLONE_ATTACH_ID), "a fast run reported an hourly id");

        let mut c = testkit::ctx().await;
        c.kube = None;
        c.suite = Suite::Hourly;
        attach(&mut c, "env-x").await;
        let rows: Vec<_> = c.steps.iter().filter(|s| s.slo_id == CLONE_ATTACH_ID).collect();
        assert_eq!(rows.len(), 1, "{CLONE_ATTACH_ID} was not reported exactly once");
        assert!(rows[0].skipped && rows[0].detail == "no kubeconfig", "{:?}", rows[0]);
    }

    /// The gate above and the catalogue's own `walks()` are two statements of one rule.
    #[test]
    fn the_catalogue_walks_the_clone_attach_id_in_exactly_the_hourly_journey() {
        for suite in [Suite::Fast, Suite::Hourly, Suite::Weekly, Suite::Monthly] {
            let ids: Vec<&str> = kloudlite_workspaces::slo::catalogue::journey(suite)
                .into_iter()
                .flat_map(|(_, ids)| ids)
                .collect();
            assert_eq!(ids.contains(&CLONE_ATTACH_ID), suite == Suite::Hourly, "{CLONE_ATTACH_ID} in {suite:?}'s journey disagrees with the stage's own gate");
        }
    }
}
