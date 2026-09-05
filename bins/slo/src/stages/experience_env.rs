//! Stage 14's environment half: the verbs a person performs on a multi-service environment, plus
//! the two reads that say what the platform thinks it is holding.
//!
//! A sibling of `experience.rs` rather than more of it: the stage is one catalogue and five
//! implementers, and one file per group of ids is what keeps them from editing the same lines.
//!
//! The four environment ids are ONE experiment on ONE environment, in journey order — an
//! environment that never came up cannot be cloned, and a restore that half-ran leaves nothing
//! meaningful to stop and start. So they chain: the first dependent id fails with the reason and
//! the rest skip.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::Pod;
use serde_json::Value;

use super::{api, get, poll_json, post};
use crate::ctx::Ctx;

/// Per-step ceilings, each at or above its catalogue target — a slow answer must be a breach with a
/// number, never a step the probe cut off. The three without a latency target still need one, or a
/// wedged environment holds the whole hourly run to the CronJob deadline.
const MULTI_CEILING: Duration = Duration::from_secs(180);
const CLONE_CEILING: Duration = Duration::from_secs(180);
const RESTORE_CEILING: Duration = Duration::from_secs(180);
const STOP_START_CEILING: Duration = Duration::from_secs(150);
/// The catalogue asks for 1 s. The ceiling is five, so a slow read is a breach with a duration
/// rather than a timeout with none.
const HISTORY_CEILING: Duration = Duration::from_secs(5);
const QUOTA_CEILING: Duration = Duration::from_secs(20);
/// One command inside a pod. A single exec that needs longer than this is a wedged API server.
const EXEC_CEILING: Duration = Duration::from_secs(15);
/// How long the vol.history workspace has to come up. Its own step measures nothing but the two
/// reads at the end, so this is a precondition ceiling, not an SLO.
const WS_CEILING: Duration = Duration::from_secs(120);
/// The whole preparation — a workspace and two pushes waited out one after the other. Every part
/// of it has a cap of its own, but nothing bounded the SUM until this did, and an unbounded
/// precondition is a stage that runs until the CronJob's deadline kills the pod.
const PREPARE_CEILING: Duration = Duration::from_secs(WS_CEILING.as_secs() * 3);
const QUOTA_GB: u64 = 1;

/// The two services. `redis` holds the marker `env.restore.inplace` puts back — on a MOUNT, because
/// only what lives on the environment's own subvolume is what a snapshot captures — and `web` is a
/// second, differently-imaged service whose only job is to be a name the first one can resolve.
const REDIS: &str = "redis";
const WEB: &str = "web";
const SERVICES: [&str; 2] = [REDIS, WEB];

/// Every environment id after the create, in journey order.
const AFTER_MULTI: [&str; 3] = ["env.clone", "env.restore.inplace", "env.stop.start"];

/// The four environment ids, walked as one journey on one environment.
pub async fn environments(c: &mut Ctx) {
    // Every step here ends in a pod: two StatefulSets ready, a resolver answer, a `redis-cli` round
    // trip. Without a kubeconfig none of them means anything, and a missing kubeconfig is a
    // deployment gap rather than an SLO breach.
    if c.kube.is_none() {
        c.skip("env.services.multi", "no kubeconfig");
        for id in AFTER_MULTI {
            c.skip(id, "no kubeconfig");
        }
        return;
    }
    let Some(env) = multi(c).await else {
        for id in AFTER_MULTI {
            c.skip(id, "the environment never came up");
        }
        return;
    };
    // The chain, all the way down: `env.clone` leaves the environment STOPPED and the restore's
    // first act is to start it again, so a clone that failed leaves a state the restore would
    // measure instead of the restore.
    if !clone(c, &env).await {
        c.skip("env.restore.inplace", "the clone left the environment mid-flight");
        c.skip("env.stop.start", "the clone left the environment mid-flight");
        return;
    }
    // A restore that failed leaves the environment mid-flight — services scaled down, a wish
    // written — and stopping THAT measures the restore, not the stop.
    if restore_in_place(c, &env).await {
        stop_start(c, &env).await;
    } else {
        c.skip("env.stop.start", "the environment was left mid-restore");
    }
}

