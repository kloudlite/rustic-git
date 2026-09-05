//! Stage 7 · Lifecycle: stop, replicate, start, restore, and the refusals and collection that
//! bound what a person can destroy.
//!
//! Worst case 500 s if every step times out — the workspace half's 335 s (15 + 60 + 30 + 60 + 20 +
//! 60 + 60) plus the environment half's 165 s (30 + 30 + 60 + 45); see `workspace.rs`'s note on
//! how the stages' sums sit against the fast suite's deadline.
//!
//! Two halves: the workspace stage 5 created and pushed, and the environment stage 6 did. Each
//! skips its own ids with one reason when its object is missing, and neither costs the other.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;
use kloudlite_workspaces::crd;
use serde_json::Value;

use super::{api, get, poll_json, post, raw};
use crate::ctx::Ctx;

const STOP_CEILING: Duration = Duration::from_secs(15);
/// The catalogue allows five minutes for a replica to hold the final cut. The probe waits ONE, and
/// a slower replica is a bad sample rather than four minutes of a 540 s budget spent waiting: the
/// wake the owner sends right after the cut makes a healthy fleet finish this in seconds.
const REPLICATED_CEILING: Duration = Duration::from_secs(60);
const START_CEILING: Duration = Duration::from_secs(30);
const RESTORE_CEILING: Duration = Duration::from_secs(60);
const REFUSAL_CEILING: Duration = Duration::from_secs(20);
/// How long the finalizers get to drop this run's worktrees before the detached restore.
const DETACH_CEILING: Duration = Duration::from_secs(60);
const ORPHAN_CEILING: Duration = Duration::from_secs(60);
/// The environment twins. Each is at or above its own catalogue target — except `env.replicated`,
/// which waits 30 s against a 300 s bound for the same reason `ws.replicated` waits 60: the wake
/// the owner sends right after the stop cut finishes this in seconds on a healthy fleet, and the
/// fast suite's budget is not there to be spent waiting out a broken one.
const ENV_STOP_CEILING: Duration = Duration::from_secs(30);
const ENV_REPLICATED_CEILING: Duration = Duration::from_secs(30);
const ENV_START_CEILING: Duration = Duration::from_secs(60);
/// The restore ends at `running` with its services ready, like `ws.restore` does, so it needs the
/// room an environment actually takes to converge — 90 s, above `env.start.p95`'s own 60 s target
/// and below `env.create.p95`'s 120: a restore grafts onto bytes the node already holds, where a
/// create builds them.
const ENV_RESTORE_CEILING: Duration = Duration::from_secs(90);

/// Every workspace id in this stage, in journey order — the list a missing workspace skips.
const IDS: [&str; 7] = [
    "ws.stop.p95",
    "ws.replicated",
    "ws.start.p95",
    "ws.restore",
    "vol.refusals",
    "vol.detached.restorable",
    "vol.orphan.collected",
];

/// The environment twins, in journey order. A separate list because they stand on stage 6's
/// environment, not on stage 5's workspace: one half being absent must not cost the other its ids.
const ENV_IDS: [&str; 4] = ["env.stop.p95", "env.replicated", "env.start.p95", "env.restore"];

/// The two delete verbs, last: they take the objects the halves above measured, so nothing runs
/// after them that could want one back.
const DELETE_IDS: [&str; 2] = ["wt.delete", "snap.delete"];

/// 75, not 45: `wt.delete`'s catalogue target is `bound(60_000)`, and a ceiling BELOW its own
/// target inverts the rule every stage here states — a slow-but-passing delete would be cut off
/// instead of measured as the breach it is. Target plus slack, like everything else.
const DELETE_CEILING: Duration = Duration::from_secs(75);
/// Each of the THREE waits inside `wt.delete` gets a slice, never the step's whole ceiling.
///
/// They used to be handed `DELETE_CEILING` each, which meant the step's own timeout always fired
/// first and every failure read "timed out after 75000 ms" — naming none of the three, so a run
/// could not say whether the worktree, the environment or the volume was the slow one. A slice
/// each makes the step report WHICH, and three of them still fit inside the ceiling.
const WAIT: Duration = Duration::from_secs(22);
const SNAP_DELETE_CEILING: Duration = Duration::from_secs(25);

