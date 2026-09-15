//! Who has left a team, judged on the keys beat — the ONE decider for a departed member's team
//! data (it replaced `pause_departed_benches` and the deleted-team `prune_team_benches`, so a
//! deleted team's members follow the same grace as a removed member's).
//!
//! Lifecycle of one `(owner, team)` pair, read from the strict `Directory::membership`:
//!
//! | directory says            | stamp        | verdict                          |
//! |---------------------------|--------------|----------------------------------|
//! | error / unreachable       | any          | Keep — write nothing, stamp nothing |
//! | member (active or paused) | present      | Clear both annotations           |
//! | active member             | none, paused | Unpause the bench                |
//! | paused member             | none         | Pause the bench, NEVER stamp     |
//! | not a member / team gone  | none         | Stamp `removed-at`, pause the bench |
//! | not a member / team gone  | < grace      | Keep                             |
//! | not a member / team gone  | ≥ grace, or `delete-now` | Delete                |
//!
//! Keep-biased because Delete is irreversible: only a directory that ANSWERS "not a member" or "no
//! such team" moves a pair toward it, and the grace restarts from a fresh stamp whenever the stamp
//! cannot be read. The stamp lives on the Bench, or on the pair's Workspaces when there is none,
//! or on its SpaceEnvironment when there is neither; the latest parseable one counts.
//!
//! Delete (gated on central `member_removal_deletes`, default off = log
//! `membership.cleanup.would_delete`): the Bench, then every team Workspace (`WORKTREE_FINALIZER`
//! takes sync points and detaches a Volume that keeps pushed snapshots), then the SpaceEnvironment.
//! Keys need no write — `project_all` drops the team's slug and `prune_namespaces` the empty `wt-`.
//! NOTHING here deletes a Snapshot, Volume, Environment, repo or image. Each object is re-judged
//! right before its DELETE and carries a uid + resourceVersion precondition; 404 and an accepted
//! delete are done, 409 or any other error stops the pair until the next beat. A Bench goes only
//! when it carries `kloudlite.io/bench-folder` (the agent's folder finalizer): until that exists
//! its folder would be stranded, so it is skipped and logged instead.
//! One `member.removed.cleanup` audit row per deleted object; `CLEANUP_DELETES_PER_BEAT` bounds a beat.
//!
//! Provenance: only a `removed-at` this beat wrote counts — applied under the field manager
//! `kloudlite-membership` and read back from `managedFields` — and `delete-now` counts only beside
//! one. A restored, hand-written or agent-written stamp is no stamp: the pair is restamped now,
//! with a fresh grace and its audit row. `deploy/k3s/agent-admission.yaml` refuses anyone but the
//! api's account a change to either annotation. Team and owner are compared trimmed and
//! lowercased, the directory's slug form, so `team: Acme` is never judged a gone team.
//!
//! A beat and not `remove_member`: that route lives in the directory binary, which holds no
//! kubeconfig, and a beat also heals a removal or team delete that happened while this was down.

use super::{ApiState, Directory, Judged, MemberState};
use crate::crd;
use kube::api::{Api, DeleteParams, Patch, PatchParams, Preconditions};
use kube::ResourceExt;
use serde_json::json;
use std::collections::BTreeSet;
use std::time::Duration;

pub const MEMBER_REMOVAL_GRACE: Duration = Duration::from_secs(7 * 24 * 3600);
pub const REMOVED_AT: &str = "kloudlite.io/removed-at";
pub const DELETE_NOW: &str = "kloudlite.io/delete-now";
pub const MEMBERSHIP_FIELD_MANAGER: &str = "kloudlite-membership";
pub const BENCH_FOLDER_FINALIZER: &str = "kloudlite.io/bench-folder";
// ponytail: a fixed cap; a backlog after a big team delete drains at this rate per beat.
pub const CLEANUP_DELETES_PER_BEAT: usize = 50;

