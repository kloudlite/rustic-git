//! `team.member.removed.cleanup` and `team.member.removed.dir_down`: what a removal deletes, what it
//! keeps, and that an unreadable directory deletes nothing.
//!
//! Its own team (`run-{id}-rmv`), so removing the second probe member touches nothing another
//! drill stands on. The removal and `delete-now` run BEFORE the step, and the step judges whichever
//! half the region's `memberRemovalDeletes` (`ClusterSettings`, echoed as delete-now's
//! `deletes_enabled`) selects: on, the controller's GC has deleted the pair within `GC_BOUND`; off,
//! every object still exists carrying a due, api-written `kloudlite.io/delete-after`.
//!
//! The bench folder `{pool}/homes/.benches/{team}/{owner}` is not visible from the probe: no pod
//! the probe can exec into mounts the share's root. The Bench object is gone only once the agent
//! has removed that folder and dropped `kloudlite.io/bench-folder`, so a vanished Bench is the
//! finalizer cleared, and the folder's absence is inferred from it.

use std::time::Instant;

use super::*;
use crate::stages::{call, clip, raw};
use kloudlite_workspaces::api::membership::{system_annotation, DELETE_AFTER, GC_DELETE_SLACK_SECS, GC_TICK_SECS, REMOVED_AT};
use kloudlite_workspaces::crd;
use kube::api::ListParams;

pub(crate) const CLEANUP_ID: &str = "team.member.removed.cleanup";
pub(crate) const DIR_DOWN_ID: &str = "team.member.removed.dir_down";
/// The drills suite has no hook that points the api's directory at a black hole; not built here.
pub(crate) const NO_DIR_HOOK: &str =
    "no directory fault hook in the drills suite: nothing points the api's directory address at a black hole for one beat";
/// Two membership beats (the keys beat, 300 s).
const TWO_BEATS: Duration = Duration::from_secs(600);
const READY_WAIT: Duration = Duration::from_secs(300);
/// How long teardown waits for the workspace before deleting the snapshot it was based on.
const WS_GONE: Duration = Duration::from_secs(60);
/// The GC's slack past delete-after, two of its ticks (one to land in, one for a pass that listed
/// just before), and 180 s for the finalizers that run after the delete: the agent's bench-folder
/// removal and the workspace's sync-point cleanup. 660 s today.
const GC_BOUND: Duration = Duration::from_secs(GC_DELETE_SLACK_SECS + 2 * GC_TICK_SECS + FINALIZER_HEADROOM_SECS);
const FINALIZER_HEADROOM_SECS: u64 = 180;
const CLEANUP_CEILING: Duration = Duration::from_secs(GC_BOUND.as_secs() + 120);

fn team(c: &Ctx) -> String {
    format!("{}-rmv", c.prefix())
}

struct Prep {
    ws: String,
    volume: String,
    snap: String,
}

fn kube(c: &Ctx) -> Result<kube::Client> {
    c.kube.clone().ok_or_else(|| anyhow!("no kubeconfig"))
}