/// `env.services.multi`: two services, both ready, and one resolving the other by bare name.
async fn multi(c: &mut Ctx) -> Option<String> {
    let name = format!("{}-e2", c.prefix());
    let body = serde_json::json!({
        "name": name,
        "region": c.cfg.region,
        "quota_gb": QUOTA_GB,
        "services": [{
            "name": REDIS,
            "image": "redis:7-alpine",
            "command": [],
            "env": {},
            // The one thing on the environment's own subvolume, and so the only thing a snapshot
            // holds: redis's `/data` is where its RDB lands, and `env.restore.inplace` is that file
            // coming back.
            "mounts": [{ "folder": "redis", "path": "/data" }],
            "ports": [6379],
        }, {
            "name": WEB,
            "image": "alpine:3.20",
            // BusyBox `sleep` takes a number, not `infinity` — a container that exits immediately
            // is a CrashLoopBackOff this step would report as the fleet being slow.
            "command": ["sleep", "86400"],
            "env": {},
            "mounts": [],
            // A service with no declared port gets no ClusterIP and no DNS name at all
            // (`k8s::service_clusterip`), so the port is what makes `web` resolvable. Nothing
            // listens on it; the record is the point.
            "ports": [8080],
        }],
    });
    let ok = c
        .step("env.services.multi", MULTI_CEILING, move |c| {
            let jwt = c.probe_jwt.clone();
            let url = api(c, "/v1/environments");
            async move {
                let doc = post(c, &url, &jwt, body).await.context("could not create the environment")?;
                let id = id_of(&doc)?;
                // Before the wait: an environment that never became ready still exists, and
                // teardown needs its volume whatever happened here.
                c.state.env_multi = Some(id.clone());
                c.state.extra_volumes.push(id.clone());
                running(c, &id, MULTI_CEILING).await?;
                // Asked from `web`, about `redis`: a sibling resolving a DIFFERENT service is the
                // whole claim, and a service resolving itself would pass with one StatefulSet.
                let (code, _, err) = svc_exec(c, &id, WEB, &format!("getent hosts {REDIS}"), EXEC_CEILING).await?;
                if code != 0 {
                    return Err(anyhow!("`{REDIS}` does not resolve from `{WEB}`: {}", err.trim()));
                }
                Ok(())
            }
            .boxed()
        })
        .await;
    ok.then(|| c.state.env_multi.clone()).flatten()
}

/// `env.clone`: a STOPPED environment copies, and every service comes up in the copy.
///
/// Stopped first because that is the shape a person clones in — and because an environment clone
/// copies the source's live subvolume, so a stopped source is the one whose bytes are not moving
/// under the copy.
async fn clone(c: &mut Ctx, env: &str) -> bool {
    let name = format!("{}-e2c", c.prefix());
    let env = env.to_string();
    c.step("env.clone", CLONE_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let stop = api(c, &format!("/v1/environments/{env}/stop"));
        let clone = api(c, &format!("/v1/environments/{env}/clone"));
        let read = api(c, &format!("/v1/environments/{env}"));
        async move {
            post(c, &stop, &jwt, Value::Null).await.context("could not stop the environment")?;
            poll_json(c, &read, &jwt, CLONE_CEILING, |v| state_is(v, "stopped"))
                .await
                .context("the environment never stopped")?;
            let doc = post(c, &clone, &jwt, serde_json::json!({ "name": name })).await.context("could not clone")?;
            let id = id_of(&doc)?;
            c.state.env_clone = Some(id.clone());
            c.state.extra_volumes.push(id.clone());
            running(c, &id, CLONE_CEILING).await
        }
        .boxed()
    })
    .await
}