#[derive(Debug, PartialEq)]
pub enum Verdict {
    Keep,
    Pause,
    Unpause,
    Stamp,
    Clear,
    Delete,
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
            _ if delete_now => Verdict::Delete,
            None => Verdict::Stamp,
            Some(t) if now.saturating_sub(t) >= MEMBER_REMOVAL_GRACE.as_secs() as i64 => Verdict::Delete,
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
pub(super) fn system_annotation(m: &kube::core::ObjectMeta, key: &str) -> Option<String> {
    let pointer = format!("/f:metadata/f:annotations/f:{}", key.replace('~', "~0").replace('/', "~1"));
    let owned = m.managed_fields.iter().flatten().any(|f| {
        f.manager.as_deref() == Some(MEMBERSHIP_FIELD_MANAGER) && f.fields_v1.as_ref().is_some_and(|v| v.0.pointer(&pointer).is_some())
    });
    owned.then(|| m.annotations.as_ref().and_then(|a| a.get(key)).cloned()).flatten()
}

pub async fn reconcile(s: &ApiState) {
    let (Some(c), Some(dir)) = (s.kube.as_ref(), s.directory.as_ref()) else { return };
    let o = match list_all(c).await {
        Ok(o) => o,
        Err(error) => return tracing::warn!(%error, "membership.reconcile.listing.failed"),
    };
    let (mut failed, mut last, mut budget) = (0usize, String::new(), CLEANUP_DELETES_PER_BEAT);
    for (owner, team) in pairs(&o) {
        if let Err(e) = judge(s, c, dir.as_ref(), &o, &owner, &team, &mut budget).await {
            (failed, last) = (failed + 1, e);
        }
    }
    if failed > 0 {
        tracing::warn!(failed, error = %last, "membership.reconcile.skipped");
    }
}

pub async fn reconcile_pair(s: &ApiState, owner: &str, team: &str) {
    let (Some(c), Some(dir)) = (s.kube.as_ref(), s.directory.as_ref()) else { return };
    if !team_pair(owner, team) {
        return;
    }
    let (owner, team) = (&norm(owner), &norm(team));
    match list_all(c).await {
        Ok(o) => {
            if let Err(error) = judge(s, c, dir.as_ref(), &o, owner, team, &mut CLEANUP_DELETES_PER_BEAT.clone()).await {
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
async fn judge(s: &ApiState, c: &kube::Client, dir: &dyn Directory, o: &Objects, owner: &str, team: &str, budget: &mut usize) -> Result<(), String> {
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
            let p = json!({"metadata": {"annotations": {REMOVED_AT: null, DELETE_NOW: null}}});
            let has = |m: &kube::core::ObjectMeta| ann(m, REMOVED_AT).is_some() || ann(m, DELETE_NOW).is_some();
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
            if !benches.is_empty() {
                // The stamp, and during the grace no tools — the pause as its own merge patch so
                // the membership manager never takes ownership of `spec.access`.
                for b in &benches {
                    ok |= stamp(&bapi, &b.name_any(), json!({REMOVED_AT: at.to_rfc3339()})).await;
                    write(&bapi, &b.name_any(), access(crd::BenchAccess::Paused), "membership.bench.paused").await;
                }
            } else if !workspaces.is_empty() {
                for w in &workspaces {
                    ok |= stamp(&wapi, &w.name_any(), json!({REMOVED_AT: at.to_rfc3339()})).await;
                }
            } else {
                for x in &spaces {
                    ok |= stamp(&sapi, &x.name_any(), json!({REMOVED_AT: at.to_rfc3339()})).await;
                }
            }
            if !ok {
                return Ok(());
            }
            let reason = if judged == Judged::TeamGone { "team_deleted" } else { "left_or_removed" };
            let delete_at = (at + MEMBER_REMOVAL_GRACE).to_rfc3339();
            let detail = json!({"owner": owner, "team": team, "reason": reason, "delete_at": delete_at}).to_string();
            super::admin::audit(s, "system:membership", "member.removed.judged", &format!("{team}/{owner}"), Some(detail), "ok").await;
        }
        Verdict::Delete if !s.central.load().member_removal_deletes => tracing::info!(%owner, %team, delete_now, "membership.cleanup.would_delete"),
        Verdict::Delete => {
            let mut steps: Vec<Target> = Vec::new();
            for b in &benches {
                if b.finalizers().iter().any(|f| f == BENCH_FOLDER_FINALIZER) {
                    steps.push(Target::of(*b, "Bench"));
                } else {
                    tracing::warn!(%owner, %team, name = %b.name_any(), "membership.cleanup.bench_waits_finalizer");
                }
            }
            steps.extend(workspaces.iter().map(|w| Target { env: crd::attached_environment(w), ..Target::of(*w, "Workspace") }));
            steps.extend(spaces.iter().map(|x| Target::of(*x, "SpaceEnvironment")));
            cleanup(s, dir, (&bapi, &wapi, &sapi), owner, team, steps, budget).await;
        }
    }
    Ok(())
}

struct Target {
    kind: &'static str,
    name: String,
    uid: Option<String>,
    rv: Option<String>,
    deleting: bool,
    env: Option<String>,
}

impl Target {
    fn of<K: kube::Resource>(x: &K, kind: &'static str) -> Self {
        Target { kind, name: x.name_any(), uid: x.uid(), rv: x.resource_version(), deleting: x.meta().deletion_timestamp.is_some(), env: None }
    }
}

type Apis<'a> = (&'a Api<crd::Bench>, &'a Api<crd::Workspace>, &'a Api<crd::SpaceEnvironment>);

async fn cleanup(s: &ApiState, dir: &dyn Directory, apis: Apis<'_>, owner: &str, team: &str, steps: Vec<Target>, budget: &mut usize) {
    for t in steps.into_iter().filter(|t| !t.deleting) {
        if *budget == 0 {
            return tracing::info!(%owner, %team, "membership.cleanup.deferred");
        }
        // A member re-added since the list must never lose data: ask again, right before this DELETE.
        let reason = match dir.membership(team, owner).await {
            Ok(Judged::TeamGone) => "team_deleted",
            Ok(Judged::NotMember) => "left_or_removed",
            other => return tracing::info!(%owner, %team, answer = ?other, "membership.cleanup.rejudged_keep"),
        };
        let dp = DeleteParams { preconditions: Some(Preconditions { uid: t.uid.clone(), resource_version: t.rv.clone() }), ..Default::default() };
        let res = match t.kind {
            "Bench" => apis.0.delete(&t.name, &dp).await.map(|_| ()),
            "Workspace" => apis.1.delete(&t.name, &dp).await.map(|_| ()),
            _ => apis.2.delete(&t.name, &dp).await.map(|_| ()),
        };
        // The env-side attach policy lives in another namespace, so no ownerReference collects it:
        // drop it after the Workspace, exactly as `delete_as` does (best effort, logged there).
        let gone = match &res {
            Ok(()) => true,
            Err(kube::Error::Api(e)) => e.code == 404,
            Err(_) => false,
        };
        if t.kind == "Workspace" && gone {
            super::workspaces::drop_attach_policy(&apis.1.clone().into_client(), &t.name, t.env.as_deref()).await;
        }
        match res {
            Ok(()) => {
                *budget -= 1;
                tracing::info!(%owner, %team, kind = t.kind, name = %t.name, "membership.cleanup.deleted");
                let detail = json!({"owner": owner, "team": team, "kind": t.kind, "name": t.name, "reason": reason}).to_string();
                super::admin::audit(s, "system:membership", "member.removed.cleanup", &format!("{team}/{owner}"), Some(detail), "ok").await;
            }
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(kube::Error::Api(e)) if e.code == 409 => {
                return tracing::warn!(%owner, %team, kind = t.kind, name = %t.name, error = %e, "membership.cleanup.conflict");
            }
            Err(error) => return tracing::warn!(%owner, %team, kind = t.kind, name = %t.name, %error, "membership.cleanup.failed"),
        }
    }
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
            v["metadata"]["finalizers"] = json!([BENCH_FOLDER_FINALIZER]);
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
    fn removed(owner: &str, bench_status: u16) -> Vec<crate::kube_test::Route> {
        let b = with_meta(bench(owner, "acme", "paused", None), &crd::bench_id(owner, "acme"), Some(OLD), true);
        vec![
            benches(vec![b]),
            list_of("Workspace", "workspaces", vec![ws("w1", owner, "acme"), ws("w2", owner, "acme"), ws("w9", owner, "")]),
            list_of("SpaceEnvironment", "spaceenvironments", vec![space(owner, "acme")]),
            del(path(owner, "acme"), bench_status),
            del(format!("{API}/workspaces/w1"), 200),
            del(format!("{API}/workspaces/w2"), 200),
            del(format!("{API}/spaceenvironments/{}", crd::space_name(owner, "acme")), 200),
        ]
    }

    fn deleting(s: ApiState) -> ApiState {
        s.central.store(kloudlite_core::settings::CentralSettings { member_removal_deletes: true, ..kloudlite_core::settings::CentralSettings::built_in_defaults() });
        s
    }

    fn deletes(rec: &Recorder) -> Vec<String> {
        rec.calls().into_iter().filter(|c| c.starts_with("DELETE ")).collect()
    }

    #[tokio::test]
    async fn a_mixed_case_team_is_judged_by_its_slug() {
        let (s, rec, dir) = setup(vec![benches(vec![bench("alice", " Acme", "full", None)])], &[]);
        reconcile(&deleting(s)).await;
        assert_eq!(dir.asked.load(Ordering::SeqCst), 1);
        assert!(writes(&rec).is_empty(), "a live member of acme is kept: {:?}", writes(&rec));
    }

    #[tokio::test]
    async fn a_stamp_this_system_did_not_write_restarts_the_grace() {
        let mut routes = removed("bob", 200);
        let mut b = bench("bob", "acme", "paused", None);
        b["metadata"]["annotations"] = json!({REMOVED_AT: OLD, DELETE_NOW: "yes"});
        b["metadata"]["finalizers"] = json!([BENCH_FOLDER_FINALIZER]);
        routes[0] = benches(vec![b]);
        routes[1] = list_of("Workspace", "workspaces", vec![]);
        let (s, rec, _) = setup(routes, &[("bob", "acme")]);
        reconcile(&deleting(s)).await;
        assert!(deletes(&rec).is_empty(), "{:?}", deletes(&rec));
        let sent = rec.sent("PATCH", &path("bob", "acme"));
        assert_ne!(sent[0]["metadata"]["annotations"][REMOVED_AT], json!(OLD), "a fresh stamp");
    }

    #[tokio::test]
    async fn a_workspace_only_pair_is_stamped_on_its_workspaces() {
        let routes = vec![list_of("Workspace", "workspaces", vec![ws("w1", "bob", "acme")]), patch(format!("{API}/workspaces/w1"), ws("w1", "bob", "acme"))];
        let (s, rec, _) = setup(routes, &[]);
        reconcile(&s).await;
        let sent = rec.sent("PATCH", &format!("{API}/workspaces/w1"));
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["kind"], "Workspace");
        assert!(sent[0]["metadata"]["annotations"][REMOVED_AT].is_string());
    }

    #[tokio::test]
    async fn a_bench_set_back_to_full_during_the_grace_is_paused_again() {
        let fresh = chrono::Utc::now().to_rfc3339();
        let (s, rec, _) = setup(vec![benches(vec![bench("bob", "acme", "full", Some(&fresh))])], &[("bob", "acme")]);
        reconcile(&s).await;
        assert_eq!(rec.sent("PATCH", &path("bob", "acme")), vec![json!({"spec": {"access": "paused"}})]);
    }

    #[tokio::test]
    async fn deletes_run_bench_then_workspaces_then_space_choice() {
        let (s, rec, _) = setup(removed("bob", 200), &[]);
        reconcile(&deleting(s)).await;
        let want = vec![
            format!("DELETE {}", path("bob", "acme")),
            format!("DELETE {API}/workspaces/w1"),
            format!("DELETE {API}/workspaces/w2"),
            format!("DELETE {API}/spaceenvironments/{}", crd::space_name("bob", "acme")),
        ];
        assert_eq!(deletes(&rec), want);
        let pre = &rec.sent("DELETE", &format!("{API}/workspaces/w1"))[0]["preconditions"];
        assert_eq!((pre["uid"].as_str(), pre["resourceVersion"].as_str()), (Some("uid-w1"), Some("7")));
    }

    #[tokio::test]
    async fn an_attached_workspace_delete_also_drops_the_env_side_policy() {
        let mut routes = removed("bob", 200);
        let mut w1 = ws("w1", "bob", "acme");
        w1["spec"]["attachedEnvironment"] = json!("env-7");
        routes[1] = list_of("Workspace", "workspaces", vec![w1, ws("w2", "bob", "acme")]);
        let policy = format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/{}", crd::env_namespace("env-7"), crate::k8s::attach_policy_name("w1"));
        routes.push(del(policy.clone(), 200));
        let (s, rec, _) = setup(routes, &[]);
        reconcile(&deleting(s)).await;
        let d = deletes(&rec);
        let at = |x: &str| d.iter().position(|c| c == x).unwrap_or_else(|| panic!("{x} missing: {d:?}"));
        assert!(at(&format!("DELETE {API}/workspaces/w1")) < at(&format!("DELETE {policy}")), "the Workspace goes first");
        assert_eq!(d.iter().filter(|c| c.contains("networkpolicies")).count(), 1, "an unattached workspace has none");
    }

    #[tokio::test]
    async fn no_snapshot_volume_or_environment_is_ever_deleted() {
        let (s, rec, _) = setup(removed("bob", 200), &[]);
        reconcile(&deleting(s)).await;
        for c in deletes(&rec) {
            assert!(!["/snapshots", "/volumes", "/environments/"].iter().any(|k| c.contains(k)), "{c}");
        }
        assert_eq!(deletes(&rec).len(), 4);
    }

    #[tokio::test]
    async fn a_bench_without_the_folder_finalizer_is_left_for_its_folder() {
        let mut routes = removed("bob", 200);
        routes[0] = benches(vec![with_meta(bench("bob", "acme", "paused", None), &crd::bench_id("bob", "acme"), Some(OLD), false)]);
        let (s, rec, _) = setup(routes, &[]);
        reconcile(&deleting(s)).await;
        assert!(!deletes(&rec).contains(&format!("DELETE {}", path("bob", "acme"))));
        assert_eq!(deletes(&rec).len(), 3, "the workspaces and the space choice still go");
    }

    #[tokio::test]
    async fn a_409_stops_the_pair_and_is_not_forced() {
        let (s, rec, _) = setup(removed("bob", 409), &[]);
        reconcile(&deleting(s)).await;
        assert_eq!(deletes(&rec), vec![format!("DELETE {}", path("bob", "acme"))]);
    }

    #[tokio::test]
    async fn with_deletes_off_nothing_is_deleted() {
        let (s, rec, _) = setup(removed("bob", 200), &[]);
        reconcile(&s).await;
        assert!(writes(&rec).is_empty(), "{:?}", writes(&rec));
    }

    #[tokio::test]
    async fn a_rejudge_that_finds_the_member_back_deletes_nothing() {
        let (s, rec, _) = setup(removed("rex", 200), &[]);
        reconcile(&deleting(s)).await;
        assert!(deletes(&rec).is_empty(), "{:?}", deletes(&rec));
    }

    #[tokio::test]
    async fn the_beat_after_a_full_cleanup_writes_nothing() {
        let mut routes = removed("bob", 200);
        routes.extend([benches(vec![]), list_of("Workspace", "workspaces", vec![]), list_of("SpaceEnvironment", "spaceenvironments", vec![])]);
        let (s, rec, _) = setup(routes, &[]);
        let s = deleting(s);
        reconcile(&s).await;
        let first = writes(&rec).len();
        reconcile(&s).await;
        assert_eq!(writes(&rec).len(), first, "{:?}", writes(&rec));
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
            assert_eq!(decide(&Ok(j), Some(now - g), false, now, true), Verdict::Delete);
            assert_eq!(decide(&Ok(j), Some(now), true, now, true), Verdict::Delete);
            assert_eq!(decide(&Ok(j), None, true, now, false), Verdict::Delete);
        }
    }

