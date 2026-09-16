//! Who has left a team, judged on the keys beat — the ONE decider for a departed member's team
//! data (it replaced `pause_departed_benches` and the deleted-team `prune_team_benches`, so a
//! deleted team's members follow the same grace as a removed member's).
//!
//! Lifecycle of one `(owner, team)` pair, read from the strict `Directory::membership`:
//!
//! | directory says            | stamp        | verdict                          |
//! |---------------------------|--------------|----------------------------------|
//! | error / unreachable       | any          | Keep — write nothing, stamp nothing |
//! | member (active or paused) | present      | Clear every removal annotation   |
//! | active member             | none, paused | Unpause the bench                |
//! | paused member             | none         | Pause the bench, NEVER stamp     |
//! | not a member / team gone  | none         | Stamp `removed-at` + `delete-after`, pause the bench |
//! | not a member / team gone  | < grace      | Keep                             |
//! | not a member / team gone  | ≥ grace, or `delete-now` | Due — mark any object still lacking `delete-after` |
//!
//! This module DELETES NOTHING. It only marks: `kloudlite.io/delete-after` (removed-at + grace, or
//! now for an admin's delete-now) is the whole hand-off, and the cluster controller's GC
//! (`bins/controller/src/gc.rs`, elected leader only, gated on the regional `memberRemovalDeletes`)
//! is the one thing that deletes a due Bench, team Workspace or SpaceEnvironment. The split keeps
//! the irreversible step in the process that holds the region's lease, and lets a re-add undo a
//! removal by clearing an annotation rather than racing a delete.
//!
//! Keep-biased: only a directory that ANSWERS "not a member" or "no such team" moves a pair toward a
//! mark, and the grace restarts from a fresh stamp whenever the stamp cannot be read. The latest
//! parseable stamp on any object of the pair counts. An existing `delete-after` is never moved later
//! or earlier by the beat; only delete-now (`removals.rs`) rewrites it, to now.
//!
//! Provenance: only annotations this beat wrote count — applied under the field manager
//! `kloudlite-membership` and read back from `managedFields` — and `delete-now` counts only beside
//! a `removed-at`. A restored, hand-written or agent-written stamp is no stamp: the pair is
//! restamped now, with a fresh grace and its audit row. `deploy/k3s/agent-admission.yaml` refuses
//! anyone but the api's account a change to any of the three annotations. Team and owner are
//! compared trimmed and lowercased, the directory's slug form, so `team: Acme` is never judged a
//! gone team.
//!
//! A beat and not `remove_member`: that route lives in the directory binary, which holds no
//! kubeconfig, and a beat also heals a removal or team delete that happened while this was down.

use super::{ApiState, Directory, Judged, MemberState};
use crate::crd;
use kube::api::{Api, Patch, PatchParams};
use kube::ResourceExt;
use serde_json::json;
use std::collections::BTreeSet;
use std::time::Duration;

pub const MEMBER_REMOVAL_GRACE: Duration = Duration::from_secs(7 * 24 * 3600);
pub const REMOVED_AT: &str = "kloudlite.io/removed-at";
pub const DELETE_NOW: &str = "kloudlite.io/delete-now";
/// RFC 3339; the controller's GC deletes the object once this is past (and only if the membership
/// manager wrote it).
pub const DELETE_AFTER: &str = "kloudlite.io/delete-after";
pub const MEMBERSHIP_FIELD_MANAGER: &str = "kloudlite-membership";
/// The controller's removal GC tick. Here, not in the controller, so the SLO probe's bound is
/// computed from the same numbers the GC runs on.
pub const GC_TICK_SECS: u64 = 60;
/// How far past `delete-after` an object must be before the GC deletes it: one keys beat plus a
/// margin. Every re-add clears the mark, but some paths (a superadmin grant from the admin process,
/// an accept whose immediate reconcile timed out) only clear on the api's keys beat.
pub const GC_DELETE_SLACK_SECS: u64 = super::keys::KEYS_RESYNC_SECS + 60;

#[derive(Debug, PartialEq)]
pub enum Verdict {
    Keep,
    Pause,
    Unpause,
    Stamp,
    Clear,
    Due,
}