pub async fn run(c: &mut Ctx) {
    workspace_half(c).await;
    environment_half(c).await;
    deletes(c).await;
}

/// `wt.delete` and `snap.delete`: the two verbs a person uses to take something away, and the
/// reference-counting rule that decides what survives them.
///
/// Both stand on the ENVIRONMENT, which by now is the last thing this run holds that has a volume
/// with a snapshot on it — the workspace half's `orphan_collected` has already collected the
/// workspace's. That is also why they run here rather than in either half: they are the end of the
/// stage by construction, and there is nothing left to measure afterwards.
async fn deletes(c: &mut Ctx) {
    let (Some(env), Some(volume)) = (c.state.environment.clone(), c.state.env_volume.clone()) else {
        for id in DELETE_IDS {
            c.skip(id, "no environment");
        }
        return;
    };
    let Some(snapshot) = c.state.env_snapshot.clone() else {
        for id in DELETE_IDS {
            c.skip(id, "the environment was never pushed");
        }
        return;
    };
    if !wt_delete(c, &env, &volume, &c.state.clone.clone()).await {
        return c.skip("snap.delete", "the environment's worktree never went");
    }
    snap_delete(c, &volume, &snapshot).await;
}

/// `wt.delete`: `cleanup_parent`'s detach-or-keep rule, both directions.
///
/// One step, because either direction alone is worthless: a finalizer that ALWAYS detached would
/// pass the "collected" half, and one that never did would pass the "survives" half. The two
/// subjects are the ones this run already holds — the workspace clone, which was never pushed and
/// whose Volume must therefore go with it, and the environment, whose push must keep its Volume
/// standing detached. A lost detach here is bytes nothing on any tier can find again.
async fn wt_delete(c: &mut Ctx, env: &str, volume: &str, clone: &Option<String>) -> bool {
    let (env, volume, clone) = (env.to_string(), volume.to_string(), clone.clone());
    c.step("wt.delete", DELETE_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let env_url = api(c, &format!("/v1/environments/{env}"));
        async move {
            // The workspace direction: the clone was never pushed, so nothing references its
            // Volume once the working copy is gone and Kubernetes GC must take it. The clone's own
            // Volume carries the clone's id, exactly as a fresh workspace's does.
            if let Some(clone) = clone {
                let ws = api(c, &format!("/v1/workspaces/{clone}"));
                // Already gone is the state this half wants, and `detached_restorable` may well
                // have taken it: a 404 from the delete is not a failure.
                let _ = super::call(c, reqwest::Method::DELETE, &ws, &jwt, None).await;
                gone(c, &ws, &jwt, WAIT, "the clone's worktree").await?;
                volume_gone(c, &jwt, &clone, WAIT, "the clone's volume with no snapshot").await?;
            }
            // The environment direction: a snapshot still references the Volume, so the worktree
            // goes and the Volume stays — detached, with its history readable.
            super::call(c, reqwest::Method::DELETE, &env_url, &jwt, None)
                .await
                .context("could not delete the environment")?;
            gone(c, &env_url, &jwt, WAIT, "the environment's worktree").await?;
            // And every OTHER environment this run still holds, because `env.restore` leaves one
            // standing ON THIS SNAPSHOT: a restore grafts a new working copy onto it and waits for
            // it to run, so the snapshot `snap.delete` is about to take is a running worktree's
            // base until that copy goes — which the api refuses with 409, correctly. Deleting them
            // here is also what makes the assertion below mean anything: with no working copy left
            // at all, the volume surviving can only be the snapshot holding it.
            for id in environments(c).await {
                let url = api(c, &format!("/v1/environments/{id}"));
                let _ = super::call(c, reqwest::Method::DELETE, &url, &jwt, None).await;
                gone(c, &url, &jwt, WAIT, "a leftover environment on this run's volume").await?;
            }
            match volume_listed(c, &jwt, &volume).await? {
                true => Ok(()),
                false => Err(anyhow!(
                    "the volume was taken with the environment even though a snapshot remains"
                )),
            }
        }
        .boxed()
    })
    .await
}