    #[tokio::test]
    async fn a_directory_error_changes_nothing() {
        let (s, rec, _) = setup(vec![benches(vec![bench("bob", "down", "full", Some("2020-01-01T00:00:00Z"))])], &[("bob", "down")]);
        reconcile(&deleting(s)).await;
        assert!(writes(&rec).is_empty(), "{:?}", writes(&rec));
    }

    #[tokio::test]
    async fn a_personal_pair_is_never_a_candidate() {
        let (s, rec, dir) = setup(vec![benches(vec![bench("alice", "Alice", "full", None), bench("carol", "", "full", None)])], &[]);
        reconcile(&s).await;
        assert_eq!(dir.asked.load(Ordering::SeqCst), 0);
        assert!(writes(&rec).is_empty(), "{:?}", writes(&rec));
    }

    #[tokio::test]
    async fn a_removed_pair_is_stamped_once_and_the_second_beat_writes_nothing() {
        let routes = vec![benches(vec![bench("bob", "acme", "full", None)]), benches(vec![bench("bob", "acme", "paused", Some("2026-09-15T00:00:00Z"))])];
        let (s, rec, _) = setup(routes, &[("bob", "acme")]);
        reconcile(&s).await;
        let sent = rec.sent("PATCH", &path("bob", "acme"));
        assert_eq!(sent.len(), 2);
        assert!(sent[0]["metadata"]["annotations"][REMOVED_AT].is_string());
        assert!(rec.requests().iter().any(|r| r.contains("fieldManager=kloudlite-membership")), "{:?}", rec.requests());
        assert_eq!(sent[1], json!({"spec": {"access": "paused"}}));
        reconcile(&s).await;
        assert_eq!(rec.sent("PATCH", &path("bob", "acme")).len(), 2, "the second beat writes nothing");
    }