/// Pure: the decision for one (owner, team) pair.
pub fn decide(judged: &Result<Judged, String>, stamped_at: Option<i64>, delete_now: bool, now: i64, currently_paused: bool) -> Verdict {
    let marked = stamped_at.is_some() || delete_now;
    match judged {
        Err(_) => Verdict::Keep,
        Ok(Judged::Member(_)) if marked => Verdict::Clear,
        Ok(Judged::Member(MemberState::Active)) if currently_paused => Verdict::Unpause,
        Ok(Judged::Member(MemberState::Paused)) if !currently_paused => Verdict::Pause,
        Ok(Judged::Member(_)) => Verdict::Keep,
        Ok(Judged::NotMember | Judged::TeamGone) => match stamped_at {
            _ if delete_now => Verdict::Due,
            None => Verdict::Stamp,
            Some(t) if now.saturating_sub(t) >= MEMBER_REMOVAL_GRACE.as_secs() as i64 => Verdict::Due,
            Some(_) => Verdict::Keep,
        },
    }
}


pub(super) struct Objects {
    pub(super) benches: Vec<crd::Bench>,
    pub(super) workspaces: Vec<crd::Workspace>,
    pub(super) spaces: Vec<crd::SpaceEnvironment>,
}

async fn list<K>(c: &kube::Client) -> Result<Vec<K>, String>
where
    K: kube::Resource<Scope = kube::core::ClusterResourceScope, DynamicType = ()> + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    match Api::<K>::all(c.clone()).list(&Default::default()).await {
        Ok(l) => Ok(l.items),
        // The CRD not applied: genuinely no objects of that kind.
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(Vec::new()),
        Err(e) => Err(format!("{}: {e}", K::kind(&()))),
    }
}

pub(super) async fn list_all(c: &kube::Client) -> Result<Objects, String> {
    Ok(Objects { benches: list(c).await?, workspaces: list(c).await?, spaces: list(c).await? })
}

pub(super) fn norm(x: &str) -> String {
    x.trim().to_ascii_lowercase()
}

/// A personal pair (team empty or the owner, in any case) is never a candidate.
pub(super) fn team_pair(owner: &str, team: &str) -> bool {
    let team = norm(team);
    !team.is_empty() && team != norm(owner)
}

pub(super) fn pairs(o: &Objects) -> BTreeSet<(String, String)> {
    let b = o.benches.iter().map(|x| (&x.spec.owner, &x.spec.team));
    let w = o.workspaces.iter().map(|x| (&x.spec.owner, &x.spec.team));
    let s = o.spaces.iter().map(|x| (&x.spec.owner, &x.spec.team));
    b.chain(w).chain(s).filter(|(o, t)| team_pair(o, t)).map(|(o, t)| (norm(o), norm(t))).collect()
}

/// The annotation, only when `kloudlite-membership` owns it in `managedFields`.
pub fn system_annotation(m: &kube::core::ObjectMeta, key: &str) -> Option<String> {
    let pointer = format!("/f:metadata/f:annotations/f:{}", key.replace('~', "~0").replace('/', "~1"));
    let owned = m.managed_fields.iter().flatten().any(|f| {
        f.manager.as_deref() == Some(MEMBERSHIP_FIELD_MANAGER) && f.fields_v1.as_ref().is_some_and(|v| v.0.pointer(&pointer).is_some())
    });
    owned.then(|| m.annotations.as_ref().and_then(|a| a.get(key)).cloned()).flatten()
}

/// `Ok` only when every pair was judged: the directory and kube client present, the listing read,
/// and no pair's judge failed. ONE failed pair is an `Err` too, not only a wholesale failure: the
/// keys beat renews its heartbeat Lease on `Ok` alone, and the controller's GC reads that Lease as
/// "re-adds are being cleared" — a pair this pass could not judge is exactly a re-add it did not clear.
pub async fn reconcile(s: &ApiState) -> Result<(), String> {
    let (Some(c), Some(dir)) = (s.kube.as_ref(), s.directory.as_ref()) else { return Err("no kube client or directory".into()) };
    let o = match list_all(c).await {
        Ok(o) => o,
        Err(error) => {
            tracing::warn!(%error, "membership.reconcile.listing.failed");
            return Err(error.to_string());
        }
    };
    let (mut failed, mut last) = (0usize, String::new());
    for (owner, team) in pairs(&o) {
        if let Err(e) = judge(s, c, dir.as_ref(), &o, &owner, &team).await {
            (failed, last) = (failed + 1, e);
        }
    }
    if failed > 0 {
        tracing::warn!(failed, error = %last, "membership.reconcile.skipped");
        return Err(format!("{failed} pairs unjudged: {last}"));
    }
    Ok(())
}