/// `snap.delete`: the ACCEPTED delete, and the rule at the end of it.
///
/// `vol.refusals` probes only the three deletes that must be REFUSED, so the path a person
/// actually walks was never measured. Both halves of the SLI in one step because they are one
/// fact: this is the environment volume's last snapshot on a volume `wt.delete` just detached, so
/// removing it must take the snapshot out of history AND take the volume with it.
async fn snap_delete(c: &mut Ctx, volume: &str, snapshot: &str) {
    let (volume, snapshot) = (volume.to_string(), snapshot.to_string());
    c.step("snap.delete", SNAP_DELETE_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let one = api(c, &format!("/v1/volumes/{volume}/snapshots/{snapshot}"));
        async move {
            super::call(c, reqwest::Method::DELETE, &one, &jwt, None)
                .await
                .context("could not delete the snapshot")?;
            // The volume goes with its last snapshot.
            volume_gone(c, &jwt, &volume, SNAP_DELETE_CEILING, "the detached volume's last snapshot").await
        }
        .boxed()
    })
    .await;
}

/// Whether `/v1/volumes` still lists this volume.
///
/// The LIST, never `GET /v1/volumes/{name}` — there is no such route. `/v1/volumes/{name}` is
/// registered DELETE-only (`crates/workspaces/src/api/mod.rs:334`), so a GET there answers 405
/// forever: not a state a wait can converge on, which is exactly how `wt.delete` spent its whole
/// budget asking. A volume's `name` in the listing is its ws/env id, which is what every caller
/// here holds (`display_name` is the caller-chosen one teardown matches on).
async fn volume_listed(c: &Ctx, jwt: &str, name: &str) -> Result<bool> {
    let rows = get(c, &api(c, "/v1/volumes"), jwt).await.context("could not list the volumes")?;
    Ok(rows
        .as_array()
        .is_some_and(|rows| rows.iter().any(|r| r.get("name").and_then(Value::as_str) == Some(name))))
}