    #[tokio::test]
    async fn a_deleted_team_waits_out_the_grace_too() {
        let (s, rec, _) = setup(vec![benches(vec![bench("bob", "gone", "paused", Some(&chrono::Utc::now().to_rfc3339()))])], &[("bob", "gone")]);
        reconcile(&s).await;
        assert!(writes(&rec).is_empty(), "{:?}", writes(&rec));
    }

    #[tokio::test]
    async fn a_readd_during_the_grace_clears_the_stamp() {
        let (s, rec, _) = setup(vec![benches(vec![bench("alice", "acme", "full", Some("2026-09-15T00:00:00Z"))])], &[("alice", "acme")]);
        reconcile(&s).await;
        assert_eq!(rec.sent("PATCH", &path("alice", "acme")), vec![json!({"metadata": {"annotations": {REMOVED_AT: null, DELETE_NOW: null}}})]);
    }

    #[tokio::test]
    async fn a_paused_member_is_never_stamped() {
        let (s, rec, _) = setup(vec![benches(vec![bench("paula", "acme", "full", None)])], &[("paula", "acme")]);
        reconcile(&s).await;
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
        reconcile(&deleting(s)).await;
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
        reconcile(&s).await;
        assert!(writes(&rec).is_empty(), "{:?}", writes(&rec));
    }