/// `env.restore.inplace`: a value written into redis, pushed, deleted, and put back by a restore of
/// this environment's own volume.
///
/// The start is part of this step rather than a precondition of its own: `env.clone` left the
/// environment stopped, and a marker cannot be written into a service that is not running.
async fn restore_in_place(c: &mut Ctx, env: &str) -> bool {
    let (env, want) = (env.to_string(), c.run_id.clone());
    c.step("env.restore.inplace", RESTORE_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let start = api(c, &format!("/v1/environments/{env}/start"));
        let push = api(c, &format!("/v1/environments/{env}/push"));
        let restore = api(c, &format!("/v1/environments/{env}/restore-in-place"));
        let history = api(c, &format!("/v1/volumes/{env}/history"));
        async move {
            post(c, &start, &jwt, Value::Null).await.context("could not start the environment")?;
            running(c, &env, RESTORE_CEILING).await?;
            // `save` is load-bearing: a snapshot is of the DISK, and redis holds a `SET` in memory
            // until it decides to write an RDB. Without it this step restores an empty dataset and
            // calls the restore broken.
            let set = format!("redis-cli set slo {want} && redis-cli save");
            expect_exec(c, &env, &set, "could not write the marker").await?;
            let doc = post(c, &push, &jwt, serde_json::json!({ "message": "restore" }))
                .await
                .context("could not push")?;
            let snap = id_of(&doc)?;
            poll_json(c, &history, &jwt, RESTORE_CEILING, |v| super::workspace::row_ready(v, &snap))
                .await
                .context("the snapshot never turned ready")?;
            expect_exec(c, &env, "redis-cli del slo", "could not delete the marker").await?;
            post(c, &restore, &jwt, serde_json::json!({ "snapshot_id": snap }))
                .await
                .context("could not restore in place")?;
            // The services are scaled down and back up under a restore, so an exec that FAILS is
            // "not yet", never "the value is gone" — reading a dead pod as a verdict would fail
            // this step on every restore that worked.
            let deadline = std::time::Instant::now() + RESTORE_CEILING - Duration::from_secs(5);
            loop {
                if let Ok((0, out, _)) = svc_exec(c, &env, REDIS, "redis-cli get slo", EXEC_CEILING).await {
                    if out.trim() == want {
                        return Ok(());
                    }
                }
                if std::time::Instant::now() >= deadline {
                    return Err(anyhow!("the marker never came back after the restore"));
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        .boxed()
    })
    .await
}

/// `env.stop.start`: the round trip a person does every evening — the pods go, and they come back.
///
/// "Pods gone" rather than "state is stopped": the state is the record, and an environment that
/// reports `stopped` with its StatefulSets still running is exactly the bug worth catching.
async fn stop_start(c: &mut Ctx, env: &str) {
    let env = env.to_string();
    c.step("env.stop.start", STOP_START_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let stop = api(c, &format!("/v1/environments/{env}/stop"));
        let start = api(c, &format!("/v1/environments/{env}/start"));
        async move {
            post(c, &stop, &jwt, Value::Null).await.context("could not stop")?;
            pods_gone(c, &env, STOP_START_CEILING).await?;
            post(c, &start, &jwt, Value::Null).await.context("could not start")?;
            running(c, &env, STOP_START_CEILING).await
        }
        .boxed()
    })
    .await;
}

/// `vol.history`: two pushes with messages, read back newest first, and `refs` naming the tip.
///
/// The pushes are a PRECONDITION, outside the step but under `PREPARE_CEILING`: the catalogue's
/// target is one second, which is a history read, not two snapshots being cut.
pub async fn history(c: &mut Ctx) {
    // Under a ceiling of its own, and reported as `vol.history` when it fails: a workspace that
    // never comes up would otherwise hold the hourly run for as long as it took the CronJob's
    // deadline to notice, and the id would report nothing at all. A skip is the wrong answer too
    // — the fleet WAS asked, and the pushes the read needs are what failed.
    let prepared = match tokio::time::timeout(PREPARE_CEILING, prepare(c)).await {
        Ok(out) => out,
        Err(_) => Err(anyhow!("the two pushes did not happen within {} ms", PREPARE_CEILING.as_millis())),
    };
    let (volume, tip) = match prepared {
        Ok(v) => v,
        Err(e) => {
            let why = format!("{e:#}");
            c.step("vol.history", HISTORY_CEILING, move |_| async move { Err(anyhow!("{why}")) }.boxed()).await;
            return;
        }
    };
    c.step("vol.history", HISTORY_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let hist = api(c, &format!("/v1/volumes/{volume}/history"));
        let refs = api(c, &format!("/v1/volumes/{volume}/refs"));
        async move {
            let rows = get(c, &hist, &jwt).await.context("could not read the history")?;
            let main = get(c, &refs, &jwt).await.context("could not read the refs")?;
            reads_back(&rows, &main, &tip)
        }
        .boxed()
    })
    .await;
}