/// Poll the listing until the volume is no longer in it.
async fn volume_gone(c: &Ctx, jwt: &str, name: &str, cap: Duration, what: &str) -> Result<()> {
    let start = std::time::Instant::now();
    loop {
        if !volume_listed(c, jwt, name).await? {
            return Ok(());
        }
        if start.elapsed() >= cap {
            return Err(anyhow!("{what} is still listed after {} ms", cap.as_millis()));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Poll `url` until it 404s. The deletes above are all wishes — a finalizer drops the worktree
/// afterwards — so "it is gone" is a wait, never a read.
async fn gone(c: &Ctx, url: &str, jwt: &str, cap: Duration, what: &str) -> Result<()> {
    let start = std::time::Instant::now();
    loop {
        let (status, _) = raw(c, reqwest::Method::GET, url, jwt, None, &[]).await?;
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        if start.elapsed() >= cap {
            return Err(anyhow!("{what} still answers {status} after {} ms", cap.as_millis()));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn workspace_half(c: &mut Ctx) {
    let (Some(ws), Some(volume)) = (c.state.workspace.clone(), c.state.volume.clone()) else {
        for id in IDS {
            c.skip(id, "no workspace");
        }
        return;
    };
    stop(c, &ws).await;
    replicated(c, &ws).await;
    start(c, &ws).await;
    let Some(snapshot) = c.state.snapshot.clone() else {
        // The push is what every remaining id stands on: without one there is nothing to restore
        // from, no base a delete could be refused over, and no volume to collect.
        for id in &IDS[3..] {
            c.skip(id, "the workspace was never pushed");
        }
        return;
    };
    restore(c, &snapshot).await;
    refusals(c, &volume, &snapshot).await;
    detached_restorable(c, &snapshot).await;
    orphan_collected(c, &volume).await;
}

/// The same four verbs on stage 6's environment — the owner's rule that every workspace SLO has an
/// environment counterpart at the same cadence. They chain like the workspace half: a stop that
/// never converged leaves a state the start would measure instead of the start.
async fn environment_half(c: &mut Ctx) {
    let Some(env) = c.state.environment.clone() else {
        for id in ENV_IDS {
            c.skip(id, "no environment");
        }
        return;
    };
    if !env_stop(c, &env).await {
        for id in &ENV_IDS[1..] {
            c.skip(id, "the environment never stopped");
        }
        return;
    }
    env_replicated(c, &env).await;
    env_start(c, &env).await;
    match c.state.env_snapshot.clone() {
        Some(snap) => env_restore(c, &snap).await,
        None => c.skip("env.restore", "the environment was never pushed"),
    }
}

/// `env.stop.p95`: the stop, and the wait for `stopped` — an environment tears its StatefulSets
/// down after the stop cut, so a 202 says nothing about whether that landed.
async fn env_stop(c: &mut Ctx, env: &str) -> bool {
    let env = env.to_string();
    c.step("env.stop.p95", ENV_STOP_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/environments/{env}/stop"));
        let doc = api(c, &format!("/v1/environments/{env}"));
        async move {
            post(c, &url, &jwt, Value::Null).await.context("could not stop")?;
            poll_json(c, &doc, &jwt, ENV_STOP_CEILING, |v| {
                v.get("state").and_then(Value::as_str) == Some("stopped")
            })
            .await
        }
        .boxed()
    })
    .await
}

/// `env.replicated`: read off the `Replicated` condition the owner computes, exactly as
/// `ws.replicated` is — the same condition placement itself reads.
async fn env_replicated(c: &mut Ctx, env: &str) {
    let env = env.to_string();
    c.step("env.replicated", ENV_REPLICATED_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let doc = api(c, &format!("/v1/environments/{env}"));
        async move {
            poll_json(c, &doc, &jwt, ENV_REPLICATED_CEILING, |v| {
                // `ready`, not `status`: the wire shape is a `ConditionDoc`, as it is for a
                // workspace — see `replicated` above.
                v.pointer("/replicated/ready").and_then(Value::as_bool) == Some(true)
            })
            .await
            .context("no other node reported holding the final sync point")
        }
        .boxed()
    })
    .await;
}

/// `env.start.p95`: the services come back. `running` is the environment's own word for `ready`.
async fn env_start(c: &mut Ctx, env: &str) {
    let env = env.to_string();
    c.step("env.start.p95", ENV_START_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/environments/{env}/start"));
        let doc = api(c, &format!("/v1/environments/{env}"));
        async move {
            post(c, &url, &jwt, Value::Null).await.context("could not start")?;
            poll_json(c, &doc, &jwt, ENV_START_CEILING, |v| {
                v.get("state").and_then(Value::as_str) == Some("running")
            })
            .await
        }
        .boxed()
    })
    .await;
}

/// `env.restore`: a NEW environment grafted onto stage 6's push — `ws.restore`'s twin.
///
/// The services are left out of the body deliberately: absent means "the ones the snapshot froze",
/// which is what a person restoring gets.
///
/// It ends at `running` WITH its services ready, not at the accept. An accept has resolved the
/// snapshot, its volume and the caller's right to it and nothing else — and "restoring an
/// environment succeeds" is, to the person who asked for it, the services coming back. A restore
/// that is taken and then never converges is exactly the failure this id is named for, and ending
/// at the 202 would have reported it green.
async fn env_restore(c: &mut Ctx, snapshot: &str) {
    let name = format!("{}-envrestore", c.prefix());
    let snapshot = snapshot.to_string();
    c.step("env.restore", ENV_RESTORE_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, "/v1/environments/restore");
        let body = serde_json::json!({ "name": name, "snapshot_id": snapshot });
        async move {
            let doc = post(c, &url, &jwt, body).await.context("could not restore the environment")?;
            let id = doc
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("the restore answered no environment id"))?
                .to_string();
            // Its Volume is named after it and no prefix sweep can see one. Recorded BEFORE the
            // wait: a restore that never converges still holds a subvolume.
            c.state.extra_volumes.push(id.clone());
            let doc = api(c, &format!("/v1/environments/{id}"));
            poll_json(c, &doc, &jwt, ENV_RESTORE_CEILING, |v| {
                v.get("state").and_then(Value::as_str) == Some("running")
            })
            .await
            .context("the restored environment never reported running")?;
            // `running` is the record; the services are what a person restored FOR. Without a
            // kubeconfig there is nothing to read and the record stands alone.
            super::environment::service_ready(c, &id, ENV_RESTORE_CEILING).await
        }
        .boxed()
    })
    .await;
}

