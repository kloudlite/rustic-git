//! One push, one object, one reconciler.
//!
//! The CR is the REQUEST; the snapshot is the registry commit record its reconciler writes to the
//! server tier — durable, content-addressed, cross-region, and what a cold clone or a restore on
//! another node reads. Deleting the CR deletes no data.
//!
//! The idempotency guard is `Ctx::running`, keyed by the request's uid, exactly as for Volume work;
//! the Volume's `ws_lock` inside the engine serialises this against a clone-running or a restore on
//! the same disk.

use crate::controller::{patch_status, running_contains, Ctx, Done, ReconcileErr, TICK};
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{finalizer, Event as FinalizerEvent};
use kube::{Api, Resource, ResourceExt};
use rustic_git_workspaces::crd;
use std::sync::Arc;

/// The finalizer wrapper, exactly the shape `reconcile_volume` has: a delete routes every pass to
/// `cleanup_snapshot` until it returns, which is what makes waiting for an in-flight push free.
pub async fn reconcile_snapshot(r: Arc<crd::SnapshotRequest>, ctx: Arc<Ctx>) -> Result<Action, ReconcileErr> {
    let api: Api<crd::SnapshotRequest> = Api::all(ctx.client.clone());
    finalizer(&api, crd::SNAPSHOT_FINALIZER, r, |event| async {
        match event {
            FinalizerEvent::Cleanup(r) => cleanup_snapshot(&r, &ctx).await,
            FinalizerEvent::Apply(r) => apply_snapshot(&r, &ctx).await,
        }
    })
    .await
    .map_err(|e| ReconcileErr(e.to_string()))
}

/// Whether this agent owns the request, by reading the named Volume's node.
///
/// `Ok(None)` means "not mine, or not resolvable yet" and the caller does NOTHING — no status, no
/// condition. Every agent watches every request, so a second agent writing this object's status is
/// the multi-writer problem the design exists to remove. A Volume that does not exist yet is the
/// same answer: the `SnapshotRequest`→`Volume` watch wakes us when it appears.
async fn my_volume(r: &crd::SnapshotRequest, ctx: &Arc<Ctx>) -> Result<Option<crd::Volume>, ReconcileErr> {
    let api: Api<crd::Volume> = Api::all(ctx.client.clone());
    match api.get_opt(&r.spec.volume).await? {
        Some(v) if v.spec.node_name == ctx.node => Ok(Some(v)),
        _ => Ok(None),
    }
}

