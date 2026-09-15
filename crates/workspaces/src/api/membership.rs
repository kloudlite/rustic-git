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
            for b in &full {
                write(&bapi, &b.name_any(), access(crd::BenchAccess::Paused), "membership.bench.paused").await;
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
        if let Err(e) = super::bench::delete_tool_secret(c, owner, team).await {
            tracing::warn!(%owner, %team, error = %e, "membership.tool_token.delete.failed");
        }
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
mod tests {
    use super::*;
    use crate::api::{OwnerMaterial, TeamRole};
    use crate::kube_test::{get, mock_client, patch, Recorder};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const API: &str = "/apis/kloudlite.io/v1alpha1";

    /// `down` is unreadable, `gone` does not exist, `paula` is paused, `alice` is in `acme`.
    #[derive(Default)]
    struct Fake {
        asked: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Directory for Fake {
        async fn teams_for(&self, _u: &str) -> Vec<String> {
            Vec::new()
        }
        async fn is_live(&self, _j: &str) -> bool {
            false
        }
        async fn for_owner(&self, _o: &str) -> Option<OwnerMaterial> {
            None
        }
        async fn authorized_keys_for_owner(&self, _o: &str) -> Option<String> {
            None
        }
        async fn owners_of(&self, _e: &str) -> Vec<String> {
            Vec::new()
        }
        async fn team_role(&self, u: &str, t: &str) -> Option<TeamRole> {
            match (u, t) {
                ("ann", "acme") => Some(TeamRole::Admin),
                ("mem", "acme") => Some(TeamRole::Member),
                _ => None,
            }
        }
        async fn is_team(&self, _s: &str) -> bool {
            false
        }
        async fn ensure_user(&self, _e: &str, _n: &str, _u: &str) -> Result<(), String> {
            Err("no".into())
        }
        async fn add_superadmin(&self, _e: &str, _b: &str) -> Result<(), String> {
            Err("no".into())
        }
        async fn is_superadmin(&self, u: &str) -> Result<bool, String> {
            match u {
                "root" => Ok(true),
                "flaky" => Err("directory unreachable".into()),
                _ => Ok(false),
            }
        }
        async fn membership(&self, team: &str, user: &str) -> Result<Judged, String> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            match (team, user) {
                ("down", _) => Err("directory unreachable".into()),
                ("gone", _) => Ok(Judged::TeamGone),
                (_, "paula") => Ok(Judged::Member(MemberState::Paused)),
                ("acme", "alice") => Ok(Judged::Member(MemberState::Active)),
                // Removed at the first ask, back by the second: the re-judge must see it.
                (_, "rex") if self.asked.load(Ordering::SeqCst) > 1 => Ok(Judged::Member(MemberState::Active)),
                _ => Ok(Judged::NotMember),
            }
        }
    }

    fn bench(owner: &str, team: &str, access: &str, stamp: Option<&str>) -> serde_json::Value {
        let mut b = json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "Bench",
            "metadata": {"name": crd::bench_id(owner, team)},
            "spec": {"owner": owner, "team": team, "image": "i", "desiredState": "running", "access": access,
                     "resources": {"cpuRequest": "1", "cpuLimit": "1", "memoryRequest": "1Gi", "memoryLimit": "1Gi"}}
        });
        if let Some(t) = stamp {
            b["metadata"]["annotations"] = json!({REMOVED_AT: t});
            b["metadata"]["managedFields"] = owned(&[REMOVED_AT]);
        }
        b
    }

    fn owned(keys: &[&str]) -> serde_json::Value {
        let ann: serde_json::Map<_, _> = keys.iter().map(|k| (format!("f:{k}"), json!({}))).collect();
        json!([{"manager": MEMBERSHIP_FIELD_MANAGER, "operation": "Apply", "apiVersion": "kloudlite.io/v1alpha1", "fieldsType": "FieldsV1", "fieldsV1": {"f:metadata": {"f:annotations": ann}}}])
    }

    fn benches(items: Vec<serde_json::Value>) -> crate::kube_test::Route {
        get(format!("{API}/benches"), json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "BenchList", "metadata": {}, "items": items}))
    }

    fn path(owner: &str, team: &str) -> String {
        format!("{API}/benches/{}", crd::bench_id(owner, team))
    }

    fn setup(mut routes: Vec<crate::kube_test::Route>, patched: &[(&str, &str)]) -> (ApiState, Recorder, Arc<Fake>) {
        routes.extend(patched.iter().map(|(o, t)| patch(path(o, t), bench(o, t, "full", None))));
        let (client, rec) = mock_client(routes);
        let dir = Arc::new(Fake::default());
        let jwt = Arc::new(kloudlite_core::jwt::Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
        (ApiState::new(jwt).with_kube(client).with_directory(dir.clone()), rec, dir)
    }

    fn writes(rec: &Recorder) -> Vec<String> {
        rec.calls().into_iter().filter(|c| !c.starts_with("GET ")).collect()
    }

    const OLD: &str = "2020-01-01T00:00:00Z";

    fn with_meta(mut v: serde_json::Value, name: &str, stamp: Option<&str>, finalizer: bool) -> serde_json::Value {
        v["metadata"]["name"] = json!(name);
        v["metadata"]["uid"] = json!(format!("uid-{name}"));
        v["metadata"]["resourceVersion"] = json!("7");
        if let Some(t) = stamp {
            v["metadata"]["annotations"] = json!({REMOVED_AT: t});
            v["metadata"]["managedFields"] = owned(&[REMOVED_AT]);
        }
        if finalizer {
            v["metadata"]["finalizers"] = json!([crd::BENCH_FOLDER_FINALIZER]);
        }
        v
    }

    fn ws(name: &str, owner: &str, team: &str) -> serde_json::Value {
        let spec = json!({"owner": owner, "team": team, "name": name, "region": "r", "image": "i", "desiredState": "running"});
        with_meta(json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace", "metadata": {}, "spec": spec}), name, None, false)
    }

    fn space(owner: &str, team: &str) -> serde_json::Value {
        let v = serde_json::to_value(crd::space_environment(owner, team, "env-1")).unwrap();
        with_meta(v, &crd::space_name(owner, team), None, false)
    }

    fn list_of(kind: &str, plural: &str, items: Vec<serde_json::Value>) -> crate::kube_test::Route {
        get(format!("{API}/{plural}"), json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": format!("{kind}List"), "metadata": {}, "items": items}))
    }

    fn del(path: String, status: u16) -> crate::kube_test::Route {
        let body = if status == 200 { json!({"kind": "Status", "apiVersion": "v1", "status": "Success", "code": 200}) } else { serde_json::to_value(kube::core::Status::failure("x", "Conflict").with_code(status)).unwrap() };
        crate::kube_test::Route { method: "DELETE", path, status, body }
    }

    /// A removed `(owner, acme)` past the grace: a finalized bench, two workspaces, a space choice.
    fn removed(owner: &str) -> Vec<crate::kube_test::Route> {
        let b = with_meta(bench(owner, "acme", "paused", None), &crd::bench_id(owner, "acme"), Some(OLD), true);
        vec![
            benches(vec![b]),
            list_of("Workspace", "workspaces", vec![ws("w1", owner, "acme"), ws("w2", owner, "acme"), ws("w9", owner, "")]),
            list_of("SpaceEnvironment", "spaceenvironments", vec![space(owner, "acme")]),
        ]
    }

    fn deletes(rec: &Recorder) -> Vec<String> {
        rec.calls().into_iter().filter(|c| c.starts_with("DELETE ")).collect()
    }

    const BEAT: &str = "/apis/coordination.k8s.io/v1/namespaces/kube-system/leases/kloudlite-keys-beat";

    fn beat_patches(rec: &Recorder) -> usize {
        rec.calls().iter().filter(|c| *c == &format!("PATCH {BEAT}")).count()
    }

    /// The heartbeat means "re-adds are being cleared": renewed after a whole judged pass, never
    /// after a pass that could not read the directory, the listing, or any one pair.
    #[tokio::test]
    async fn the_keys_beat_lease_is_renewed_only_after_a_fully_judged_pass() {
        let lease = || patch(BEAT, json!({"apiVersion": "coordination.k8s.io/v1", "kind": "Lease", "metadata": {"name": "kloudlite-keys-beat"}}));
        let (s, rec, _) = setup(vec![benches(vec![bench("alice", "acme", "full", None)]), lease()], &[]);
        crate::api::keys::membership_beat(&s).await;
        assert_eq!(beat_patches(&rec), 1, "success: {:?}", rec.calls());

        let (s, rec, _) = setup(vec![benches(vec![bench("alice", "down", "full", None)]), lease()], &[]);
        crate::api::keys::membership_beat(&s).await;
        assert_eq!(beat_patches(&rec), 0, "every judge erroring: {:?}", rec.calls());

        let failing = crate::kube_test::Route { method: "GET", path: format!("{API}/benches"), status: 500, body: json!({}) };
        let (s, rec, _) = setup(vec![failing, lease()], &[]);
        crate::api::keys::membership_beat(&s).await;
        assert_eq!(beat_patches(&rec), 0, "listing failed: {:?}", rec.calls());

        let (client, rec) = mock_client(vec![benches(vec![]), lease()]);
        let jwt = Arc::new(kloudlite_core::jwt::Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
        crate::api::keys::membership_beat(&ApiState::new(jwt).with_kube(client)).await;
        assert_eq!(beat_patches(&rec), 0, "no directory: {:?}", rec.calls());
    }

    #[tokio::test]
    async fn a_mixed_case_team_is_judged_by_its_slug() {
        let (s, rec, dir) = setup(vec![benches(vec![bench("alice", " Acme", "full", None)])], &[]);
        let _ = reconcile(&s).await;
        assert_eq!(dir.asked.load(Ordering::SeqCst), 1);
        assert!(writes(&rec).is_empty(), "a live member of acme is kept: {:?}", writes(&rec));
    }

    #[tokio::test]
    async fn a_stamp_this_system_did_not_write_restarts_the_grace() {
        let mut routes = removed("bob");
        let mut b = bench("bob", "acme", "paused", None);
        b["metadata"]["annotations"] = json!({REMOVED_AT: OLD, DELETE_NOW: "yes"});
        b["metadata"]["finalizers"] = json!([crd::BENCH_FOLDER_FINALIZER]);
        routes[0] = benches(vec![b]);
        routes[1] = list_of("Workspace", "workspaces", vec![]);
        let (s, rec, _) = setup(routes, &[("bob", "acme")]);
        let _ = reconcile(&s).await;
        assert!(deletes(&rec).is_empty(), "{:?}", deletes(&rec));
        let sent = rec.sent("PATCH", &path("bob", "acme"));
        assert_ne!(sent[0]["metadata"]["annotations"][REMOVED_AT], json!(OLD), "a fresh stamp");
    }

    #[tokio::test]
    async fn a_workspace_only_pair_is_stamped_on_its_workspaces() {
        let routes = vec![list_of("Workspace", "workspaces", vec![ws("w1", "bob", "acme")]), patch(format!("{API}/workspaces/w1"), ws("w1", "bob", "acme"))];
        let (s, rec, _) = setup(routes, &[]);
        let _ = reconcile(&s).await;
        let sent = rec.sent("PATCH", &format!("{API}/workspaces/w1"));
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["kind"], "Workspace");
        assert!(sent[0]["metadata"]["annotations"][REMOVED_AT].is_string());
    }

    #[tokio::test]
    async fn reconciling_a_pair_forgets_its_cached_verdict() {
        let (s, _, _) = setup(vec![], &[]);
        let put = |o: &str| s.member_verdicts.lock().unwrap().insert(("acme".into(), o.into()), (std::time::Instant::now(), Judged::NotMember));
        put("bob");
        put("alice");
        reconcile_pair(&s, "Bob", "ACME").await;
        let left: Vec<_> = s.member_verdicts.lock().unwrap().keys().cloned().collect();
        assert_eq!(left, vec![("acme".to_string(), "alice".to_string())]);
    }

    #[tokio::test]
    async fn every_object_of_the_pair_is_stamped_so_a_deleted_bench_keeps_the_clock() {
        let routes = vec![
            benches(vec![bench("bob", "acme", "full", None)]),
            list_of("Workspace", "workspaces", vec![ws("w1", "bob", "acme")]),
            patch(format!("{API}/workspaces/w1"), ws("w1", "bob", "acme")),
        ];
        let (s, rec, _) = setup(routes, &[("bob", "acme")]);
        let _ = reconcile(&s).await;
        assert!(rec.sent("PATCH", &path("bob", "acme"))[0]["metadata"]["annotations"][REMOVED_AT].is_string());
        let w = rec.sent("PATCH", &format!("{API}/workspaces/w1"));
        assert_eq!(w.len(), 1);
        assert!(w[0]["metadata"]["annotations"][REMOVED_AT].is_string());
        assert!(rec.requests().iter().filter(|r| r.contains("/workspaces/w1")).all(|r| r.contains("fieldManager=kloudlite-membership")), "{:?}", rec.requests());
    }

    #[tokio::test]
    async fn a_bench_set_back_to_full_during_the_grace_is_paused_again() {
        let fresh = chrono::Utc::now().to_rfc3339();
        let (s, rec, _) = setup(vec![benches(vec![bench("bob", "acme", "full", Some(&fresh))])], &[("bob", "acme")]);
        let _ = reconcile(&s).await;
        assert_eq!(rec.sent("PATCH", &path("bob", "acme")), vec![json!({"spec": {"access": "paused"}})]);
    }

    #[tokio::test]
    async fn a_due_pair_is_marked_on_every_object_and_nothing_is_deleted() {
        let (s, rec, _) = setup(removed("bob"), &[]);
        let _ = reconcile(&s).await;
        assert!(deletes(&rec).is_empty(), "{:?}", deletes(&rec));
        let paths = [path("bob", "acme"), format!("{API}/workspaces/w1"), format!("{API}/workspaces/w2"), format!("{API}/spaceenvironments/{}", crd::space_name("bob", "acme"))];
        for p in &paths {
            let sent = rec.sent("PATCH", p);
            assert_eq!(sent.len(), 1, "{p}: {:?}", rec.calls());
            assert_eq!(sent[0]["metadata"]["annotations"], json!({REMOVED_AT: "2020-01-01T00:00:00+00:00", DELETE_AFTER: "2020-01-08T00:00:00+00:00"}), "{p}");
        }
        assert!(!writes(&rec).iter().any(|c| c.contains("/workspaces/w9")), "a personal workspace is never marked");
    }

    #[tokio::test]
    async fn a_due_pair_already_marked_writes_nothing() {
        let mut b = with_meta(bench("bob", "acme", "paused", None), &crd::bench_id("bob", "acme"), None, true);
        b["metadata"]["annotations"] = json!({REMOVED_AT: OLD, DELETE_AFTER: "2020-01-08T00:00:00Z"});
        b["metadata"]["managedFields"] = owned(&[REMOVED_AT, DELETE_AFTER]);
        let (s, rec, _) = setup(vec![benches(vec![b])], &[]);
        let _ = reconcile(&s).await;
        assert!(writes(&rec).is_empty(), "{:?}", writes(&rec));
    }


    #[test]
    fn decide_covers_every_row() {
        use Judged::*;
        use MemberState::*;
        let g = MEMBER_REMOVAL_GRACE.as_secs() as i64;
        let now = 10 * g;
        let err: Result<Judged, String> = Err("x".into());
        assert_eq!(decide(&err, Some(0), true, now, false), Verdict::Keep);
        assert_eq!(decide(&Ok(Member(Active)), Some(now), false, now, false), Verdict::Clear);
        assert_eq!(decide(&Ok(Member(Active)), None, true, now, false), Verdict::Clear);
        assert_eq!(decide(&Ok(Member(Active)), None, false, now, true), Verdict::Unpause);
        assert_eq!(decide(&Ok(Member(Active)), None, false, now, false), Verdict::Keep);
        assert_eq!(decide(&Ok(Member(Paused)), None, false, now, false), Verdict::Pause);
        assert_eq!(decide(&Ok(Member(Paused)), None, false, now, true), Verdict::Keep);
        assert_eq!(decide(&Ok(Member(Paused)), Some(0), false, now, true), Verdict::Clear);
        for j in [NotMember, TeamGone] {
            assert_eq!(decide(&Ok(j), None, false, now, false), Verdict::Stamp);
            assert_eq!(decide(&Ok(j), Some(now - g + 1), false, now, true), Verdict::Keep);
            assert_eq!(decide(&Ok(j), Some(now - g), false, now, true), Verdict::Due);
            assert_eq!(decide(&Ok(j), Some(now), true, now, true), Verdict::Due);
            assert_eq!(decide(&Ok(j), None, true, now, false), Verdict::Due);
        }
    }

    #[tokio::test]
    async fn a_directory_error_changes_nothing() {
        let (s, rec, _) = setup(vec![benches(vec![bench("bob", "down", "full", Some("2020-01-01T00:00:00Z"))])], &[("bob", "down")]);
        let _ = reconcile(&s).await;
        assert!(writes(&rec).is_empty(), "{:?}", writes(&rec));
    }

    #[tokio::test]
    async fn a_personal_pair_is_never_a_candidate() {
        let (s, rec, dir) = setup(vec![benches(vec![bench("alice", "Alice", "full", None), bench("carol", "", "full", None)])], &[]);
        let _ = reconcile(&s).await;
        assert_eq!(dir.asked.load(Ordering::SeqCst), 0);
        assert!(writes(&rec).is_empty(), "{:?}", writes(&rec));
    }

    #[tokio::test]
    async fn a_removed_pair_is_stamped_once_and_the_second_beat_writes_nothing() {
        let routes = vec![benches(vec![bench("bob", "acme", "full", None)]), benches(vec![bench("bob", "acme", "paused", Some("2026-09-15T00:00:00Z"))])];
        let (s, rec, _) = setup(routes, &[("bob", "acme")]);
        let _ = reconcile(&s).await;
        let sent = rec.sent("PATCH", &path("bob", "acme"));
        assert_eq!(sent.len(), 2);
        let a = &sent[0]["metadata"]["annotations"];
        let at = chrono::DateTime::parse_from_rfc3339(a[REMOVED_AT].as_str().unwrap()).unwrap();
        assert_eq!(a[DELETE_AFTER], json!((at + MEMBER_REMOVAL_GRACE).to_rfc3339()), "grace starts with the stamp");
        assert!(rec.requests().iter().any(|r| r.contains("fieldManager=kloudlite-membership")), "{:?}", rec.requests());
        assert_eq!(sent[1], json!({"spec": {"access": "paused"}}));
        let _ = reconcile(&s).await;
        assert_eq!(rec.sent("PATCH", &path("bob", "acme")).len(), 2, "the second beat writes nothing");
    }

    #[tokio::test]
    async fn a_deleted_team_waits_out_the_grace_too() {
        let (s, rec, _) = setup(vec![benches(vec![bench("bob", "gone", "paused", Some(&chrono::Utc::now().to_rfc3339()))])], &[("bob", "gone")]);
        let _ = reconcile(&s).await;
        assert!(writes(&rec).is_empty(), "{:?}", writes(&rec));
    }

    #[tokio::test]
    async fn a_readd_during_the_grace_clears_the_stamp() {
        let (s, rec, _) = setup(vec![benches(vec![bench("alice", "acme", "full", Some("2026-09-15T00:00:00Z"))])], &[("alice", "acme")]);
        let _ = reconcile(&s).await;
        assert_eq!(rec.sent("PATCH", &path("alice", "acme")), vec![json!({"metadata": {"annotations": {REMOVED_AT: null, DELETE_NOW: null, DELETE_AFTER: null}}})]);
    }

    #[tokio::test]
    async fn a_paused_member_is_never_stamped() {
        let (s, rec, _) = setup(vec![benches(vec![bench("paula", "acme", "full", None)])], &[("paula", "acme")]);
        let _ = reconcile(&s).await;
        assert_eq!(rec.sent("PATCH", &path("paula", "acme")), vec![json!({"spec": {"access": "paused", "desiredState": "stopped"}})]);
        assert!(rec.calls().iter().all(|c| !c.contains("removed-at")));
    }

    fn secret_path(owner: &str, team: &str) -> String {
        format!("/api/v1/namespaces/{}/secrets/{}", crd::ws_namespace(owner, team), crate::k8s::BENCH_TOOL_SECRET)
    }

    fn paused_pair(bench_json: serde_json::Value, team_ws: serde_json::Value) -> Vec<crate::kube_test::Route> {
        vec![
            benches(vec![bench_json]),
            list_of("Workspace", "workspaces", vec![team_ws, ws("w9", "paula", "")]),
            patch(format!("{API}/workspaces/w1"), ws("w1", "paula", "acme")),
            del(secret_path("paula", "acme"), 200),
        ]
    }

    #[tokio::test]
    async fn pause_stops_bench_and_team_workspaces_and_marks_access() {
        let (s, rec, _) = setup(paused_pair(bench("paula", "acme", "full", None), ws("w1", "paula", "acme")), &[("paula", "acme")]);
        let _ = reconcile(&s).await;
        assert_eq!(rec.sent("PATCH", &path("paula", "acme")), vec![json!({"spec": {"access": "paused", "desiredState": "stopped"}})]);
        assert_eq!(rec.sent("PATCH", &format!("{API}/workspaces/w1")), vec![json!({"spec": {"desiredState": "stopped"}})]);
        assert_eq!(deletes(&rec), vec![format!("DELETE {}", secret_path("paula", "acme"))], "only the tool token is deleted");
    }

    #[tokio::test]
    async fn pause_twice_writes_nothing() {
        let mut b = bench("paula", "acme", "paused", None);
        b["spec"]["desiredState"] = json!("stopped");
        let mut w = ws("w1", "paula", "acme");
        w["spec"]["desiredState"] = json!("stopped");
        let (s, rec, _) = setup(paused_pair(b, w), &[]);
        let _ = reconcile(&s).await;
        assert!(writes(&rec).is_empty(), "{:?}", writes(&rec));
    }

    #[tokio::test]
    async fn unpause_sets_full_and_starts_nothing() {
        let mut b = bench("alice", "acme", "paused", None);
        b["spec"]["desiredState"] = json!("stopped");
        let mut w = ws("w1", "alice", "acme");
        w["spec"]["desiredState"] = json!("stopped");
        let (s, rec, _) = setup(vec![benches(vec![b]), list_of("Workspace", "workspaces", vec![w])], &[("alice", "acme")]);
        let _ = reconcile(&s).await;
        assert_eq!(writes(&rec), vec![format!("PATCH {}", path("alice", "acme"))]);
        assert_eq!(rec.sent("PATCH", &path("alice", "acme")), vec![json!({"spec": {"access": "full"}})]);
    }

    #[tokio::test]
    async fn a_readd_after_removal_gets_full_access_back() {
        let (s, rec, _) = setup(vec![benches(vec![bench("alice", "acme", "paused", Some("2026-09-15T00:00:00Z"))])], &[("alice", "acme")]);
        let _ = reconcile(&s).await;
        assert!(rec.sent("PATCH", &path("alice", "acme")).contains(&json!({"spec": {"access": "full"}})));
    }

    #[tokio::test]
    async fn a_personal_workspace_of_a_paused_member_is_untouched() {
        let (s, rec, _) = setup(paused_pair(bench("paula", "acme", "full", None), ws("w1", "paula", "acme")), &[("paula", "acme")]);
        let _ = reconcile(&s).await;
        assert!(!writes(&rec).iter().any(|c| c.contains("/workspaces/w9")), "{:?}", writes(&rec));
    }

    fn who(name: &str) -> crate::api::Caller {
        crate::api::Caller { name: name.into(), superadmin: false, parent: None, scope: None, jti8: None }
    }

    fn confirm(person: &str, team: &str) -> crate::api::removals::Confirm {
        crate::api::removals::Confirm { person: person.into(), team: team.into() }
    }

    #[tokio::test]
    async fn delete_now_requires_admin_and_both_names() {
        use crate::api::removals::delete_now;
        let (s, rec, _) = setup(vec![], &[]);
        assert_eq!(delete_now(&s, &who("ann"), "acme", "bob", &confirm("bob", "other")).await.status(), 400);
        assert_eq!(delete_now(&s, &who("ann"), "acme", "bob", &confirm("bo", "acme")).await.status(), 400);
        assert_eq!(delete_now(&s, &who("mem"), "acme", "bob", &confirm("bob", "acme")).await.status(), 403);
        assert_eq!(delete_now(&s, &who("eve"), "acme", "bob", &confirm("bob", "acme")).await.status(), 404);
        assert!(rec.calls().is_empty(), "{:?}", rec.calls());
    }

    #[tokio::test]
    async fn delete_now_trusts_the_superadmin_row_not_the_claim() {
        use crate::api::removals::delete_now;
        let (s, rec, _) = setup(vec![], &[]);
        let claim = |n: &str| crate::api::Caller { superadmin: true, ..who(n) };
        assert_eq!(delete_now(&s, &claim("eve"), "acme", "bob", &confirm("bob", "acme")).await.status(), 404, "a claim with no row");
        assert_eq!(delete_now(&s, &claim("flaky"), "acme", "bob", &confirm("bob", "acme")).await.status(), 503, "fails closed");
        assert!(rec.calls().is_empty(), "{:?}", rec.calls());
    }

    #[tokio::test]
    async fn delete_now_refuses_an_unstamped_pair() {
        let (s, rec, _) = setup(vec![benches(vec![bench("alice", "acme", "full", None)])], &[]);
        let r = crate::api::removals::delete_now(&s, &who("ann"), "acme", "alice", &confirm("alice", "acme")).await;
        assert_eq!(r.status(), 409);
        assert!(writes(&rec).is_empty(), "{:?}", writes(&rec));
    }

    #[tokio::test]
    async fn delete_now_refuses_a_readded_member_with_a_stale_stamp() {
        let (s, rec, _) = setup(vec![benches(vec![bench("alice", "acme", "full", Some(OLD))])], &[]);
        let r = crate::api::removals::delete_now(&s, &who("ann"), "acme", "alice", &confirm("alice", "acme")).await;
        assert_eq!(r.status(), 409);
        assert!(rec.calls().is_empty(), "judged before any read or write: {:?}", rec.calls());
    }

    #[tokio::test]
    async fn delete_now_on_an_unreadable_directory_is_503_and_writes_nothing() {
        let (s, rec, _) = setup(vec![benches(vec![bench("bob", "down", "paused", Some(OLD))])], &[]);
        let admin = crate::api::Caller { superadmin: true, ..who("root") };
        let r = crate::api::removals::delete_now(&s, &admin, "down", "bob", &confirm("bob", "down")).await;
        assert_eq!(r.status(), 503);
        assert!(rec.calls().is_empty(), "{:?}", rec.calls());
    }

    #[tokio::test]
    async fn delete_now_marks_delete_after_now_and_deletes_nothing() {
        let fresh = chrono::Utc::now().to_rfc3339();
        let name = crd::bench_id("bob", "acme");
        let b = with_meta(bench("bob", "acme", "paused", None), &name, Some(&fresh), true);
        let routes = vec![benches(vec![b]), patch(path("bob", "acme"), bench("bob", "acme", "paused", None))];
        let (s, rec, _) = setup(routes, &[]);
        let before = chrono::Utc::now().timestamp();
        let r = crate::api::removals::delete_now(&s, &who("ann"), "Acme", "bob", &confirm("bob", "acme")).await;
        assert_eq!(r.status(), 202);
        let sent = rec.sent("PATCH", &path("bob", "acme"));
        let a = &sent[0]["metadata"]["annotations"];
        assert_eq!((&a[REMOVED_AT], &a[DELETE_NOW]), (&json!(fresh), &json!("true")), "removed-at is kept");
        let after = chrono::DateTime::parse_from_rfc3339(a[DELETE_AFTER].as_str().unwrap()).unwrap().timestamp();
        assert!((before..=chrono::Utc::now().timestamp()).contains(&after), "due now, not after the grace");
        assert!(rec.requests().iter().any(|r| r.contains("fieldManager=kloudlite-membership")));
        assert!(deletes(&rec).is_empty(), "{:?}", deletes(&rec));
    }


    #[test]
    fn removals_lists_stamped_pairs_only() {
        let parse = |v: serde_json::Value| serde_json::from_value::<crd::Bench>(v).unwrap();
        let mut foreign = bench("carl", "acme", "paused", None);
        foreign["metadata"]["annotations"] = json!({REMOVED_AT: OLD});
        let o = Objects {
            benches: vec![parse(bench("bob", "acme", "paused", Some(OLD))), parse(bench("alice", "acme", "full", None)), parse(foreign)],
            workspaces: vec![],
            spaces: vec![],
        };
        let r = removals(&o);
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!((r[0].owner.as_str(), r[0].team.as_str()), ("bob", "acme"));
        assert_eq!(r[0].delete_at, "2020-01-08T00:00:00+00:00");
    }
}
