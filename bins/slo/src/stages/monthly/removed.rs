//! `team.member.removed.cleanup` and `team.member.removed.dir_down`: what a removal deletes, what it
//! keeps, and that an unreadable directory deletes nothing.
//!
//! Its own team (`run-{id}-rmv`), so removing the second probe member touches nothing another
//! drill stands on. The removal and `delete-now` run BEFORE the step: `memberRemovalDeletes` is off
//! on a fleet until somebody decides otherwise, and then `delete-now` only marks — a skip naming the
//! setting, never a failure the product did not commit.
//!
//! The bench folder `{pool}/homes/.benches/{team}/{owner}` is not visible from the probe: no pod
//! the probe can exec into mounts the share's root. The Bench object is gone only once the agent
//! has removed that folder and dropped `kloudlite.io/bench-folder`, so a vanished Bench is the
//! finalizer cleared, and the folder's absence is inferred from it.

use std::time::Instant;

use super::*;
use crate::stages::{call, clip, raw};
use kloudlite_workspaces::api::membership::REMOVED_AT;
use kloudlite_workspaces::crd;
use kube::api::ListParams;

pub(crate) const CLEANUP_ID: &str = "team.member.removed.cleanup";
pub(crate) const DIR_DOWN_ID: &str = "team.member.removed.dir_down";
/// The drills suite has no hook that points the api's directory at a black hole; not built here.
pub(crate) const NO_DIR_HOOK: &str =
    "no directory fault hook in the drills suite: nothing points the api's directory address at a black hole for one beat (filed)";
/// Two membership beats (the keys beat, 300 s).
const TWO_BEATS: Duration = Duration::from_secs(600);
const READY_WAIT: Duration = Duration::from_secs(300);
const CLEANUP_CEILING: Duration = Duration::from_secs(TWO_BEATS.as_secs() + 120);

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
    for action in ["member.removed.judged", "member.removed.delete_now", "member.removed.cleanup"] {
        let url = admin(c, &format!("/admin/audit?action={action}&target={target}"));
        poll_json(c, &url, &c.admin_jwt(), Duration::from_secs(30), |v| v.get("rows").and_then(Value::as_array).is_some_and(|r| !r.is_empty()))
            .await
            .with_context(|| format!("no {action} audit row"))?;
    }
    Ok(())
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
    match remove(c, &team).await {
        Ok(true) => {}
        Ok(false) => {
            c.skip(CLEANUP_ID, "memberRemovalDeletes is off on this fleet: delete-now only marked the pair");
            return teardown(c, &team, Some(&prep)).await;
        }
        Err(e) => {
            let why = format!("removing the member: {e:#}");
            c.step(CLEANUP_ID, CLEANUP_CEILING, move |_| async move { Err(anyhow!("{why}")) }.boxed()).await;
            return teardown(c, &team, Some(&prep)).await;
        }
    }
    let (t, p) = (team.clone(), Prep { ws: prep.ws.clone(), volume: prep.volume.clone(), snap: prep.snap.clone() });
    c.step(CLEANUP_ID, CLEANUP_CEILING, move |c| {
        async move {
            let start = Instant::now();
            loop {
                match cleaned(c, &t, &p).await {
                    Ok(()) => break,
                    Err(e) if start.elapsed() >= TWO_BEATS => return Err(e.context(format!("not cleaned within {} s", TWO_BEATS.as_secs()))),
                    Err(_) => tokio::time::sleep(Duration::from_secs(10)).await,
                }
            }
            audited(c, &t).await?;
            join(c, &t).await.context("could not re-add the person")?;
            let (status, body) = raw(c, reqwest::Method::GET, &api(c, &format!("/v1/bench?team={t}")), &c.other_jwt, None, &[]).await?;
            if status != reqwest::StatusCode::NOT_FOUND {
                return Err(anyhow!("the re-added person finds a bench ({status}): {}", clip(&body)));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
    teardown(c, &team, Some(&prep)).await;
}

/// Best effort; the `run-` prefix sweep takes what this misses, the kept volume included.
async fn teardown(c: &Ctx, team: &str, p: Option<&Prep>) {
    let _ = call(c, reqwest::Method::DELETE, &api(c, &format!("/v1/teams/{team}/members/{}", c.other_email)), &c.probe_jwt, None).await;
    if let Some(p) = p {
        let _ = call(c, reqwest::Method::DELETE, &api(c, &format!("/v1/workspaces/{}", p.ws)), &c.other_jwt, None).await;
        let _ = call(c, reqwest::Method::DELETE, &api(c, &format!("/v1/volumes/{}/snapshots/{}", p.volume, p.snap)), &c.other_jwt, None).await;
    }
    if let Err(e) = call(c, reqwest::Method::DELETE, &api(c, &format!("/v1/teams/{team}")), &c.probe_jwt, None).await {
        tracing::warn!(kind = "team", op = "delete", name = %team, error = %format!("{e:#}"), "slo.teardown.failed");
        return;
    }
    super::super::env_intercept::delete_members_now(c, team).await;
}

#[cfg(test)]
mod tests {
    use super::*;

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