/// `ws.stop.p95`: the stop, and the wait for the workspace to actually be `stopped` — a stop cuts
/// a final sync point first, and a 202 says nothing about whether that landed.
async fn stop(c: &mut Ctx, ws: &str) {
    let ws = ws.to_string();
    c.step("ws.stop.p95", STOP_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/workspaces/{ws}/stop"));
        let doc = api(c, &format!("/v1/workspaces/{ws}"));
        async move {
            post(c, &url, &jwt, Value::Null).await.context("could not stop")?;
            poll_json(c, &doc, &jwt, STOP_CEILING, |v| {
                v.get("state").and_then(Value::as_str) == Some("stopped")
            })
            .await
        }
        .boxed()
    })
    .await;
}

/// `ws.replicated`: another node holds the final sync point, read off the `Replicated` condition
/// the owner computes — never inferred from anything else, because that condition is what
/// placement itself reads before letting the workspace start elsewhere.
async fn replicated(c: &mut Ctx, ws: &str) {
    let ws = ws.to_string();
    c.step("ws.replicated", REPLICATED_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let doc = api(c, &format!("/v1/workspaces/{ws}"));
        async move {
            poll_json(c, &doc, &jwt, REPLICATED_CEILING, |v| {
                // `ready`, not `status`: the wire shape is a `ConditionDoc` (`ready`/`reason`/
                // `message`), and the CRD's `status: "True"` string never reaches the client. A
                // pointer at a field that is not there is never true, so this SLO would have
                // failed on every run of a perfectly healthy fleet.
                v.pointer("/replicated/ready").and_then(Value::as_bool) == Some(true)
            })
            .await
            .context("no other node reported holding the final sync point")?;
            // The condition is the owner's own summary; this is the peer's. `may_claim` reads
            // `VolumeReplica.status.branches[worktree]` and starts the workspace elsewhere only
            // when it names that worktree's newest Ready transient — so a condition that flipped
            // while no replica names the cut is a workspace that cannot actually move, which is
            // the whole thing this SLO exists to promise.
            named_by_a_replica(c, &ws).await
        }
        .boxed()
    })
    .await;
}

/// A `VolumeReplica` on some OTHER node names this worktree's cut by name.
///
/// Without a kubeconfig there is nothing to read — a deployment gap, not a breach — so the
/// condition the step already checked stands alone and this adds nothing.
async fn named_by_a_replica(c: &Ctx, ws: &str) -> Result<()> {
    let Some(k) = c.kube.as_ref() else { return Ok(()) };
    let api: kube::Api<crd::VolumeReplica> = kube::Api::all(k.clone());
    let list = api
        .list(&kube::api::ListParams::default())
        .await
        .context("could not list the volume replicas")?;
    let named = list.items.iter().any(|r| {
        r.status
            .as_ref()
            .is_some_and(|s| s.branches.get(ws).is_some_and(|cut| !cut.is_empty()))
    });
    named
        .then_some(())
        .ok_or_else(|| anyhow!("`Replicated` is true but no VolumeReplica names {ws}'s cut"))
}

async fn start(c: &mut Ctx, ws: &str) {
    let ws = ws.to_string();
    c.step("ws.start.p95", START_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/workspaces/{ws}/start"));
        let doc = api(c, &format!("/v1/workspaces/{ws}"));
        async move {
            post(c, &url, &jwt, Value::Null).await.context("could not start")?;
            poll_json(c, &doc, &jwt, START_CEILING, |v| {
                v.get("state").and_then(Value::as_str) == Some("ready")
            })
            .await
        }
        .boxed()
    })
    .await;
}

/// `ws.restore`: a new working copy grafted onto the push, which is the undo a person reaches for.
async fn restore(c: &mut Ctx, snapshot: &str) {
    let name = format!("{}-restore", c.prefix());
    let snapshot = snapshot.to_string();
    c.step("ws.restore", RESTORE_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, "/v1/workspaces/restore");
        let body = serde_json::json!({ "name": name, "snapshot_id": snapshot });
        async move {
            let doc = post(c, &url, &jwt, body).await.context("could not restore")?;
            let id = doc
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("the restore answered no workspace id"))?;
            let ws = api(c, &format!("/v1/workspaces/{id}"));
            poll_json(c, &ws, &jwt, RESTORE_CEILING, |v| {
                v.get("state").and_then(Value::as_str) == Some("ready")
            })
            .await
        }
        .boxed()
    })
    .await;
}