pub async fn apply_snapshot(r: &crd::SnapshotRequest, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    let uid = r.uid().unwrap_or_default();
    let generation = r.meta().generation.unwrap_or(0);
    let phase = r.status.as_ref().map(|s| s.phase).unwrap_or(crd::Phase::Pending);
    // A request is never re-run past `done` or `error`: the bytes are already in the registry (or
    // the user has been told to push again), and a second run appends a commit nobody asked for.
    // Checked BEFORE the Volume read, so a finished request costs no API call at all.
    if matches!(phase, crd::Phase::Done | crd::Phase::Error) && !running_contains(ctx, &uid) {
        return Ok(Action::await_change());
    }
    let Some(vol) = my_volume(r, ctx).await? else {
        return Ok(Action::await_change());
    };

    let (finished, still_running) = {
        let mut running = ctx.running.lock().unwrap_or_else(|p| p.into_inner());
        match running.get(&uid) {
            Some((_, h)) if h.is_finished() => (running.remove(&uid), false),
            Some(_) => (None, true),
            None => (None, false),
        }
    };
    if still_running {
        write_status(r, working(generation), ctx).await?;
        return Ok(Action::requeue(TICK));
    }
    // The restart case: `working` in status, nothing in the map. The map died with the process, and
    // there is no way to tell "crashed before starting" from "crashed mid-send" — so this is NOT
    // re-run. `engine.push_env` would take a fresh snapshot and register a SECOND commit record for
    // one user push. Marked permanently failed; the user pushes again.
    // ponytail: fails instead of resuming. The engine already leaves an internal `unpushed` stage
    // mark for crash recovery — resume from it once the engine can answer "is this lineage entry
    // already registered", and this branch becomes a retry.
    if finished.is_none() && phase == crd::Phase::Working {
        let st = serde_json::json!({
            "phase": crd::Phase::Error,
            "observedGeneration": generation,
            "conditions": [crd::condition(
                "Ready", false, "AgentRestarted",
                "the agent restarted while this push was in flight; push again", generation,
            )],
        });
        write_status(r, st, ctx).await?;
        return Ok(Action::await_change());
    }
    if let Some((_, handle)) = finished {
        let outcome = handle.await.unwrap_or_else(|e| Err(format!("push panicked: {e}")));
        return match outcome {
            Ok(done) => {
                let st = serde_json::json!({
                    "phase": crd::Phase::Done,
                    "observedGeneration": generation,
                    // One push produces ONE identity: `PushOut::layer` is both the commit record's
                    // id and the lineage's new tip. They are separate STATUS fields because a
                    // future push that lands on top of an existing record would make them differ;
                    // neither is read back out of the other here.
                    "snapshotId": done.lineage_tip,
                    "lineageTip": done.lineage_tip,
                    "at": k8s_openapi::jiff::Timestamp::now().to_string(),
                    "conditions": [crd::condition("Ready", true, "Pushed", "the snapshot record is in the registry", generation)],
                });
                write_status(r, st, ctx).await?;
                // Nothing is written on the Volume. "The newest snapshot of this volume" is a query
                // over these objects by the `rustic-git.io/volume` label — a second controller
                // force-applying the Volume's status under the same field manager would have its
                // field pruned by the Volume reconciler's very next pass.
                Ok(Action::await_change())
            }
            // A failed push is `error` with the reason, and the user pushes again. Not a retry
            // loop: a btrfs send that failed once fails the same way at RETRY, and the log line is
            // indistinguishable from a healthy idle agent.
            Err(e) => {
                let st = serde_json::json!({
                    "phase": crd::Phase::Error,
                    "observedGeneration": generation,
                    "conditions": [crd::condition("Ready", false, "PushFailed", &e, generation)],
                });
                write_status(r, st, ctx).await?;
                Ok(Action::await_change())
            }
        };
    }

    // Start it, on its own OS thread: `Engine::push_env` blocks on `ws_lock`'s synchronous
    // `libc::flock`, and a lock wait on the shared reactor would freeze every other workspace.
    let engine = ctx.engine.clone();
    let volume = r.spec.volume.clone();
    let message = r.spec.message.clone();
    // `spec.owner` on the Volume is the truth; the request's `rustic-git.io/owner` label is a view
    // of it, and this repo never reads a label as authority.
    let owner = vol.spec.owner.clone();
    let handle = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
        rt.block_on(async {
            // `push_env` rather than `push`: the VOLUME is what gets pushed, keyed by id alone.
            let out = engine
                .push_env(&owner, &volume, &serde_json::Value::Null, message.as_deref())
                .await
                .map_err(|e| e.to_string())?;
            Ok(Done { phase: crd::Phase::Done, lineage_tip: Some(out.layer) })
        })
    });
    ctx.running.lock().unwrap_or_else(|p| p.into_inner()).insert(uid, (generation, handle));
    write_status(r, working(generation), ctx).await?;
    Ok(Action::requeue(TICK))
}

/// Wait for an in-flight push, then let the object go.
///
/// The same shape and the same reason as `cleanup_volume`: reclaiming or abandoning while a
/// `btrfs send` is still reading destroys the source mid-stream, and the finalizer makes waiting
/// cost one tick. The finished handle must be DRAINED here, not merely observed — while an object
/// is deleting the finalizer routes every pass to this arm, so `apply_snapshot` never runs and
/// nothing else would ever remove the entry.
pub async fn cleanup_snapshot(r: &crd::SnapshotRequest, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    let uid = r.uid().unwrap_or_default();
    let mut running = ctx.running.lock().unwrap_or_else(|p| p.into_inner());
    match running.get(&uid) {
        Some((_, h)) if h.is_finished() => {
            running.remove(&uid);
        }
        Some(_) => {
            tracing::info!(request = %r.name_any(), "delete waiting for an in-flight push");
            return Ok(Action::requeue(TICK));
        }
        None => {}
    }
    // Nothing on disk or in the registry is reclaimed by this: the record is content-addressed and
    // shared, and deleting the wish never deletes the bytes.
    Ok(Action::await_change())
}

fn working(generation: i64) -> serde_json::Value {
    serde_json::json!({
        "phase": crd::Phase::Working,
        "observedGeneration": generation,
        "conditions": [crd::condition("Progressing", true, "Working", "btrfs snapshot and upload in flight", generation)],
    })
}

async fn write_status(r: &crd::SnapshotRequest, st: serde_json::Value, ctx: &Arc<Ctx>) -> Result<(), ReconcileErr> {
    // Same guard as everywhere else: a status write that is not a change is a watch event that
    // triggers itself, which is an outage rather than a warning.
    if let Some(cur) = &r.status {
        if serde_json::to_value(cur).is_ok_and(|c| c["phase"] == st["phase"] && c["snapshotId"] == st["snapshotId"]) {
            return Ok(());
        }
    }
    let api: Api<crd::SnapshotRequest> = Api::all(ctx.client.clone());
    patch_status(&api, &r.name_any(), "SnapshotRequest", st).await
}