/// Team with the member in it, their bench ready, a team workspace with one READY push.
async fn prepare(c: &Ctx, team: &str) -> Result<Prep> {
    post(c, &api(c, "/v1/teams"), &c.probe_jwt, json!({ "slug": team, "name": "kloudlite slo removal", "region": c.cfg.region }))
        .await
        .context("could not create the removal team")?;
    join(c, team).await?;
    post(c, &api(c, "/v1/bench"), &c.other_jwt, json!({ "team": team, "region": c.cfg.region })).await.context("could not create the member's bench")?;
    let bench = api(c, &format!("/v1/bench?team={team}"));
    poll_json(c, &bench, &c.other_jwt, READY_WAIT, |v| v.get("phase").and_then(Value::as_str) == Some("ready"))
        .await
        .context("the member's bench never became ready")?;
    let body = json!({ "team": team, "name": format!("{team}-ws"), "region": c.cfg.region, "quota_gb": 1, "packages": [] });
    let doc = post(c, &api(c, "/v1/workspaces"), &c.other_jwt, body).await.context("could not create the team workspace")?;
    let ws = doc.get("id").and_then(Value::as_str).ok_or_else(|| anyhow!("the answer carried no workspace id"))?.to_string();
    poll_json(c, &api(c, &format!("/v1/workspaces/{ws}")), &c.other_jwt, READY_WAIT, |v| v.get("state").and_then(Value::as_str) == Some("ready"))
        .await
        .context("the team workspace never became ready")?;
    // Named after the workspace until a push publishes a pointer (`stage 5`'s own reasoning).
    let volume = kube::Api::<crd::Workspace>::all(kube(c)?)
        .get(&ws)
        .await
        .ok()
        .and_then(|w| w.status.and_then(|s| s.volume_ref))
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| ws.clone());
    let pushed = post(c, &api(c, &format!("/v1/workspaces/{ws}/push")), &c.other_jwt, json!({ "message": team })).await.context("could not push")?;
    let snap = pushed.get("id").and_then(Value::as_str).ok_or_else(|| anyhow!("the push answered no snapshot id"))?.to_string();
    let history = api(c, &format!("/v1/volumes/{volume}/history"));
    poll_json(c, &history, &c.other_jwt, READY_WAIT, |v| super::super::workspace::row_ready(v, &snap))
        .await
        .context("the snapshot never turned ready")?;
    Ok(Prep { ws, volume, snap })
}

async fn join(c: &Ctx, team: &str) -> Result<()> {
    let invite = json!({ "email": c.other_email, "role": "member" });
    let issued = post(c, &api(c, &format!("/v1/teams/{team}/invites")), &c.probe_jwt, invite).await.context("could not invite")?;
    let token = issued.get("token").and_then(Value::as_str).filter(|t| !t.is_empty()).ok_or_else(|| anyhow!("no invite token"))?;
    post(c, &api(c, &format!("/v1/invites/{token}/accept")), &c.other_jwt, Value::Null).await.context("the member could not join")?;
    Ok(())
}