/// `vol.refusals`: the three deletes that must be refused, in one step, first failure wins.
///
/// All three or none: a tier that refused everything would pass any one of them alone, and each on
/// its own says nothing about whether the OTHER two doors are open. The sync point has to be found
/// through the CRs — `/v1/volumes/{n}/history` lists snapshots only, deliberately — so with no
/// kubeconfig this skips rather than testing two thirds of an id.
async fn refusals(c: &mut Ctx, volume: &str, snapshot: &str) {
    let Some(k) = c.kube.clone() else {
        return c.skip("vol.refusals", "no kubeconfig: a sync point cannot be named");
    };
    let sync = match sync_point(&k, volume).await {
        Ok(s) => s,
        Err(e) => return c.skip("vol.refusals", &format!("{e:#}")),
    };
    let (volume, snapshot) = (volume.to_string(), snapshot.to_string());
    c.step("vol.refusals", REFUSAL_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let base = api(c, &format!("/v1/volumes/{volume}"));
        async move {
            // The agent's own cut: deleting one by hand removes a replica's send parent.
            refused(c, &format!("{base}/snapshots/{sync}"), &jwt, "a sync point").await?;
            // The base a running worktree is standing on.
            refused(c, &format!("{base}/snapshots/{snapshot}"), &jwt, "a running worktree's base").await?;
            // The volume itself, while working copies are still on it.
            refused(c, &base, &jwt, "a volume with a working copy").await
        }
        .boxed()
    })
    .await;
}