pub async fn reconcile_pair(s: &ApiState, owner: &str, team: &str) {
    // A pause or unpause lands here first on the replica that answered it: drop that pair's cached
    // `team_access` verdict so its own team-workspace verbs do not wait out the 30 s TTL.
    s.member_verdicts.lock().unwrap_or_else(|p| p.into_inner()).remove(&(norm(team), norm(owner)));
    let (Some(c), Some(dir)) = (s.kube.as_ref(), s.directory.as_ref()) else { return };
    if !team_pair(owner, team) {
        return;
    }
    let (owner, team) = (&norm(owner), &norm(team));
    match list_all(c).await {
        Ok(o) => {
            if let Err(error) = judge(s, c, dir.as_ref(), &o, owner, team).await {
                tracing::warn!(%owner, %team, %error, "membership.reconcile.skipped");
            }
        }
        Err(error) => tracing::warn!(%error, "membership.reconcile.listing.failed"),
    }
}

/// Every object of one normalised pair.
pub(super) fn metas_of<'a>(o: &'a Objects, owner: &str, team: &str) -> Vec<&'a kube::core::ObjectMeta> {
    let mine = |o_: &str, t: &str| norm(o_) == owner && norm(t) == team;
    let b = o.benches.iter().filter(|x| mine(&x.spec.owner, &x.spec.team)).map(|x| &x.metadata);
    let w = o.workspaces.iter().filter(|x| mine(&x.spec.owner, &x.spec.team)).map(|x| &x.metadata);
    let s = o.spaces.iter().filter(|x| mine(&x.spec.owner, &x.spec.team)).map(|x| &x.metadata);
    b.chain(w).chain(s).collect()
}

/// Latest wins, and an unparsable or foreign stamp is no stamp: all only ever push a delete later.
pub(super) fn latest_stamp(metas: &[&kube::core::ObjectMeta]) -> Option<i64> {
    metas
        .iter()
        .filter_map(|m| system_annotation(m, REMOVED_AT))
        .filter_map(|v| chrono::DateTime::parse_from_rfc3339(&v).ok())
        .map(|t| t.timestamp())
        .max()
}

#[derive(serde::Serialize, Debug, PartialEq)]
pub struct Removal {
    pub owner: String,
    pub team: String,
    pub removed_at: String,
    pub delete_at: String,
}

/// Pairs carrying a system-written stamp — the same provenance rule the beat decides from, so a
/// listing never shows a date the beat would not act on.
pub(super) fn removals(o: &Objects) -> Vec<Removal> {
    let at = |t: i64| chrono::DateTime::from_timestamp(t, 0).unwrap_or_default();
    pairs(o)
        .into_iter()
        .filter_map(|(owner, team)| {
            let t = latest_stamp(&metas_of(o, &owner, &team))?;
            Some(Removal { removed_at: at(t).to_rfc3339(), delete_at: (at(t) + MEMBER_REMOVAL_GRACE).to_rfc3339(), owner, team })
        })
        .collect()
}

