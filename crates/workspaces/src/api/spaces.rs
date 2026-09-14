//! The two keys-beat halves that keep `SpaceEnvironment` honest without a request behind them:
//!
//! * **migration** (one release): every Workspace or Bench still carrying the retired
//!   `spec.attachedEnvironment` whose space has no choice yet becomes one — if the environment
//!   still passes `/v1/me/environments`' rules — and the field is then cleared, so the beat
//!   terminates. Two objects of one space attached to different environments: the most recently
//!   updated wins (owner ruling, 2026-09-14), the rest are logged `space.migrate.conflict`. A
//!   transient failure clears nothing, so the next beat retries; the agent's field fallback keeps
//!   DNS working meanwhile.
//! * **departure**: a choice whose person is no longer in its team is deleted. At most one beat
//!   (`KEYS_RESYNC_SECS`) late, accepted by the owner: every access goes through `/v1`, which reads
//!   membership fresh on each call.

use super::scope::{is_team, teams_for};
use super::ApiState;
use crate::crd;
use crate::k8s;
use kube::api::{Api, Patch, PatchParams};
use kube::{Resource, ResourceExt};
use std::collections::{BTreeMap, BTreeSet};

/// One object still naming an environment through the retired field.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Legacy {
    pub kind: &'static str,
    pub name: String,
    pub owner: String,
    /// The space's team: the object's team, or the owner for a personal one.
    pub team: String,
    pub environment: String,
    pub updated_ms: i64,
}

/// Which choices to write, and which objects lost a conflict. Pure, so the rule is testable: a
/// space that already has a choice gets nothing written (the person's own choice wins over any
/// field), and within a space the newest object's environment wins.
pub(crate) fn plan(legacy: &[Legacy], existing: &BTreeSet<String>) -> (Vec<Legacy>, Vec<Legacy>) {
    let mut by_space: BTreeMap<String, Vec<&Legacy>> = BTreeMap::new();
    for l in legacy {
        let space = crd::space_name(&l.owner, &l.team);
        if !existing.contains(&space) {
            by_space.entry(space).or_default().push(l);
        }
    }
    let (mut writes, mut conflicts) = (Vec::new(), Vec::new());
    for (_, mut group) in by_space {
        // Newest first; the name breaks a tie so two beats always pick the same winner.
        group.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms).then_with(|| a.name.cmp(&b.name)));
        let winner = group[0].clone();
        conflicts.extend(group.iter().skip(1).filter(|l| l.environment != winner.environment).map(|l| (*l).clone()));
        writes.push(winner);
    }
    (writes, conflicts)
}

/// When the API server last wrote the object, else when it was made.
fn updated_ms<K: Resource>(o: &K) -> i64 {
    let m = o.meta();
    let managed = m.managed_fields.iter().flatten().filter_map(|f| f.time.as_ref().map(|t| t.0.as_millisecond())).max();
    managed.or_else(|| m.creation_timestamp.as_ref().map(|t| t.0.as_millisecond())).unwrap_or(0)
}

fn team_of(owner: &str, team: &str) -> String {
    if team.is_empty() { owner.to_lowercase() } else { team.to_lowercase() }
}

/// `Ok(true)` may write, `Ok(false)` never will (clear the field), `Err` unknown (retry next beat).
async fn allowed(s: &ApiState, envs: &Api<crd::Environment>, l: &Legacy) -> Result<bool, ()> {
    let e = match envs.get_opt(&l.environment).await {
        Ok(e) => e,
        Err(_) => return Err(()),
    };
    let Some(e) = e.filter(super::environments::visible_env) else { return Ok(false) };
    if !e.spec.owner.eq_ignore_ascii_case(&l.team) {
        return Ok(false);
    }
    if l.team.eq_ignore_ascii_case(&l.owner) {
        return Ok(true);
    }
    if s.directory.is_none() {
        return Err(());
    }
    if teams_for(s, &l.owner).await.iter().any(|t| t.eq_ignore_ascii_case(&l.team)) {
        return Ok(true);
    }
    // `teams_for` fails closed to empty, so "not a member" is definitive only for a team the
    // directory confirms exists.
    if is_team(s, &l.team).await { Ok(false) } else { Err(()) }
}