/// Delete every working copy this run made and wait until `/v1` reports none left.
async fn detach_all(c: &Ctx, cap: Duration) -> Result<()> {
    let start = std::time::Instant::now();
    loop {
        let live = worktrees(c).await;
        if live.is_empty() {
            return Ok(());
        }
        for id in &live {
            let url = api(c, &format!("/v1/workspaces/{id}"));
            let _ = super::call(c, reqwest::Method::DELETE, &url, &c.probe_jwt.clone(), None).await;
        }
        if start.elapsed() >= cap {
            return Err(anyhow!("{} working copies were still on the volume after {} ms", live.len(), cap.as_millis()));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// One DELETE that must answer 409.
async fn refused(c: &Ctx, url: &str, jwt: &str, what: &str) -> Result<()> {
    let (status, body) = raw(c, reqwest::Method::DELETE, url, jwt, None, &[]).await?;
    if status == reqwest::StatusCode::CONFLICT {
        return Ok(());
    }
    Err(anyhow!("deleting {what} answered {status}: {}", body.chars().take(200).collect::<String>()))
}

/// The newest sync point on `volume` — the agent's own cut, which no `/v1` listing shows.
async fn sync_point(k: &kube::Client, volume: &str) -> Result<String> {
    use kube::ResourceExt;
    let api: kube::Api<crd::Snapshot> = kube::Api::all(k.clone());
    let list = api
        .list(&kube::api::ListParams::default().fields(&format!("spec.volume={volume}")))
        .await
        .context("could not list the volume's snapshots")?;
    list.items
        .iter()
        .find(|s| !s.is_snapshot())
        .map(|s| s.name_any())
        .ok_or_else(|| anyhow!("the volume has no sync point to try deleting"))
}

/// `vol.detached.restorable`: the snapshots outlive every working copy, and one of them still
/// restores.
///
/// The accept is the measurement, not a second wait for `ready`: what this SLO is about is whether
/// a detached volume's record can still be reached at all — a restore that the API takes has
/// already resolved the snapshot, its volume and the caller's right to it, and `ws.restore` above
/// is the id that measures a restore converging.
async fn detached_restorable(c: &mut Ctx, snapshot: &str) {
    // Every working copy on the volume goes first — that is what "detached" means — and the delete
    // is only ACCEPTED synchronously: the `WORKTREE_FINALIZER` drops the worktree and detaches the
    // Volume afterwards, so restoring straight after the DELETE would measure a volume that is
    // still attached and prove nothing this SLO is about.
    if let Err(e) = detach_all(c, DETACH_CEILING).await {
        return c.skip("vol.detached.restorable", &format!("{e:#}"));
    }
    let name = format!("{}-detached", c.prefix());
    let snapshot = snapshot.to_string();
    c.step("vol.detached.restorable", RESTORE_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, "/v1/workspaces/restore");
        let body = serde_json::json!({ "name": name, "snapshot_id": snapshot });
        async move {
            let doc = post(c, &url, &jwt, body).await.context("the detached snapshot would not restore")?;
            // The id is not kept: teardown finds it by the `run-{id}` name prefix like every other
            // object, and the next step re-reads the volume's worktrees anyway.
            doc.get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("the restore answered no workspace id"))?;
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `vol.orphan.collected`: with no working copy and no snapshot left, the `Volume` is collected —
/// the reference count reaching zero, watched from the outside.
///
/// The deletes are inside the step's loop rather than before it because they are the thing being
/// measured against: a snapshot delete is refused while a worktree still stands on it, so the
/// order the fleet converges in is the order this retries in.
async fn orphan_collected(c: &mut Ctx, volume: &str) {
    let Some(k) = c.kube.clone() else {
        return c.skip("vol.orphan.collected", "no kubeconfig: the Volume cannot be watched");
    };
    let volume = volume.to_string();
    c.step("vol.orphan.collected", ORPHAN_CEILING, move |c| {
        async move {
            let start = std::time::Instant::now();
            loop {
                for id in worktrees(c).await {
                    let url = api(c, &format!("/v1/workspaces/{id}"));
                    let _ = super::call(c, reqwest::Method::DELETE, &url, &c.probe_jwt.clone(), None).await;
                }
                for id in snapshots(c, &volume).await {
                    let url = api(c, &format!("/v1/volumes/{volume}/snapshots/{id}"));
                    let _ = super::call(c, reqwest::Method::DELETE, &url, &c.probe_jwt.clone(), None).await;
                }
                // `retire_pass` deletes a Volume with no owner entry AND no snapshot, so the
                // detach is the half that has to happen first — a Volume that still lists its
                // parent as an owner and vanished anyway went for some other reason, and a lost
                // detach is an error rather than a completed finalizer. Read before the wait so a
                // still-owned Volume is named as that rather than as a slow sweep.
                still_owned(&k, &volume).await?;
                let left = ORPHAN_CEILING.saturating_sub(start.elapsed());
                if crate::kube::wait_for::<crd::Volume>(&k, &volume, left.min(Duration::from_secs(5)), |v| v.is_none())
                    .await
                    .is_ok()
                {
                    return Ok(());
                }
                if start.elapsed() >= ORPHAN_CEILING {
                    return Err(anyhow!("the Volume was still there after {} ms", ORPHAN_CEILING.as_millis()));
                }
            }
        }
        .boxed()
    })
    .await;
}

/// Whether the Volume still lists a parent as an owner. `Ok` means it does not — either because
/// the finalizer detached it, or because it is already gone, which is the state the caller is
/// waiting for anyway. A Volume that is still owned is not yet a candidate for `retire_pass` and
/// is reported as that, so a sweep that took an OWNED volume cannot pass as a collection.
async fn still_owned(k: &kube::Client, volume: &str) -> Result<()> {
    let api: kube::Api<crd::Volume> = kube::Api::all(k.clone());
    let Ok(Some(v)) = api.get_opt(volume).await else { return Ok(()) };
    match v.metadata.owner_references.as_ref().map_or(0, Vec::len) {
        0 => Ok(()),
        n => Err(anyhow!("the Volume still lists {n} owner(s): the finalizer has not detached it")),
    }
}

/// Every workspace THIS RUN created, by name prefix — the same contract teardown sweeps on.
///
/// Not by the doc's `volume` field: that field is null until the volume has a push AND it is the
/// owner's pushed set, so a clone or a restore taken before the first push simply would not appear
/// and would be left standing on the volume this stage is trying to empty.
///
/// A listing that fails is an empty list; every caller re-reads on its next pass.
async fn worktrees(c: &Ctx) -> Vec<String> {
    let url = api(c, "/v1/workspaces");
    let prefix = c.prefix();
    let rows = get(c, &url, &c.probe_jwt).await.unwrap_or(Value::Null);
    rows.as_array()
        .map(|rows| {
            rows.iter()
                .filter(|r| r.get("name").and_then(Value::as_str).is_some_and(|n| n.starts_with(&prefix)))
                .filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Every environment THIS RUN created, by name prefix — `worktrees`' twin for the other kind.
async fn environments(c: &Ctx) -> Vec<String> {
    let prefix = c.prefix();
    let rows = get(c, &api(c, "/v1/environments"), &c.probe_jwt).await.unwrap_or(Value::Null);
    rows.as_array()
        .map(|rows| {
            rows.iter()
                .filter(|r| r.get("name").and_then(Value::as_str).is_some_and(|n| n.starts_with(&prefix)))
                .filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Every push on `volume`, newest first — what `history` lists, which is exactly the set a person
/// may delete by hand.
async fn snapshots(c: &Ctx, volume: &str) -> Vec<String> {
    let url = api(c, &format!("/v1/volumes/{volume}/history"));
    let rows = get(c, &url, &c.probe_jwt).await.unwrap_or(Value::Null);
    rows.as_array()
        .map(|rows| rows.iter().filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_string)).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The volume checks read the LISTING, because `/v1/volumes/{name}` is DELETE-only and a GET
    /// there answers 405 forever — a status no wait converges on, which is how `wt.delete` spent
    /// its entire budget. And they match on `name` (the ws/env id), not `display_name`: the two
    /// differ, and picking the wrong one is the trap that once made teardown's sweep find nothing.
    #[tokio::test]
    async fn a_volume_is_looked_for_in_the_listing_by_its_id() {
        let app = axum::Router::new().route(
            "/v1/volumes",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!([
                    { "name": "ws-kept", "display_name": "run-fast-1-clone" },
                ]))
            }),
        );
        let c = crate::testkit::ctx_against(app).await;
        let jwt = c.probe_jwt.clone();
        assert!(volume_listed(&c, &jwt, "ws-kept").await.expect("listed"));
        // The caller-chosen name is NOT what these match on.
        assert!(!volume_listed(&c, &jwt, "run-fast-1-clone").await.expect("listed"));
        assert!(!volume_listed(&c, &jwt, "ws-gone").await.expect("listed"));
        // And "gone" converges on absence rather than on a status.
        assert!(volume_gone(&c, &jwt, "ws-gone", Duration::from_millis(50), "x").await.is_ok());
        assert!(volume_gone(&c, &jwt, "ws-kept", Duration::from_millis(50), "x").await.is_err());
    }

    /// Stage 5 failing must not cost stage 7 its ids: every one is produced exactly once, skipped
    /// with the reason, so a broken workspace is one failure rather than eight.
    #[tokio::test]
    async fn lifecycle_skips_when_no_workspace_in_state() {
        let mut c = crate::testkit::ctx().await;
        run(&mut c).await;
        assert_eq!(c.steps.len(), IDS.len() + ENV_IDS.len() + DELETE_IDS.len());
        for id in IDS {
            let s = c.steps.iter().find(|s| s.slo_id == id).unwrap_or_else(|| panic!("{id}"));
            assert!(s.skipped && s.detail == "no workspace", "{s:?}");
        }
        // The two halves are independent: no environment costs the env ids nothing but their own
        // reason, and every id in the stage is still produced exactly once.
        for id in ENV_IDS {
            let s = c.steps.iter().find(|s| s.slo_id == id).unwrap_or_else(|| panic!("{id}"));
            assert!(s.skipped && s.detail == "no environment", "{s:?}");
        }
        // The two delete ids stand on the environment like the four above them, and skip with
        // their own reason rather than the workspace half's.
        for id in DELETE_IDS {
            let s = c.steps.iter().find(|s| s.slo_id == id).unwrap_or_else(|| panic!("{id}"));
            assert!(s.skipped && s.detail == "no environment", "{s:?}");
        }
        assert_eq!(c.failed(), 0, "a skip is not a failure");
    }
}