/// `Err` only for an unreadable directory, which writes nothing.
async fn judge(s: &ApiState, c: &kube::Client, dir: &dyn Directory, o: &Objects, owner: &str, team: &str) -> Result<(), String> {
    let metas = metas_of(o, owner, team);
    let mine = |o_: &str, t: &str| norm(o_) == owner && norm(t) == team;
    let benches: Vec<_> = o.benches.iter().filter(|x| mine(&x.spec.owner, &x.spec.team)).collect();
    let workspaces: Vec<_> = o.workspaces.iter().filter(|x| mine(&x.spec.owner, &x.spec.team)).collect();
    let spaces: Vec<_> = o.spaces.iter().filter(|x| mine(&x.spec.owner, &x.spec.team)).collect();
    let ann = |m: &kube::core::ObjectMeta, k: &str| m.annotations.as_ref().and_then(|a| a.get(k)).cloned();
    let stamped_at = latest_stamp(&metas);
    let delete_now = stamped_at.is_some() && metas.iter().any(|m| system_annotation(m, DELETE_NOW).is_some());
    let full: Vec<_> = benches.iter().filter(|b| b.spec.access == crd::BenchAccess::Full).collect();
    let paused = !benches.is_empty() && full.is_empty();

    // ponytail: one uncached directory call per pair per beat (plus one per delete); cache per beat if pairs reach thousands.
    let judged = dir.membership(team, owner).await;
    let now = chrono::Utc::now().timestamp();
    let verdict = decide(&judged, stamped_at, delete_now, now, paused);
    let judged = judged?;

    let bapi: Api<crd::Bench> = Api::all(c.clone());
    let wapi: Api<crd::Workspace> = Api::all(c.clone());
    let sapi: Api<crd::SpaceEnvironment> = Api::all(c.clone());
    let access = |a: crd::BenchAccess| json!({"spec": {"access": a}});
    match verdict {
        // During the grace a bench someone set back to Full has no tools either.
        Verdict::Keep if matches!(judged, Judged::NotMember | Judged::TeamGone) => {
            let mut written = false;
            for b in &full {
                written |= write(&bapi, &b.name_any(), access(crd::BenchAccess::Paused), "membership.bench.paused").await;
            }
            // Same as `pause`: an already-minted tool token dies with the access, not 15 minutes later.
            if written {
                drop_tool_secret(c, owner, team).await;
            }
        }
        // A paused member's pair is re-converged on every beat, so a workspace started since the
        // pause stops again; every write is skipped when its object is already there.
        Verdict::Pause | Verdict::Keep if judged == Judged::Member(MemberState::Paused) => {
            pause(c, &bapi, &benches, &workspaces, owner, team).await;
        }
        Verdict::Keep => {}
        Verdict::Pause => {}
        // Starts nothing: the person starts what they want.
        Verdict::Unpause => {
            for b in &benches {
                write(&bapi, &b.name_any(), access(crd::BenchAccess::Full), "member.unpause.applied").await;
            }
        }
        Verdict::Clear => {
            // A member back from a removal gets their tools back too; nothing is started.
            for b in benches.iter().filter(|b| b.spec.access != crd::BenchAccess::Full && judged == Judged::Member(MemberState::Active)) {
                write(&bapi, &b.name_any(), access(crd::BenchAccess::Full), "member.unpause.applied").await;
            }
            let p = json!({"metadata": {"annotations": {REMOVED_AT: null, DELETE_NOW: null, DELETE_AFTER: null}}});
            let has = |m: &kube::core::ObjectMeta| [REMOVED_AT, DELETE_NOW, DELETE_AFTER].iter().any(|k| ann(m, k).is_some());
            let mut ok = false;
            for x in benches.iter().filter(|x| has(&x.metadata)) {
                ok |= write(&bapi, &x.name_any(), p.clone(), "membership.stamp.cleared").await;
            }
            for x in workspaces.iter().filter(|x| has(&x.metadata)) {
                ok |= write(&wapi, &x.name_any(), p.clone(), "membership.stamp.cleared").await;
            }
            for x in spaces.iter().filter(|x| has(&x.metadata)) {
                ok |= write(&sapi, &x.name_any(), p.clone(), "membership.stamp.cleared").await;
            }
            if ok {
                let detail = json!({"owner": owner, "team": team}).to_string();
                super::admin::audit(s, "system:membership", "member.removed.cleared", &format!("{team}/{owner}"), Some(detail), "ok").await;
            }
            // Removed → re-added → paused inside one beat (the pause route's own `reconcile_pair`
            // hits exactly this): clearing the stamps is only half of it, the pause still applies.
            if judged == Judged::Member(MemberState::Paused) {
                pause(c, &bapi, &benches, &workspaces, owner, team).await;
            }
        }
        Verdict::Stamp => {
            let at = chrono::DateTime::from_timestamp(now, 0).unwrap_or_default();
            let mut ok = false;
            // Every object of the pair carries the stamp, so whichever goes first (the controller's GC,
            // a 409, the person deleting their own bench) never restarts the grace or drops delete-now.
            let v = json!({REMOVED_AT: at.to_rfc3339(), DELETE_AFTER: (at + MEMBER_REMOVAL_GRACE).to_rfc3339()});
            for b in &benches {
                ok |= stamp(&bapi, &b.name_any(), v.clone()).await;
                // The pause as its own merge patch so the membership manager never owns `spec.access`.
                write(&bapi, &b.name_any(), access(crd::BenchAccess::Paused), "membership.bench.paused").await;
            }
            for w in &workspaces {
                ok |= stamp(&wapi, &w.name_any(), v.clone()).await;
            }
            for x in &spaces {
                ok |= stamp(&sapi, &x.name_any(), v.clone()).await;
            }
            if !ok {
                return Ok(());
            }
            let reason = if judged == Judged::TeamGone { "team_deleted" } else { "left_or_removed" };
            let delete_at = (at + MEMBER_REMOVAL_GRACE).to_rfc3339();
            let detail = json!({"owner": owner, "team": team, "reason": reason, "delete_at": delete_at}).to_string();
            super::admin::audit(s, "system:membership", "member.removed.judged", &format!("{team}/{owner}"), Some(detail), "ok").await;
        }
        // Past the grace (or delete-now): every object carries a due `delete-after` for the
        // controller's GC. Only an object still lacking one is written, so a due pair costs nothing
        // per beat and a delete-now's "now" is never pushed back to removed-at + grace.
        Verdict::Due => {
            let Some(t) = stamped_at else { return Ok(()) };
            let at = chrono::DateTime::from_timestamp(t, 0).unwrap_or_default();
            let after = if delete_now { chrono::DateTime::from_timestamp(now, 0).unwrap_or_default() } else { at + MEMBER_REMOVAL_GRACE };
            let mut v = json!({REMOVED_AT: at.to_rfc3339(), DELETE_AFTER: after.to_rfc3339()});
            if delete_now {
                v[DELETE_NOW] = json!("true");
            }
            let unmarked = |m: &kube::core::ObjectMeta| system_annotation(m, DELETE_AFTER).is_none();
            for x in benches.iter().filter(|x| unmarked(&x.metadata)) {
                stamp(&bapi, &x.name_any(), v.clone()).await;
            }
            for x in workspaces.iter().filter(|x| unmarked(&x.metadata)) {
                stamp(&wapi, &x.name_any(), v.clone()).await;
            }
            for x in spaces.iter().filter(|x| unmarked(&x.metadata)) {
                stamp(&sapi, &x.name_any(), v.clone()).await;
            }
        }
    }
    Ok(())
}