/// The workspace and the two pushes `vol.history` reads. Answers `(volume, newest snapshot)`.
///
/// Its own workspace rather than stage 5's: this needs two pushes with KNOWN messages in a known
/// order, and a volume another stage also pushes to has neither.
async fn prepare(c: &mut Ctx) -> Result<(String, String)> {
    let name = format!("{}-v", c.prefix());
    let jwt = c.probe_jwt.clone();
    let url = api(c, "/v1/workspaces");
    let body = serde_json::json!({
        "name": name,
        "region": c.cfg.region,
        "quota_gb": QUOTA_GB,
        "packages": ["bash"],
    });
    let doc = post(c, &url, &jwt, body).await.context("could not create the history workspace")?;
    let id = id_of(&doc)?;
    c.state.history_workspace = Some(id.clone());
    // A fresh workspace's Volume is named after the workspace itself.
    c.state.extra_volumes.push(id.clone());
    let read = api(c, &format!("/v1/workspaces/{id}"));
    poll_json(c, &read, &jwt, WS_CEILING, |v| state_is(v, "ready"))
        .await
        .context("the history workspace never became ready")?;
    let mut tip = String::new();
    // In order, and each waited out: two pushes racing would both claim the same parent, and the
    // second is refused while the first is still `Working`.
    for msg in ["one", "two"] {
        tip = push_once(c, &id, msg).await.with_context(|| format!("the {msg:?} push"))?;
    }
    Ok((id, tip))
}

/// One push with a message, waited to `ready`. Answers the snapshot id.
async fn push_once(c: &Ctx, ws: &str, message: &str) -> Result<String> {
    let jwt = c.probe_jwt.clone();
    let url = api(c, &format!("/v1/workspaces/{ws}/push"));
    let history = api(c, &format!("/v1/volumes/{ws}/history"));
    let doc = post(c, &url, &jwt, serde_json::json!({ "message": message })).await.context("could not push")?;
    let snap = id_of(&doc)?;
    poll_json(c, &history, &jwt, WS_CEILING, |v| super::workspace::row_ready(v, &snap))
        .await
        .context("the snapshot never turned ready")?;
    Ok(snap)
}

/// What the two reads have to say. A function of its own so the assertion is testable without a
/// fleet behind it — it is the whole meaning of the id.
fn reads_back(rows: &Value, refs: &Value, tip: &str) -> Result<()> {
    let rows = rows.as_array().ok_or_else(|| anyhow!("the history is not a list"))?;
    let messages: Vec<&str> = rows.iter().filter_map(|r| r.get("message").and_then(Value::as_str)).collect();
    // Newest first, which is the one thing about this listing a consumer cannot re-derive.
    if messages.first() != Some(&"two") || messages.get(1) != Some(&"one") {
        return Err(anyhow!("the history is not newest-first with both messages: {messages:?}"));
    }
    match refs.get("main").and_then(Value::as_str) {
        Some(m) if m == tip => Ok(()),
        other => Err(anyhow!("`main` is {other:?}, not the newest push")),
    }
}

