//! Stage 7 · Lifecycle: stop, replicate, start, restore, and the refusals and collection that
//! bound what a person can destroy.
//!
//! Every id here is about the workspace stage 5 created and pushed, so with no workspace the whole
//! stage skips with one reason.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;
use kloudlite_git_workspaces::crd;
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
const ORPHAN_CEILING: Duration = Duration::from_secs(60);

/// Every id in this stage, in journey order — the list a missing precondition skips.
const IDS: [&str; 7] = [
    "ws.stop.p95",
    "ws.replicated",
    "ws.start.p95",
    "ws.restore",
    "vol.refusals",
    "vol.detached.restorable",
    "vol.orphan.collected",
];

pub async fn run(c: &mut Ctx) {
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
    detached_restorable(c, &volume, &snapshot).await;
    orphan_collected(c, &volume).await;
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
                v.pointer("/replicated/status").and_then(Value::as_str) == Some("True")
            })
            .await
            .context("no other node reported holding the final sync point")
        }
        .boxed()
    })
    .await;
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
async fn detached_restorable(c: &mut Ctx, volume: &str, snapshot: &str) {
    // Every working copy on the volume goes first — that is what "detached" means, and the
    // deletes are the journey's own cleanup either way.
    let names = worktrees(c, volume).await;
    for id in &names {
        let url = api(c, &format!("/v1/workspaces/{id}"));
        let _ = super::call(c, reqwest::Method::DELETE, &url, &c.probe_jwt.clone(), None).await;
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
                for id in worktrees(c, &volume).await {
                    let url = api(c, &format!("/v1/workspaces/{id}"));
                    let _ = super::call(c, reqwest::Method::DELETE, &url, &c.probe_jwt.clone(), None).await;
                }
                for id in snapshots(c, &volume).await {
                    let url = api(c, &format!("/v1/volumes/{volume}/snapshots/{id}"));
                    let _ = super::call(c, reqwest::Method::DELETE, &url, &c.probe_jwt.clone(), None).await;
                }
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

/// Every workspace of the probe's whose working copy is on `volume`. A listing that fails is an
/// empty list: the steps that use it re-read on their next pass.
async fn worktrees(c: &Ctx, volume: &str) -> Vec<String> {
    let url = api(c, "/v1/workspaces");
    let rows = get(c, &url, &c.probe_jwt).await.unwrap_or(Value::Null);
    rows.as_array()
        .map(|rows| {
            rows.iter()
                // The doc's `volume` is `vol/{owner}/{volume}` — the pointer, whose last segment is
                // the `Volume` CR's own name.
                .filter(|r| {
                    r.get("volume")
                        .and_then(Value::as_str)
                        .and_then(|v| v.rsplit('/').next())
                        == Some(volume)
                })
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

    /// Stage 5 failing must not cost stage 7 its ids: every one is produced exactly once, skipped
    /// with the reason, so a broken workspace is one failure rather than eight.
    #[tokio::test]
    async fn lifecycle_skips_when_no_workspace_in_state() {
        let mut c = crate::testkit::ctx().await;
        run(&mut c).await;
        assert_eq!(c.steps.len(), IDS.len());
        for id in IDS {
            let s = c.steps.iter().find(|s| s.slo_id == id).unwrap_or_else(|| panic!("{id}"));
            assert!(s.skipped && s.detail == "no workspace", "{s:?}");
        }
        assert_eq!(c.failed(), 0, "a skip is not a failure");
    }
}