/// Bench access + desiredState, each team Workspace's desiredState, and the tool-token Secret —
/// nothing else is written or deleted.
async fn pause(c: &kube::Client, bapi: &Api<crd::Bench>, benches: &[&crd::Bench], workspaces: &[&crd::Workspace], owner: &str, team: &str) {
    use crd::{BenchAccess, DesiredState};
    let mut bench_written = false;
    for b in benches.iter().filter(|b| b.spec.access != BenchAccess::Paused || b.spec.desired_state != DesiredState::Stopped) {
        let p = json!({"spec": {"access": BenchAccess::Paused, "desiredState": DesiredState::Stopped}});
        bench_written |= write(bapi, &b.name_any(), p, "member.pause.applied").await;
    }
    for w in workspaces.iter().filter(|w| w.spec.desired_state != DesiredState::Stopped) {
        // The stop route's own path, so the agent cuts the stop sync point exactly as for a person's stop.
        match super::workspaces::set_desired::<crd::Workspace>(c, &w.name_any(), DesiredState::Stopped).await {
            Ok(()) => tracing::info!(%owner, %team, name = %w.name_any(), "member.pause.applied"),
            Err(r) => tracing::warn!(%owner, %team, name = %w.name_any(), status = %r.status(), "membership.write.failed"),
        }
    }
    // Already-minted tool tokens die now, not when their 15 minutes run out.
    if bench_written {
        drop_tool_secret(c, owner, team).await;
    }
}

async fn drop_tool_secret(c: &kube::Client, owner: &str, team: &str) {
    if let Err(e) = super::bench::delete_tool_secret(c, owner, team).await {
        tracing::warn!(%owner, %team, error = %e, "membership.tool_token.delete.failed");
    }
}

/// `true` only when the write landed.
async fn write<K>(api: &Api<K>, name: &str, patch: serde_json::Value, event: &'static str) -> bool
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    match api.patch(name, &PatchParams::default(), &Patch::Merge(&patch)).await {
        Ok(_) => {
            tracing::info!(%name, event);
            true
        }
        Err(kube::Error::Api(e)) if e.code == 404 => false,
        Err(error) => {
            tracing::warn!(%name, event, %error, "membership.write.failed");
            false
        }
    }
}

/// The stamp by server-side apply, so `managedFields` records who wrote it (see module docs).
pub(super) async fn stamp<K>(api: &Api<K>, name: &str, annotations: serde_json::Value) -> bool
where
    K: kube::Resource<DynamicType = ()> + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let body = json!({"apiVersion": K::api_version(&()), "kind": K::kind(&()), "metadata": {"name": name, "annotations": annotations}});
    match api.patch(name, &PatchParams::apply(MEMBERSHIP_FIELD_MANAGER).force(), &Patch::Apply(&body)).await {
        Ok(_) => {
            tracing::info!(%name, "membership.stamped");
            true
        }
        Err(kube::Error::Api(e)) if e.code == 404 => false,
        Err(error) => {
            tracing::warn!(%name, %error, "membership.write.failed");
            false
        }
    }
}


#[cfg(test)]
mod tests;