/// `quota.view`: the live counts are at least what this run is holding, and never past the limits.
///
/// Lower bounds only: the probe's owner may hold objects from a run still in flight, so an exact
/// count would be flaky by design. What it can say is that the usage is computed rather than
/// stored — a zero here with a workspace of ours running is the bug this id exists for.
pub async fn quota_view(c: &mut Ctx) {
    c.step("quota.view", QUOTA_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, "/v1/quota");
        async move {
            let doc = get(c, &url, &jwt).await.context("could not read the quota")?;
            reflects(&doc)
        }
        .boxed()
    })
    .await;
}

/// The three dimensions this run certainly moved: it created a workspace, an environment, and
/// pushed. Every one of them must be counted, and none of them may exceed its own limit.
fn reflects(doc: &Value) -> Result<()> {
    for dim in ["workspaces", "environments", "snapshots"] {
        let used = doc.get("used").and_then(|u| u.get(dim)).and_then(Value::as_u64);
        let limit = doc.get("limit").and_then(|l| l.get(dim)).and_then(Value::as_u64);
        match (used, limit) {
            (Some(u), Some(l)) if u >= 1 && u <= l => {}
            (Some(u), Some(l)) => return Err(anyhow!("{dim}: {u} of {l} does not reflect what this run holds")),
            _ => return Err(anyhow!("the quota answer carries no {dim}")),
        }
    }
    Ok(())
}

/// `id` off a create/clone/push answer.
fn id_of(doc: &Value) -> Result<String> {
    doc.get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("the answer carried no id"))
}

fn state_is(v: &Value, want: &str) -> bool {
    v.get("state").and_then(Value::as_str) == Some(want)
}

/// The environment says `running` AND both StatefulSets have a ready replica. The record alone is
/// not what a person waits for.
async fn running(c: &Ctx, env: &str, cap: Duration) -> Result<()> {
    let read = api(c, &format!("/v1/environments/{env}"));
    poll_json(c, &read, &c.probe_jwt.clone(), cap, |v| state_is(v, "running"))
        .await
        .context("the environment never reported running")?;
    all_ready(c, env, cap).await
}