    #[tokio::test]
    async fn unpause_sets_full_and_starts_nothing() {
        let mut b = bench("alice", "acme", "paused", None);
        b["spec"]["desiredState"] = json!("stopped");
        let mut w = ws("w1", "alice", "acme");
        w["spec"]["desiredState"] = json!("stopped");
        let (s, rec, _) = setup(vec![benches(vec![b]), list_of("Workspace", "workspaces", vec![w])], &[("alice", "acme")]);
        reconcile(&s).await;
        assert_eq!(writes(&rec), vec![format!("PATCH {}", path("alice", "acme"))]);
        assert_eq!(rec.sent("PATCH", &path("alice", "acme")), vec![json!({"spec": {"access": "full"}})]);
    }

    #[tokio::test]
    async fn a_readd_after_removal_gets_full_access_back() {
        let (s, rec, _) = setup(vec![benches(vec![bench("alice", "acme", "paused", Some("2026-09-15T00:00:00Z"))])], &[("alice", "acme")]);
        reconcile(&s).await;
        assert!(rec.sent("PATCH", &path("alice", "acme")).contains(&json!({"spec": {"access": "full"}})));
    }

    #[tokio::test]
    async fn a_personal_workspace_of_a_paused_member_is_untouched() {
        let (s, rec, _) = setup(paused_pair(bench("paula", "acme", "full", None), ws("w1", "paula", "acme")), &[("paula", "acme")]);
        reconcile(&s).await;
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
    async fn delete_now_refuses_an_unstamped_pair() {
        let (s, rec, _) = setup(vec![benches(vec![bench("alice", "acme", "full", None)])], &[]);
        let r = crate::api::removals::delete_now(&deleting(s), &who("ann"), "acme", "alice", &confirm("alice", "acme")).await;
        assert_eq!(r.status(), 409);
        assert!(writes(&rec).is_empty(), "{:?}", writes(&rec));
    }

    #[tokio::test]
    async fn delete_now_runs_the_delete_without_waiting_the_grace() {
        let fresh = chrono::Utc::now().to_rfc3339();
        let name = crd::bench_id("bob", "acme");
        let b = with_meta(bench("bob", "acme", "paused", None), &name, Some(&fresh), true);
        let mut marked = b.clone();
        marked["metadata"]["annotations"] = json!({REMOVED_AT: fresh, DELETE_NOW: "true"});
        marked["metadata"]["managedFields"] = owned(&[REMOVED_AT, DELETE_NOW]);
        let routes = vec![benches(vec![b.clone()]), benches(vec![b]), benches(vec![marked.clone()]), patch(path("bob", "acme"), marked), del(path("bob", "acme"), 200)];
        let (s, rec, _) = setup(routes, &[]);
        let r = crate::api::removals::delete_now(&deleting(s), &who("ann"), "Acme", "bob", &confirm("bob", "acme")).await;
        assert_eq!(r.status(), 202);
        let sent = rec.sent("PATCH", &path("bob", "acme"));
        assert_eq!(sent[0]["metadata"]["annotations"], json!({REMOVED_AT: fresh, DELETE_NOW: "true"}), "removed-at is kept");
        assert!(rec.requests().iter().any(|r| r.contains("fieldManager=kloudlite-membership")));
        assert_eq!(deletes(&rec), vec![format!("DELETE {}", path("bob", "acme"))]);
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