/// Remove, wait for the stamp on the Bench, then ask for deletion now. `Ok(false)`: deletes are off.
async fn remove(c: &Ctx, team: &str) -> Result<bool> {
    call(c, reqwest::Method::DELETE, &api(c, &format!("/v1/teams/{team}/members/{}", c.other_email)), &c.probe_jwt, None)
        .await
        .context("could not remove the member")?;
    let benches = kube::Api::<crd::Bench>::all(kube(c)?);
    let id = crd::bench_id(&c.other_user, team);
    let start = Instant::now();
    loop {
        let b = benches.get(&id).await.context("could not read the member's Bench")?;
        if b.metadata.annotations.as_ref().is_some_and(|a| a.contains_key(REMOVED_AT)) {
            break;
        }
        if start.elapsed() >= TWO_BEATS {
            return Err(anyhow!("the Bench carried no {REMOVED_AT} within {} s", TWO_BEATS.as_secs()));
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    let who = &c.other_user;
    let url = api(c, &format!("/v1/teams/{team}/members/{who}/delete-now"));
    let v = post(c, &url, &c.admin_jwt(), json!({ "person": who, "team": team })).await.context("delete-now failed")?;
    Ok(v["deletes_enabled"] != false)
}

/// `Err` names the first thing that is not yet as decision 11 says.
async fn cleaned(c: &Ctx, team: &str, p: &Prep) -> Result<()> {
    let k = kube(c)?;
    let owner = &c.other_user;
    if kube::Api::<crd::Bench>::all(k.clone()).get_opt(&crd::bench_id(owner, team)).await?.is_some() {
        return Err(anyhow!("the Bench is still there (its bench-folder finalizer has not cleared)"));
    }
    if kube::Api::<crd::Workspace>::all(k.clone()).get_opt(&p.ws).await?.is_some() {
        return Err(anyhow!("the team Workspace {} is still there", p.ws));
    }
    let spaces = kube::Api::<crd::SpaceEnvironment>::all(k.clone()).list(&ListParams::default()).await?;
    if spaces.items.iter().any(|s| &s.spec.owner == owner && s.spec.team == team) {
        return Err(anyhow!("a SpaceEnvironment of the pair is still there"));
    }
    let snaps = kube::Api::<crd::Snapshot>::all(k.clone()).list(&ListParams::default()).await?;
    if let Some(s) = snaps.items.iter().find(|s| s.spec.worktree == p.ws && s.spec.transient) {
        return Err(anyhow!("sync point {} is still there", s.metadata.name.as_deref().unwrap_or("?")));
    }
    if !snaps.items.iter().any(|s| s.metadata.name.as_deref() == Some(p.snap.as_str())) {
        return Err(anyhow!("the pushed snapshot {} was deleted, not kept", p.snap));
    }
    if kube::Api::<crd::Volume>::all(k).get_opt(&p.volume).await?.is_none() {
        return Err(anyhow!("the pushed snapshot's volume {} was deleted, not kept detached", p.volume));
    }
    Ok(())
}

async fn audited(c: &Ctx, team: &str) -> Result<()> {
    let target = format!("{team}%2F{}", c.other_user);
    for action in ["member.removed.judged", "member.removed.delete_now"] {
        let url = admin(c, &format!("/admin/audit?action={action}&target={target}"));
        poll_json(c, &url, &c.admin_jwt(), Duration::from_secs(30), |v| v.get("rows").and_then(Value::as_array).is_some_and(|r| !r.is_empty()))
            .await
            .with_context(|| format!("no {action} audit row"))?;
    }
    Ok(())
}

/// Deletes off: every object of the pair is still there and carries a due, api-written mark.
async fn marked_due(c: &Ctx, team: &str, p: &Prep) -> Result<()> {
    let k = kube(c)?;
    let now = chrono::Utc::now();
    let due = |m: &kube::core::ObjectMeta, what: &str| -> Result<()> {
        let at = system_annotation(m, DELETE_AFTER).ok_or_else(|| anyhow!("{what} carries no api-written {DELETE_AFTER}"))?;
        let at = chrono::DateTime::parse_from_rfc3339(&at).with_context(|| format!("{what}'s {DELETE_AFTER} {at} is not RFC 3339"))?;
        if at > now {
            return Err(anyhow!("{what}'s {DELETE_AFTER} {at} is not due after delete-now"));
        }
        Ok(())
    };
    let b = kube::Api::<crd::Bench>::all(k.clone()).get_opt(&crd::bench_id(&c.other_user, team)).await?.ok_or_else(|| anyhow!("the Bench was deleted with deletes off"))?;
    due(&b.metadata, "the Bench")?;
    let w = kube::Api::<crd::Workspace>::all(k).get_opt(&p.ws).await?.ok_or_else(|| anyhow!("the team Workspace was deleted with deletes off"))?;
    due(&w.metadata, "the team Workspace")
}

pub(crate) async fn member_removed(c: &mut Ctx) {
    cleanup(c).await;
    c.skip(DIR_DOWN_ID, NO_DIR_HOOK);
}

async fn cleanup(c: &mut Ctx) {
    if c.kube.is_none() {
        return c.skip(CLEANUP_ID, "no kubeconfig");
    }
    let team = team(c);
    let prep = match prepare(c, &team).await {
        Ok(p) => p,
        Err(e) => {
            let why = format!("before the removal: {e:#}");
            c.step(CLEANUP_ID, CLEANUP_CEILING, move |_| async move { Err(anyhow!("{why}")) }.boxed()).await;
            return teardown(c, &team, None).await;
        }
    };
    let deletes = match remove(c, &team).await {
        Ok(on) => on,
        Err(e) => {
            let why = format!("removing the member: {e:#}");
            c.step(CLEANUP_ID, CLEANUP_CEILING, move |_| async move { Err(anyhow!("{why}")) }.boxed()).await;
            return teardown(c, &team, Some(&prep)).await;
        }
    };
    let (t, p) = (team.clone(), Prep { ws: prep.ws.clone(), volume: prep.volume.clone(), snap: prep.snap.clone() });
    c.step(CLEANUP_ID, CLEANUP_CEILING, move |c| {
        async move {
            if !deletes {
                marked_due(c, &t, &p).await?;
                return audited(c, &t).await;
            }
            let start = Instant::now();
            loop {
                match cleaned(c, &t, &p).await {
                    Ok(()) => break,
                    Err(e) if start.elapsed() >= GC_BOUND => return Err(e.context(format!("not cleaned within {} s", GC_BOUND.as_secs()))),
                    Err(_) => tokio::time::sleep(Duration::from_secs(10)).await,
                }
            }
            audited(c, &t).await?;
            join(c, &t).await.context("could not re-add the person")?;
            let (status, body) = raw(c, reqwest::Method::GET, &api(c, &format!("/v1/bench?team={t}")), &c.other_jwt, None, &[]).await?;
            if status != reqwest::StatusCode::NOT_FOUND || !body.contains("no bench") {
                return Err(anyhow!("the re-added person finds a bench ({status}): {}", clip(&body)));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
    teardown(c, &team, Some(&prep)).await;
}

/// Best effort; the `run-` prefix sweep takes what this misses (the per-member team-workspace sweep
/// in `stages::sweep`). The member's own objects go as the member, BEFORE the removal, while their
/// token still reaches the team; the snapshot waits for the workspace, whose base it is.
// ponytail: a crash after the member's removal leaves their workspace to the controller's GC on its
// delete-now mark (the member's token no longer reaches it); it leaks while memberRemovalDeletes is off.
async fn teardown(c: &Ctx, team: &str, p: Option<&Prep>) {
    if let Some(p) = p {
        let ws = api(c, &format!("/v1/workspaces/{}", p.ws));
        let _ = call(c, reqwest::Method::DELETE, &ws, &c.other_jwt, None).await;
        let start = Instant::now();
        while start.elapsed() < WS_GONE {
            match raw(c, reqwest::Method::GET, &ws, &c.other_jwt, None, &[]).await {
                Ok((status, _)) if status == reqwest::StatusCode::NOT_FOUND => break,
                _ => tokio::time::sleep(Duration::from_secs(3)).await,
            }
        }
        let snap = api(c, &format!("/v1/volumes/{}/snapshots/{}", p.volume, p.snap));
        if let Err(e) = call(c, reqwest::Method::DELETE, &snap, &c.other_jwt, None).await {
            tracing::warn!(kind = "snapshot", op = "delete", name = %p.snap, error = %format!("{e:#}"), "slo.teardown.failed");
        }
    }
    let _ = call(c, reqwest::Method::DELETE, &api(c, &format!("/v1/teams/{team}/members/{}", c.other_email)), &c.probe_jwt, None).await;
    if let Err(e) = call(c, reqwest::Method::DELETE, &api(c, &format!("/v1/teams/{team}")), &c.probe_jwt, None).await {
        tracing::warn!(kind = "team", op = "delete", name = %team, error = %format!("{e:#}"), "slo.teardown.failed");
        return;
    }
    super::super::env_intercept::delete_members_now(c, team).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The catalogue's "Within 11 minutes" is this bound; a change to either constant must move both.
    #[test]
    fn the_bound_is_the_gc_constants_plus_finalizer_headroom() {
        assert_eq!(GC_BOUND.as_secs(), 660);
        assert!(kloudlite_workspaces::slo::catalogue::find(CLEANUP_ID).unwrap().sli.starts_with("Within 11 minutes"));
    }

    #[test]
    fn both_ids_are_catalogued_monthly() {
        for id in [CLEANUP_ID, DIR_DOWN_ID] {
            let s = kloudlite_workspaces::slo::catalogue::find(id).unwrap();
            assert_eq!(s.stage, "13 · Monthly");
        }
    }

    #[tokio::test]
    async fn no_kubeconfig_skips_each_id_once() {
        let mut c = crate::testkit::ctx().await;
        c.kube = None;
        member_removed(&mut c).await;
        for id in [CLEANUP_ID, DIR_DOWN_ID] {
            let rows: Vec<_> = c.steps.iter().filter(|s| s.slo_id == id).collect();
            assert!(rows.len() == 1 && rows[0].skipped, "{id}");
        }
    }
}