pub async fn migrate(s: &ApiState) {
    let Some(c) = s.kube.as_ref() else { return };
    let space_api: Api<crd::SpaceEnvironment> = Api::all(c.clone());
    let mut existing: BTreeSet<String> = match space_api.list(&Default::default()).await {
        Ok(l) => l.items.iter().map(|x| x.name_any()).collect(),
        Err(e) => {
            tracing::warn!(kind = "SpaceEnvironment", error = %e, "listing.failed");
            return;
        }
    };
    let mut legacy = Vec::new();
    match Api::<crd::Workspace>::all(c.clone()).list(&Default::default()).await {
        Ok(l) => legacy.extend(l.items.iter().filter_map(|w| {
            let env = w.spec.attached_environment.clone().filter(|e| !e.is_empty())?;
            Some(Legacy { kind: "Workspace", name: w.name_any(), owner: w.spec.owner.to_lowercase(), team: team_of(&w.spec.owner, &w.spec.team), environment: env, updated_ms: updated_ms(w) })
        })),
        Err(e) => return tracing::warn!(kind = "Workspace", error = %e, "listing.failed"),
    }
    match Api::<crd::Bench>::all(c.clone()).list(&Default::default()).await {
        Ok(l) => legacy.extend(l.items.iter().filter_map(|b| {
            let env = b.spec.attached_environment.clone().filter(|e| !e.is_empty())?;
            Some(Legacy { kind: "Bench", name: b.name_any(), owner: b.spec.owner.to_lowercase(), team: team_of(&b.spec.owner, &b.spec.team), environment: env, updated_ms: updated_ms(b) })
        })),
        Err(kube::Error::Api(e)) if e.code == 404 => {}
        Err(e) => return tracing::warn!(kind = "Bench", error = %e, "listing.failed"),
    }
    if legacy.is_empty() {
        return;
    }
    let (writes, conflicts) = plan(&legacy, &existing);
    for l in &conflicts {
        tracing::warn!(kind = l.kind, name = %l.name, environment = %l.environment, space = %crd::space_name(&l.owner, &l.team), "space.migrate.conflict");
    }
    let envs: Api<crd::Environment> = Api::all(c.clone());
    let mut refused = BTreeSet::new();
    for l in writes {
        let space = crd::space_name(&l.owner, &l.team);
        match allowed(s, &envs, &l).await {
            Ok(true) => {
                let obj = crd::space_environment(&l.owner, &l.team, &l.environment);
                match space_api.patch(&space, &PatchParams::apply(crd::API_FIELD_MANAGER).force(), &Patch::Apply(&obj)).await {
                    Ok(_) => {
                        tracing::info!(space = %space, environment = %l.environment, "space.migrated");
                        existing.insert(space);
                    }
                    Err(e) => tracing::warn!(space = %space, error = %e, "space.migrate.failed"),
                }
            }
            Ok(false) => {
                tracing::info!(space = %space, environment = %l.environment, "space.migrate.refused");
                refused.insert(space);
            }
            Err(()) => {}
        }
    }
    // Clear only what is settled — a choice exists, or the attach can never become one — so a
    // transient failure keeps the field for the agent's fallback and the next beat.
    let clear = serde_json::json!({"spec": {"attachedEnvironment": null}, "metadata": {"labels": {k8s::ATTACHED_ENV_LABEL: null}}});
    for l in legacy {
        let space = crd::space_name(&l.owner, &l.team);
        if !existing.contains(&space) && !refused.contains(&space) {
            continue;
        }
        let r = match l.kind {
            "Workspace" => Api::<crd::Workspace>::all(c.clone()).patch(&l.name, &PatchParams::default(), &Patch::Merge(&clear)).await.map(|_| ()),
            _ => Api::<crd::Bench>::all(c.clone()).patch(&l.name, &PatchParams::default(), &Patch::Merge(&clear)).await.map(|_| ()),
        };
        if let Err(e) = r {
            tracing::warn!(kind = l.kind, name = %l.name, error = %e, "space.migrate.clear.failed");
        }
    }
}

/// A team choice whose person has left the team goes. Keep-biased: a team the directory does not
/// confirm is kept, because `teams_for` answers an outage with an empty list.
pub async fn prune_departed(s: &ApiState) {
    let (Some(c), Some(_)) = (s.kube.as_ref(), s.directory.as_ref()) else { return };
    let api: Api<crd::SpaceEnvironment> = Api::all(c.clone());
    let items = match api.list(&Default::default()).await {
        Ok(l) => l.items,
        Err(e) => return tracing::warn!(kind = "SpaceEnvironment", error = %e, "listing.failed"),
    };
    for x in items.iter().filter(|x| !x.spec.team.eq_ignore_ascii_case(&x.spec.owner)) {
        if teams_for(s, &x.spec.owner).await.iter().any(|t| t.eq_ignore_ascii_case(&x.spec.team)) || !is_team(s, &x.spec.team).await {
            continue;
        }
        match api.delete(&x.name_any(), &Default::default()).await {
            Ok(_) => tracing::info!(owner = %x.spec.owner, team = %x.spec.team, "space.departed.pruned"),
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(e) => tracing::warn!(owner = %x.spec.owner, team = %x.spec.team, error = %e, "space.departed.prune.failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(name: &str, owner: &str, team: &str, env: &str, at: i64) -> Legacy {
        Legacy { kind: "Workspace", name: name.into(), owner: owner.into(), team: team.into(), environment: env.into(), updated_ms: at }
    }

    #[test]
    fn the_newest_attach_in_a_space_wins_and_the_rest_are_conflicts() {
        let legacy = [l("ws-a", "alice", "acme", "env-1", 10), l("ws-b", "alice", "acme", "env-2", 20), l("ws-c", "alice", "acme", "env-2", 5), l("ws-d", "bob", "acme", "env-1", 1)];
        let (writes, conflicts) = plan(&legacy, &BTreeSet::new());
        let w: BTreeSet<(&str, &str)> = writes.iter().map(|x| (x.name.as_str(), x.environment.as_str())).collect();
        assert_eq!(w, BTreeSet::from([("ws-b", "env-2"), ("ws-d", "env-1")]));
        assert_eq!(conflicts.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), vec!["ws-a"], "same environment is no conflict");
    }

    #[test]
    fn a_space_that_already_chose_is_left_alone_so_a_second_beat_writes_nothing() {
        let legacy = [l("ws-a", "alice", "alice", "env-1", 10)];
        let (writes, _) = plan(&legacy, &BTreeSet::new());
        assert_eq!(writes.len(), 1);
        let done: BTreeSet<String> = [crd::space_name("alice", "alice")].into();
        assert!(plan(&legacy, &done).0.is_empty(), "idempotent once the choice exists");
    }
}