async fn all_ready(c: &Ctx, env: &str, cap: Duration) -> Result<()> {
    let k = c.kube.as_ref().ok_or_else(|| anyhow!("no kubeconfig"))?;
    let ns = kloudlite_workspaces::crd::env_namespace(env);
    let sts: kube::Api<StatefulSet> = kube::Api::namespaced(k.clone(), &ns);
    let start = std::time::Instant::now();
    loop {
        let mut waiting = None;
        for svc in SERVICES {
            let ready = sts.get(svc).await.ok().and_then(|s| s.status).and_then(|st| st.ready_replicas).unwrap_or(0);
            if ready < 1 {
                waiting = Some(svc);
            }
        }
        let Some(svc) = waiting else { return Ok(()) };
        if start.elapsed() >= cap {
            return Err(anyhow!("`{svc}` had no ready replica after {} ms", cap.as_millis()));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// No pod at all in the environment's namespace — a stop that left one is a stop that did not
/// happen, whatever the record says.
async fn pods_gone(c: &Ctx, env: &str, cap: Duration) -> Result<()> {
    let k = c.kube.as_ref().ok_or_else(|| anyhow!("no kubeconfig"))?;
    let ns = kloudlite_workspaces::crd::env_namespace(env);
    let pods: kube::Api<Pod> = kube::Api::namespaced(k.clone(), &ns);
    let start = std::time::Instant::now();
    loop {
        let left = pods.list(&kube::api::ListParams::default()).await.map(|l| l.items.len()).unwrap_or(usize::MAX);
        if left == 0 {
            return Ok(());
        }
        if start.elapsed() >= cap {
            return Err(anyhow!("{left} pod(s) still running after {} ms", cap.as_millis()));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// One command in a service pod. One StatefulSet per service, one replica, so the ordinal is zero;
/// the container carries the service's own name.
async fn svc_exec(c: &Ctx, env: &str, svc: &str, script: &str, cap: Duration) -> Result<(i32, String, String)> {
    let k = c.kube.as_ref().ok_or_else(|| anyhow!("no kubeconfig"))?;
    let ns = kloudlite_workspaces::crd::env_namespace(env);
    crate::kube::exec(k, &ns, &format!("{svc}-0"), Some(svc), &["sh", "-c", script], cap).await
}

/// The same, where a non-zero exit is the step's failure.
async fn expect_exec(c: &Ctx, env: &str, script: &str, what: &str) -> Result<()> {
    let (code, _, err) = svc_exec(c, env, REDIS, script, EXEC_CEILING).await.context(what.to_string())?;
    if code != 0 {
        return Err(anyhow!("{what}: exited {code}: {}", err.trim()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;
    use axum::routing::{get as route_get, post as route_post};

    /// Both reads, and the one thing about them a consumer cannot re-derive: the order.
    #[test]
    fn history_must_be_newest_first_with_the_tip_as_main() {
        let rows = serde_json::json!([{"id": "s2", "message": "two"}, {"id": "s1", "message": "one"}]);
        let refs = serde_json::json!({"main": "s2"});
        assert!(reads_back(&rows, &refs, "s2").is_ok());
        // Backwards is the regression this exists for.
        let old = serde_json::json!([{"id": "s1", "message": "one"}, {"id": "s2", "message": "two"}]);
        assert!(reads_back(&old, &refs, "s2").is_err());
        // A ref that is not the newest push is a clone grafting onto the wrong cut.
        assert!(reads_back(&rows, &serde_json::json!({"main": "s1"}), "s2").is_err());
        assert!(reads_back(&rows, &serde_json::json!({"main": null}), "s2").is_err());
    }

    /// Usage is computed from the CRDs on every request; a stored counter can only be wrong in the
    /// direction that hands out allocation nobody has.
    #[test]
    fn the_quota_view_counts_what_the_run_holds() {
        let live = serde_json::json!({
            "used": {"workspaces": 3, "environments": 2, "snapshots": 4},
            "limit": {"workspaces": 10, "environments": 5, "snapshots": 50},
        });
        assert!(reflects(&live).is_ok());
        // Nothing counted while this run holds a workspace: the bug the id exists for.
        let zero = serde_json::json!({
            "used": {"workspaces": 0, "environments": 2, "snapshots": 4},
            "limit": {"workspaces": 10, "environments": 5, "snapshots": 50},
        });
        assert!(reflects(&zero).is_err());
        // Past its own limit: allocation that was handed out without a decision.
        let over = serde_json::json!({
            "used": {"workspaces": 3, "environments": 9, "snapshots": 4},
            "limit": {"workspaces": 10, "environments": 5, "snapshots": 50},
        });
        assert!(reflects(&over).is_err());
        assert!(reflects(&serde_json::json!({"used": {}, "limit": {}})).is_err());
    }

    /// The whole environment journey reports, exactly once each, when there is no cluster to run
    /// it against — a deployment gap, not an SLO breach.
    #[tokio::test]
    async fn the_environment_ids_skip_without_a_kubeconfig() {
        let mut c = testkit::ctx().await;
        c.kube = None;
        environments(&mut c).await;
        for id in ["env.services.multi", "env.clone", "env.restore.inplace", "env.stop.start"] {
            let s = c.steps.iter().find(|s| s.slo_id == id).unwrap_or_else(|| panic!("{id}"));
            assert!(s.skipped && s.detail == "no kubeconfig", "{s:?}");
            assert_eq!(c.steps.iter().filter(|s| s.slo_id == id).count(), 1, "{id}");
        }
    }

    /// A precondition failure — the workspace the two pushes need never created — is `vol.history`
    /// FAILING with the reason, once. A skip would say the fleet was never asked.
    #[tokio::test]
    async fn vol_history_fails_once_when_its_pushes_cannot_happen() {
        let app = axum::Router::new().route(
            "/v1/workspaces",
            route_post(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
        );
        let mut c = testkit::ctx_against(app).await;
        history(&mut c).await;
        let rows: Vec<_> = c.steps.iter().filter(|s| s.slo_id == "vol.history").collect();
        assert_eq!(rows.len(), 1, "{:?}", c.steps);
        assert!(!rows[0].ok && !rows[0].skipped, "{:?}", rows[0]);
        assert!(rows[0].detail.contains("history workspace"), "{:?}", rows[0]);
    }

    /// And the whole path once, against a fleet that answers: create, ready, two pushes, both reads.
    #[tokio::test]
    async fn vol_history_passes_once_against_an_api_that_answers() {
        let app = axum::Router::new()
            .route("/v1/workspaces", route_post(|| async { axum::Json(serde_json::json!({"id": "ws-1"})) }))
            .route("/v1/workspaces/{id}", route_get(|| async { axum::Json(serde_json::json!({"state": "ready"})) }))
            .route(
                "/v1/workspaces/{id}/push",
                route_post(|axum::Json(b): axum::Json<serde_json::Value>| async move {
                    let id = if b["message"] == "one" { "s1" } else { "s2" };
                    axum::Json(serde_json::json!({"id": id}))
                }),
            )
            .route(
                "/v1/volumes/{name}/history",
                route_get(|| async {
                    axum::Json(serde_json::json!([
                        {"id": "s2", "message": "two", "phase": "ready"},
                        {"id": "s1", "message": "one", "phase": "ready"},
                    ]))
                }),
            )
            .route("/v1/volumes/{name}/refs", route_get(|| async { axum::Json(serde_json::json!({"main": "s2"})) }));
        let mut c = testkit::ctx_against(app).await;
        history(&mut c).await;
        let rows: Vec<_> = c.steps.iter().filter(|s| s.slo_id == "vol.history").collect();
        assert_eq!(rows.len(), 1, "{:?}", c.steps);
        assert!(rows[0].ok, "{:?}", rows[0]);
        // Registered, or teardown leaks a subvolume every hour.
        assert_eq!(c.state.history_workspace.as_deref(), Some("ws-1"));
        assert!(c.state.extra_volumes.contains(&"ws-1".to_string()));
    }

    /// `quota.view` reports once either way — a read that answers, and a read that does not.
    #[tokio::test]
    async fn quota_view_reports_once_on_both_paths() {
        let ok = axum::Router::new().route(
            "/v1/quota",
            route_get(|| async {
                axum::Json(serde_json::json!({
                    "used": {"workspaces": 1, "environments": 1, "snapshots": 1},
                    "limit": {"workspaces": 4, "environments": 4, "snapshots": 40},
                }))
            }),
        );
        let mut c = testkit::ctx_against(ok).await;
        quota_view(&mut c).await;
        assert_eq!(c.steps.iter().filter(|s| s.slo_id == "quota.view").count(), 1);
        assert!(c.steps[0].ok, "{:?}", c.steps[0]);

        let down = axum::Router::new()
            .route("/v1/quota", route_get(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }));
        let mut c = testkit::ctx_against(down).await;
        quota_view(&mut c).await;
        assert_eq!(c.steps.iter().filter(|s| s.slo_id == "quota.view").count(), 1);
        assert!(!c.steps[0].ok && !c.steps[0].skipped, "{:?}", c.steps[0]);
    }
}
