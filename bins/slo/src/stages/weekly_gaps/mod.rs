//! The 2026-09-06 coverage review's weekly ids: the deploy, the two registry paths real clients
//! use and nothing walked, the sweeps that touch user data, and the gateway's own ceilings.
//!
//! A second file beside `weekly` for the same reason `experience_gaps2` sits beside
//! `experience_gaps`. Every rule of the stage still holds: one sample per id on every path, a
//! missing precondition is a SKIP, and anything that changes the fleet is paired with its undo.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams, Patch, PatchParams};
use serde_json::{json, Value};

use super::{admin, api, get, poll_json, post};
use crate::ctx::Ctx;
use crate::{drill, tools};

mod roll;
pub(crate) use roll::*;
mod drain;
pub(crate) use drain::*;
mod registry;
pub(crate) use registry::*;
mod agent;
pub(crate) use agent::*;
mod gateway;
pub(crate) use gateway::*;


/// The namespace and workload names on AKS. Repeated rather than derived: what these steps are
/// about is the objects the deployment actually carries.
pub(super) const CENTRAL_NS: &str = "kloudlite";

pub(super) const SRV: &str = "kloudlite-srv";
/// The srv PODS, which do not carry the StatefulSet's name as their `app`: `deploy/kloudlite.yaml`
/// labels them `app: kloudlite, role: server`. Selecting `app=kloudlite-srv` matched nothing, so
/// `srv.drain.handover` found "no srv pod to drain" and `roll.zero.errors` tracked no pod at all
/// and passed with nothing observed.
pub(super) const SRV_PODS: &str = "app=kloudlite,role=server";
/// The srv container's `http` port (`deploy/kloudlite.yaml`), the one `/healthz` answers on and
/// the one the network policy opens to everybody. This dialled 3000 — the WEB app's port — so
/// `srv.drain.handover` reported the leaving pod as never having answered at all.
pub(super) const SRV_HTTP_PORT: u16 = 8080;


/// The roll's own budget: a StatefulSet of a handful of pods, each with a 90 s grace period for
/// its handover. The step gets a minute on top, as every step with an undo does.
pub(super) const ROLL_CAP: Duration = Duration::from_secs(600);

pub(super) const DRAIN_CAP: Duration = Duration::from_secs(60);

pub(super) const READ_CEILING: Duration = Duration::from_secs(60);

pub(super) const SWEEP_CAP: Duration = Duration::from_secs(180);
/// `gw.caps` alone: two 30 s `ssh` runs for the replay half, eleven session mints, ten tunnels
/// spawned a beat apart and a 20 s wait on the eleventh. 300 s is that with room, and the id is
/// availability — a step cut off by its own ceiling would report the probe, not the gateway.
pub(super) const TUNNEL_CEILING: Duration = Duration::from_secs(300);


pub(super) fn step_cap(body: Duration) -> Duration {
    body + Duration::from_secs(60)
}


/// `cold` is `ws.cold.profile`'s workspace, the one workspace of this run that is still there by
/// stage 12: stage 7's lifecycle verbs DELETE `State::workspace` and its volume (`wt.delete`,
/// `snap.delete`), so the three ids here that used to read them answered 404 every time.
pub async fn run(c: &mut Ctx, cold: Option<&str>) {
    roll_zero_errors(c).await;
    drain_handover(c).await;
    moved_image(c).await;
    blob_session(c).await;
    gc_packs(c).await;
    limits(c).await;
    workload_roll(c).await;
    spread(c, cold).await;
    retain(c, cold).await;
    janitor(c).await;
    lanes(c).await;
    gw_caps(c, cold).await;
}

// ── the deploy ──────────────────────────────────────────────────────────


/// How many times a 421 is re-issued before it counts against the id. Three tries over a second is
/// more patience than a git client has and far less than the roll takes.
pub(super) const RETRIES: usize = 3;


/// How often the loop pushes, rather than every beat: a push is a pack negotiation, and one every
/// two seconds would measure the probe's own git rather than the fleet.
pub(super) const PUSH_EVERY: Duration = Duration::from_secs(10);


/// The push half of the loop: one commit onto its own branch, over HTTP, each time it is asked.
pub(crate) struct Pushing {
    work: std::path::PathBuf,
    url: String,
    branch: String,
    n: std::sync::atomic::AtomicUsize,
}


impl Pushing {
    async fn once(&self, c: &Ctx) -> Result<()> {
        let i = self.n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::fs::write(self.work.join("roll.txt"), format!("{i}\n")).context("could not write")?;
        super::git::git(c, vec!["add".into(), "-A".into()], Some(&self.work)).await?;
        super::git::git(c, vec!["commit".into(), "-q".into(), "-m".into(), format!("roll {i}")], Some(&self.work)).await?;
        let refspec = format!("HEAD:refs/heads/{}", self.branch);
        super::git::git(c, super::git::authed(c, &["push", "-q", &self.url, &refspec]), Some(&self.work)).await.map(|_| ())
    }
}


/// A megabyte past `manifests::MAX_MANIFEST` (4 MiB). Repeated rather than imported: what is being
/// asserted is the limit the DEPLOYED tier holds, and a constant shared with it would move with it.
pub(super) const OVER_MANIFEST: usize = 5 * 1024 * 1024;

// ── the sweeps over user data ───────────────────────────────────────────


/// How long the eleventh tunnel gets to be refused. An ssh that is still connected after this has
/// been let through — the gateway refuses at dial, not lazily.
pub(super) const OVER_CAP: Duration = Duration::from_secs(20);


/// `tunnel::MAX_PER_WS`. Repeated rather than imported — the gateway is a binary crate with no
/// library target, and the number is part of its contract with a person's editor.
pub(super) const MAX_PER_WS: usize = 10;

// ── the console's own write ─────────────────────────────────────────────


#[cfg(test)]
mod tests {
    use super::*;

    /// Every id this file owns is produced exactly once with nothing reachable.
    #[tokio::test]
    async fn every_id_is_produced_once_with_nothing_reachable() {
        let mut c = crate::testkit::ctx().await;
        c.kube = None;
        run(&mut c, None).await;
        let ids: Vec<&str> = c.steps.iter().map(|s| s.slo_id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "roll.zero.errors",
                "srv.drain.handover",
                "reg.moved.image",
                "reg.blob.session",
                "git.gc.packs",
                "reg.limits",
                "admin.workload.roll",
                "ws.spread",
                "snap.retain",
                "agent.janitor",
                "srv.lanes",
                "gw.caps",
            ]
        );
    }
}
