//! Keys are the directory's; a cluster sees one `OwnerKeys` per owner namespace, written here and
//! only here. Two triggers, one function: a key or membership change projects the owners it
//! touched, and a beat re-projects every owner, so a write that was lost is at most one beat late.

use super::ApiState;
use crate::crd;
use crate::k8s;
use k8s_openapi::api::core::v1::{Namespace, Pod};
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::ResourceExt;
use std::collections::BTreeSet;
use std::sync::Arc;

pub const KEYS_RESYNC_SECS: u64 = 300;
/// A Lease in `kube-system` this beat renews after every membership reconcile. The controller's
/// removal GC reads it and deletes nothing while it is older than two beats: a re-add that only
/// the beat clears (a superadmin grant) is protected by the GC's slack only while the beat runs.
pub const KEYS_BEAT_LEASE: &str = "kloudlite-keys-beat";
pub const KEYS_BEAT_NAMESPACE: &str = "kube-system";

pub fn beat_lease(now: k8s_openapi::jiff::Timestamp) -> k8s_openapi::api::coordination::v1::Lease {
    use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
    Lease {
        metadata: kube::core::ObjectMeta { name: Some(KEYS_BEAT_LEASE.into()), namespace: Some(KEYS_BEAT_NAMESPACE.into()), ..Default::default() },
        spec: Some(LeaseSpec { holder_identity: Some("kloudlite-api".into()), renew_time: Some(MicroTime(now)), ..Default::default() }),
    }
}

/// The heartbeat follows a fully judged pass only (`membership::reconcile`'s `Ok` rule).
pub(crate) async fn membership_beat(s: &ApiState) {
    if super::membership::reconcile(s).await.is_ok() {
        renew_beat(s).await;
    }
}

/// Best effort: a lost write only makes the GC hold, which is the safe direction.
async fn renew_beat(s: &ApiState) {
    let Some(k) = s.kube.as_ref() else { return };
    let api: Api<k8s_openapi::api::coordination::v1::Lease> = Api::namespaced(k.clone(), KEYS_BEAT_NAMESPACE);
    let lease = beat_lease(k8s_openapi::jiff::Timestamp::now());
    if let Err(e) = api.patch(KEYS_BEAT_LEASE, &PatchParams::apply("kloudlite-api").force(), &Patch::Apply(&lease)).await {
        tracing::warn!(error = %e, "keys.beat_lease.failed");
    }
}

pub fn owner_keys(owner: &str, generation: i64, authorized_keys: String) -> crd::OwnerKeys {
    crd::OwnerKeys::new(owner, crd::OwnerKeysSpec { generation, authorized_keys })
}

/// Project ONE owner. A failed directory lookup writes nothing (the last projection stands, and
/// the beat retries); an owner with no keys writes an EMPTY file, which is the truth.
pub async fn project(s: &ApiState, owner: &str) -> Result<(), String> {
    let (Some(c), Some(dir)) = (s.kube.as_ref(), s.directory.as_ref()) else { return Ok(()) };
    let Some(file) = dir.authorized_keys_for_owner(owner).await else {
        return Err(format!("could not read {owner}'s keys"));
    };
    // Wall clock, not a counter: nothing here holds state across restarts, and a reader only ever
    // asks "is this the generation I wrote to disk", never "how many writes ago".
    let generation = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let api: Api<crd::OwnerKeys> = Api::all(c.clone());
    api.patch(
        owner,
        &PatchParams::apply(crd::API_FIELD_MANAGER).force(),
        &Patch::Apply(&owner_keys(owner, generation, file)),
    )
    .await
    .map(|_| ())
    .map_err(|e| format!("writing OwnerKeys/{owner}: {e}"))
}

/// The owner set the beat projects, from the truth rather than from a label: a TEAM workspace's
/// namespace is stamped with the PERSON's handle, so the namespace labels never name a team and a
/// team's projection would never be healed.
///
/// Returns `(project, prune)`: every owner a Workspace names, and every existing `OwnerKeys`
/// that no Workspace names any more. A projection with no workspace behind it has no pod reading
/// its file, so deleting it loses nothing — while keeping it meant every team a probe run ever
/// created lived on as an object re-projected every beat. The next workspace create projects it
/// again (`create_ws`), so a person who adds a key before their first workspace is not affected.
fn owner_set(workspaces: impl IntoIterator<Item = (String, String)>, existing: Vec<String>) -> (Vec<String>, Vec<String>) {
    let mut live: Vec<String> = Vec::new();
    for (owner, team) in workspaces {
        live.push(owner);
        if !team.is_empty() {
            live.push(team);
        }
    }
    live.retain(|o| !o.is_empty());
    live.sort();
    live.dedup();
    let mut stale: Vec<String> = existing.into_iter().filter(|e| !live.contains(e)).collect();
    stale.sort();
    stale.dedup();
    (live, stale)
}

pub async fn project_all(s: &ApiState) {
    let Some(c) = s.kube.as_ref() else { return };
    // A failed list SKIPS that source rather than projecting a smaller set: this beat only ever
    // rewrites objects, so missing one is a late projection, while guessing at the set is not
    // something a lost list can make safe.
    // Pruning needs the Workspace list to have SUCCEEDED: a failed list would make every
    // projection look stale, and this beat must never delete on a guess.
    let (specs, mut listed) = match Api::<crd::Workspace>::all(c.clone()).list(&Default::default()).await {
        Ok(l) => (l.items.into_iter().map(|w| w.spec).collect::<Vec<_>>(), true),
        Err(e) => {
            tracing::warn!(kind = "Workspace", error = %e, "listing.failed");
            (Vec::new(), false)
        }
    };
    // One builder per owner slug, from a workspace of that owner's — the region has to come from
    // somewhere real and any of their workspaces names one. This is the only back-fill there is:
    // `create_ws` writes a builder for new owners, and every owner whose workspaces predate the
    // release would otherwise have no builder and every build refused.
    let mut builders: std::collections::BTreeMap<String, (String, String, String)> = Default::default();
    for w in &specs {
        builders
            .entry(k8s::keys_owner(w).to_string())
            .or_insert_with(|| (w.owner.clone(), w.team.clone(), w.region.clone()));
    }
    let mut pairs: Vec<(String, String)> =
        specs.into_iter().map(|w| (w.owner, w.team)).collect();
    // A bench-only person has a namespace and a `user-key` Secret too: the create's one-shot install
    // can race the agent making the namespace, and the 24h registry token needs this beat to renew.
    // A failed list is a smaller set, so it also turns pruning off; a 404 is no Bench CRD.
    match Api::<crd::Bench>::all(c.clone()).list(&Default::default()).await {
        Ok(l) => pairs.extend(l.items.into_iter().map(|b| (b.spec.owner, b.spec.team))),
        Err(kube::Error::Api(e)) if e.code == 404 => {}
        Err(e) => {
            tracing::warn!(kind = "Bench", error = %e, "listing.failed");
            listed = false;
        }
    }
    let existing = match Api::<crd::OwnerKeys>::all(c.clone()).list(&Default::default()).await {
        Ok(l) => l.items.iter().filter_map(|o| o.metadata.name.clone()).collect(),
        Err(e) => {
            tracing::warn!(kind = "OwnerKeys", error = %e, "listing.failed");
            Vec::new()
        }
    };
    let (live, stale) = owner_set(pairs, existing);
    for o in live {
        if let Err(e) = project(s, &o).await {
            tracing::warn!(owner = %o, error = %e, "keys.project.failed");
        }
        // The registry token lives in `user-key`, minted with a 24h ttl (`write_user_key`), so
        // this beat is its only rotation path — nothing else re-mints it before it expires.
        super::workspaces::refresh_user_key_secrets(s, &o).await;
        tracing::info!(owner = %o, "keys.registry_token.refreshed");
    }
    // Best effort and keep-biased like the rest of the beat: a failed apply is retried next beat.
    for (slug, (owner, team, region)) in builders {
        if let Err(e) = super::environments::ensure_builder(s, &owner, &team, &region).await {
            tracing::warn!(owner = %slug, status = ?e.status(), "keys.builder.ensure.failed");
        }
    }
    if listed {
        let api: Api<crd::OwnerKeys> = Api::all(c.clone());
        for o in stale {
            match api.delete(&o, &Default::default()).await {
                Ok(_) => tracing::info!(owner = %o, "keys.projection.pruned"),
                Err(kube::Error::Api(e)) if e.code == 404 => {}
                Err(e) => tracing::warn!(owner = %o, error = %e, "keys.prune.failed"),
            }
        }
    }
}

pub async fn run_beat(s: Arc<ApiState>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(KEYS_RESYNC_SECS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        project_all(&s).await;
        membership_beat(&s).await;
        prune_namespaces(&s).await;
        prune_bindings(&s).await;
        prune_builders(&s).await;
        super::spaces::migrate(&s).await;
    }
}

/// Which of `seen` — `(name, age in seconds)` for every namespace labelled as a workspace one —
/// no longer belongs to anybody.
///
/// A team namespace is `wt-{person}-{hash of team}` and is created by the agent's `apply_binding`
/// for every team that person has a workspace in. Nothing ever deleted one: a region held 101 of
/// them, every one empty, one per hourly probe run since 2026-09-05. The rule mirrors the
/// `OwnerKeys` prune above — an object no Workspace resolves to has nothing reading it, and the
/// next workspace create rebuilds it — with two extra guards, because a namespace delete cascades
/// to everything inside it: a PERSON's own `ws-` namespace (which holds their `user-key` Secret)
/// is never a candidate whatever else is true, and one younger than a beat is left alone, which
/// closes the window between `apply_binding` creating a namespace and the workspace that needed
/// it becoming listable.
///
/// `ws-` is not only a person's, though: `ws_namespace` also returns `ws-{owner}` when the team
/// IS the owner, so a workspace owned by a team slug lands in one too — and because nothing here
/// would touch a `ws-` name, a region held 89 leaked `ws-run-hourly-*-team` namespaces by
/// 2026-09-12, one per hourly probe run, each re-walked by every agent every tick. `teams` is the
/// directory's answer for the `ws-` owners that pass the other guards; an owner the directory
/// does not call a team — including every owner it could not answer for — stays exempt.
fn stale_namespaces(
    keep: &BTreeSet<String>,
    seen: &[(String, i64)],
    max_age: i64,
    teams: &BTreeSet<String>,
) -> Vec<String> {
    seen.iter()
        .filter(|(name, age)| !keep.contains(name) && *age >= max_age)
        .filter(|(name, _)| match name.strip_prefix("ws-") {
            Some(owner) => teams.contains(owner),
            None => name.starts_with("wt-"),
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// The `ws-` owners the directory calls teams, asked only for the namespaces that already pass
/// the age and keep guards — the leak is a handful of names, and a lookup per namespace per beat
/// would make this beat's latency the region's namespace count.
async fn team_owners(s: &ApiState, keep: &BTreeSet<String>, seen: &[(String, i64)], max_age: i64) -> BTreeSet<String> {
    let owners: Vec<String> = seen
        .iter()
        .filter(|(name, age)| !keep.contains(name) && *age >= max_age)
        .filter_map(|(name, _)| name.strip_prefix("ws-").map(str::to_string))
        .collect();
    // An answered TEAM only, never "gone": a `ws-` namespace holds a person's `user-key` Secret, and
    // "no such person" can be a transient directory data problem — a binding is recreated on
    // demand, a deleted Secret is not.
    prunable_owners(s, owners, false).await
}

/// The owners a prune may act on: the directory ANSWERED, and named a team — or, with
/// `allow_gone`, nothing at all. A person, a failed read and a missing directory all keep.
async fn prunable_owners(s: &ApiState, owners: impl IntoIterator<Item = String>, allow_gone: bool) -> BTreeSet<String> {
    let Some(dir) = s.directory.as_ref() else { return BTreeSet::new() };
    let owners: Vec<String> = owners.into_iter().collect();
    let answers = futures::future::join_all(owners.iter().map(|o| dir.owner_kind(o))).await;
    owners
        .into_iter()
        .zip(answers)
        .filter(|(_, k)| matches!(k, Ok(super::OwnerKind::Team)) || (allow_gone && matches!(k, Ok(super::OwnerKind::Gone))))
        .map(|(o, _)| o)
        .collect()
}

/// The namespace half of the same beat: see `stale_namespaces` for the rule and why it is safe.
///
/// Keep-biased exactly as `project_all` is — a lost list would make every namespace look stale,
/// and this one deletes rather than rewrites, so a failure prunes NOTHING.
pub(crate) async fn prune_namespaces(s: &ApiState) {
    let Some(c) = s.kube.as_ref() else { return };
    // Namespaces FIRST, then the workspaces that spare them: `keep` must never be older than the
    // list it judges. The other order loses the workspace created between the two calls — its
    // namespace is old, absent from a stale `keep`, and its pod not yet scheduled, so all three
    // guards pass and a namespace a live Workspace needs is deleted.
    let lp = ListParams::default().labels(&format!("{}=workspace", k8s::KIND_LABEL));
    let listed = match Api::<Namespace>::all(c.clone()).list(&lp).await {
        Ok(l) => l.items,
        Err(e) => {
            tracing::warn!(kind = "Namespace", error = %e, "listing.failed");
            return;
        }
    };
    let keep: BTreeSet<String> = match Api::<crd::Workspace>::all(c.clone()).list(&Default::default()).await {
        // `ws_namespace` and not a hand-rolled name: the agent builds the namespace with this
        // exact function, so a second spelling here would prune what it just made.
        Ok(l) => l.items.iter().map(|w| crd::ws_namespace(&w.spec.owner, &w.spec.team)).collect(),
        Err(e) => {
            tracing::warn!(kind = "Workspace", error = %e, "listing.failed");
            return;
        }
    };
    // A bench holds its namespace even with no pod (idle or stopped): the `user-key` Secret and its
    // ingress policy live there. Same keep-bias; a 404 is a cluster without the Bench CRD.
    let mut keep = keep;
    match Api::<crd::Bench>::all(c.clone()).list(&Default::default()).await {
        Ok(l) => keep.extend(l.items.iter().map(|b| crd::ws_namespace(&b.spec.owner, &b.spec.team))),
        Err(kube::Error::Api(e)) if e.code == 404 => {}
        Err(e) => {
            tracing::warn!(kind = "Bench", error = %e, "listing.failed");
            return;
        }
    }
    let now = chrono::Utc::now().timestamp();
    let seen: Vec<(String, i64)> = listed
        .iter()
        .map(|n| {
            // No timestamp reads as age 0 — too young to judge, which is the keeping answer.
            let age = n.metadata.creation_timestamp.as_ref().map_or(0, |t| now - t.0.as_second());
            (n.name_any(), age)
        })
        .collect();
    let max_age = KEYS_RESYNC_SECS as i64;
    let teams = team_owners(s, &keep, &seen, max_age).await;
    let api: Api<Namespace> = Api::all(c.clone());
    for name in stale_namespaces(&keep, &seen, max_age, &teams) {
        // The last guard, and the reason it is here rather than in the rule above: a Workspace
        // that lost its labels is invisible to the keep set but its POD is not, and a namespace
        // delete would take the pod with it. Unreadable counts as occupied — a guard that cannot
        // be checked must refuse the delete, never wave it through.
        let pods: Api<Pod> = Api::namespaced(c.clone(), &name);
        match pods.list(&ListParams::default().limit(1)).await {
            Ok(p) if p.items.is_empty() => {}
            Ok(_) => {
                tracing::info!(namespace = %name, "keys.namespace.kept");
                continue;
            }
            Err(e) => {
                tracing::warn!(namespace = %name, error = %e, "keys.namespace.pods.unreadable");
                continue;
            }
        }
        match api.delete(&name, &Default::default()).await {
            Ok(_) => {
                let owner_kind = if name.starts_with("ws-") { "team" } else { "member" };
                tracing::info!(namespace = %name, %owner_kind, "keys.namespace.pruned");
            }
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(e) => tracing::warn!(namespace = %name, error = %e, "keys.namespace.prune.failed"),
        }
    }
}

/// Which of `seen` — `(name, spec.owner, age in seconds)` for every `OwnerBinding` — belongs to a
/// dead TEAM. The claiming agent creates one per (region, owner) and nothing deleted one, so by
/// 2026-09-16 109 of 116 were probe teams gone for days, each re-applied by every agent on every
/// Quota event. The rule is `stale_namespaces`' for a `ws-` team namespace: no Workspace, Bench or
/// Environment names the owner (`keep`, lowercased — `binding_name` folds case), older than a beat,
/// and the directory answered that the owner is not a person (a team, or gone — the leak's teams
/// were deleted); a person, or anyone it could not answer for, is kept. A claim that races the
/// delete is healed by the agent: `namespace_ready` recreates a missing binding.
///
/// What a delete takes, by ownerReference: the four NetworkPolicies and two RoleBindings
/// `apply_binding` stamps in the owner's namespaces — nothing else names it as owner. With nothing
/// left naming the owner there is no pod they fence or grant for, and the next claim recreates the
/// binding, which re-applies all six.
fn stale_bindings(keep: &BTreeSet<String>, seen: &[(String, String, i64)], max_age: i64, teams: &BTreeSet<String>) -> Vec<String> {
    seen.iter()
        .filter(|(_, owner, age)| {
            let o = owner.to_lowercase();
            !keep.contains(&o) && *age >= max_age && teams.contains(&o)
        })
        .map(|(name, _, _)| name.clone())
        .collect()
}

/// The binding half of the beat; see `stale_bindings`. Keep-biased like `prune_namespaces` and in
/// the same order for the same reason: bindings are listed FIRST, so the lists that spare them are
/// never older than it, and any failed list prunes NOTHING.
pub(crate) async fn prune_bindings(s: &ApiState) {
    let Some(c) = s.kube.as_ref() else { return };
    let listed = match Api::<crd::OwnerBinding>::all(c.clone()).list(&Default::default()).await {
        Ok(l) => l.items,
        Err(e) => {
            tracing::warn!(kind = "OwnerBinding", error = %e, "listing.failed");
            return;
        }
    };
    let mut keep = BTreeSet::new();
    match Api::<crd::Workspace>::all(c.clone()).list(&Default::default()).await {
        Ok(l) => keep.extend(l.items.into_iter().map(|w| w.spec.owner.to_lowercase())),
        Err(e) => {
            tracing::warn!(kind = "Workspace", error = %e, "listing.failed");
            return;
        }
    }
    // An environment's controller reads `OwnerBinding.status.team` to size its quota, and a hidden
    // builder is an environment too, so either keeps its owner's binding.
    match Api::<crd::Environment>::all(c.clone()).list(&Default::default()).await {
        Ok(l) => keep.extend(l.items.into_iter().map(|e| e.spec.owner.to_lowercase())),
        Err(e) => {
            tracing::warn!(kind = "Environment", error = %e, "listing.failed");
            return;
        }
    }
    match Api::<crd::Bench>::all(c.clone()).list(&Default::default()).await {
        Ok(l) => keep.extend(l.items.into_iter().map(|b| b.spec.owner.to_lowercase())),
        Err(kube::Error::Api(e)) if e.code == 404 => {}
        Err(e) => {
            tracing::warn!(kind = "Bench", error = %e, "listing.failed");
            return;
        }
    }
    let now = chrono::Utc::now().timestamp();
    let max_age = KEYS_RESYNC_SECS as i64;
    let seen: Vec<(String, String, i64)> = listed
        .iter()
        .map(|b| {
            let age = b.metadata.creation_timestamp.as_ref().map_or(0, |t| now - t.0.as_second());
            (b.name_any(), b.spec.owner.clone(), age)
        })
        .collect();
    // The directory is asked only about owners that pass the other guards, as `team_owners` does.
    let owners: BTreeSet<String> = seen
        .iter()
        .filter(|(_, o, age)| !keep.contains(&o.to_lowercase()) && *age >= max_age)
        .map(|(_, o, _)| o.to_lowercase())
        .collect();
    let teams = prunable_owners(s, owners, true).await;
    let api: Api<crd::OwnerBinding> = Api::all(c.clone());
    for name in stale_bindings(&keep, &seen, max_age, &teams) {
        match api.delete(&name, &Default::default()).await {
            Ok(_) => tracing::info!(binding = %name, "keys.binding.pruned"),
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(e) => tracing::warn!(binding = %name, error = %e, "keys.binding.prune.failed"),
        }
    }
}

/// Which of `seen` — `(name, spec.owner)` for every `system` environment — belongs to nobody any
/// more. `keep` is `k8s::keys_owner` of every Workspace, the SAME fold `ensure_builder` names the
/// builder with, so a team spelled `Alice` over an owner `alice` cannot keep one slug and delete
/// the other.
///
/// The keep-bias lives in the caller: an empty `keep` prunes every builder, which is exactly what
/// a failed Workspace list would produce — so `prune_builders` returns before reaching this
/// rather than passing an empty set in.
fn stale_builders(keep: &BTreeSet<String>, seen: &[(String, String)]) -> Vec<String> {
    seen.iter().filter(|(_, owner)| !keep.contains(owner)).map(|(name, _)| name.clone()).collect()
}

/// The builder half of the same beat: an owner with no workspace left has nothing to build, so
/// their hidden `bld-{slug}` environment goes — and the next workspace create writes it back.
///
/// Keep-biased exactly like `prune_namespaces`, and in the same order for the same reason: the
/// environments are listed FIRST, so the workspace list that spares them is never the older of
/// the two and a workspace created between the calls cannot lose its builder.
pub(crate) async fn prune_builders(s: &ApiState) {
    let Some(c) = s.kube.as_ref() else { return };
    let envs = match Api::<crd::Environment>::all(c.clone()).list(&Default::default()).await {
        Ok(l) => l.items,
        Err(e) => {
            tracing::warn!(kind = "Environment", error = %e, "listing.failed");
            return;
        }
    };
    let keep: BTreeSet<String> = match Api::<crd::Workspace>::all(c.clone()).list(&Default::default()).await {
        // `keys_owner`, not a hand-rolled team-else-owner: `ensure_builder` picks the slug the
        // same way, and a second spelling here would prune what a create just made.
        Ok(l) => l.items.iter().map(|w| k8s::keys_owner(&w.spec).to_string()).collect(),
        Err(e) => {
            tracing::warn!(kind = "Workspace", error = %e, "listing.failed");
            return;
        }
    };
    let seen: Vec<(String, String)> =
        envs.iter().filter(|e| e.spec.system.is_some()).map(|e| (e.name_any(), e.spec.owner.clone())).collect();
    let api: Api<crd::Environment> = Api::all(c.clone());
    for name in stale_builders(&keep, &seen) {
        match api.delete(&name, &Default::default()).await {
            Ok(_) => tracing::info!(environment = %name, "keys.builder.pruned"),
            Err(kube::Error::Api(err)) if err.code == 404 => {}
            Err(err) => tracing::warn!(environment = %name, error = %err, "keys.builder.prune.failed"),
        }
    }
}

#[cfg(test)]
mod builder_prune_tests {
    use super::stale_builders;
    use crate::crd;
    // `owner_slug` is the whole of `keys_owner`, which is what the beat calls, and it is also what
    // `ensure_builder` names the builder with — the agreement these tests are about.
    use crate::k8s::owner_slug;
    use std::collections::BTreeSet;

    /// What the beat computes from a workspace list, spelled the one way both halves must agree on.
    fn keep(workspaces: &[(&str, &str)]) -> BTreeSet<String> {
        workspaces.iter().map(|(owner, team)| owner_slug(owner, team).to_string()).collect()
    }

    fn builders(slugs: &[&str]) -> Vec<(String, String)> {
        slugs.iter().map(|s| (crd::builder_id(s), (*s).to_string())).collect()
    }

    #[test]
    fn a_member_workspace_keeps_the_teams_builder() {
        let seen = builders(&["acme", "alice"]);
        // Alice's own workspace lives in the team, so only the team's builder is spared.
        assert_eq!(stale_builders(&keep(&[("alice", "acme")]), &seen), vec!["bld-alice".to_string()]);
    }

    #[test]
    fn a_builder_whose_owner_has_no_workspace_is_pruned() {
        assert_eq!(stale_builders(&keep(&[]), &builders(&["alice"])), vec!["bld-alice".to_string()]);
        assert!(stale_builders(&keep(&[("alice", "")]), &builders(&["alice"])).is_empty());
    }

    /// The defect this rule was rewritten for: `team` differing from `owner` only in case folds to
    /// the owner on BOTH sides, so the builder a create just wrote survives the next beat.
    #[test]
    fn a_team_that_is_the_owner_in_another_case_keeps_its_builder() {
        let keep = keep(&[("alice", "Alice")]);
        assert!(keep.contains("alice"), "the fold picks the owner: {keep:?}");
        assert!(stale_builders(&keep, &builders(&["alice"])).is_empty());
    }

    /// Why `prune_builders` returns on a failed Workspace list instead of carrying on: an empty
    /// keep set is indistinguishable from "nobody has a workspace", and it prunes everything.
    #[test]
    fn an_empty_keep_set_would_prune_every_builder() {
        assert_eq!(stale_builders(&BTreeSet::new(), &builders(&["a", "b"])).len(), 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dead team's old binding goes; a live owner's, a young one, a person's and an owner the
    /// directory could not answer for all stay.
    #[test]
    fn only_an_old_binding_of_a_dead_team_is_stale() {
        let keep = BTreeSet::from(["acme".to_string()]);
        let teams = BTreeSet::from(["run-hourly-1-team".to_string(), "acme".to_string(), "run-hourly-2-team".to_string()]);
        let seen = vec![
            ("r1-run-hourly-1-team-x".to_string(), "Run-Hourly-1-Team".to_string(), 9999), // the leak
            ("r1-acme-x".to_string(), "acme".to_string(), 9999),                            // still named
            ("r1-run-hourly-2-team-x".to_string(), "run-hourly-2-team".to_string(), 10),    // too young
            ("r1-bob-x".to_string(), "bob".to_string(), 9999),                              // not a team, or unknown
        ];
        assert_eq!(stale_bindings(&keep, &seen, 300, &teams), vec!["r1-run-hourly-1-team-x".to_string()]);
        assert!(stale_bindings(&BTreeSet::new(), &seen, 300, &BTreeSet::new()).is_empty(), "no directory answer keeps all");
    }

    /// Any failed list prunes nothing: an empty keep set would read as every owner gone.
    #[tokio::test]
    async fn a_failed_listing_prunes_no_binding() {
        let list = |kind: &str, items: serde_json::Value| serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": kind, "metadata": {}, "items": items});
        let bindings = list("OwnerBindingList", serde_json::json!([{
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "OwnerBinding",
            "metadata": {"name": "r1-gone", "creationTimestamp": "2020-01-01T00:00:00Z"},
            "spec": {"owner": "gone", "region": "r1"},
        }]));
        for failing in ["workspaces", "environments", "benches"] {
            let route = |p: &str, kind: &str| {
                let path = format!("/apis/kloudlite.io/v1alpha1/{p}");
                if p == failing {
                    crate::kube_test::Route { method: "GET", path, status: 500, body: serde_json::json!({}) }
                } else {
                    crate::kube_test::get(path, list(kind, serde_json::json!([])))
                }
            };
            let (client, rec) = crate::kube_test::mock_client(vec![
                crate::kube_test::get("/apis/kloudlite.io/v1alpha1/ownerbindings", bindings.clone()),
                route("workspaces", "WorkspaceList"),
                route("environments", "EnvironmentList"),
                route("benches", "BenchList"),
            ]);
            let jwt = Arc::new(kloudlite_core::jwt::Jwt::new("test-secret-that-is-at-least-32-bytes-long").unwrap());
            let mut s = ApiState::new(jwt);
            s.kube = Some(client);
            s.directory = Some(Arc::new(AllTeams(true)));
            prune_bindings(&s).await;
            assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE")), "{failing}: {:?}", rec.calls());
        }
    }

    /// The directory as production answers it: a deleted team is `Gone` (and `is_team` false), a
    /// person is a `Person`. The gone team's binding is deleted, the person's kept, and a
    /// directory that cannot answer keeps both.
    #[tokio::test]
    async fn a_gone_teams_binding_is_deleted_and_a_persons_is_kept() {
        let list = |kind: &str, items: serde_json::Value| serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": kind, "metadata": {}, "items": items});
        let binding = |owner: &str| serde_json::json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "OwnerBinding",
            "metadata": {"name": format!("r1-{owner}"), "creationTimestamp": "2020-01-01T00:00:00Z"},
            "spec": {"owner": owner, "region": "r1"},
        });
        for answers in [true, false] {
            let (client, rec) = crate::kube_test::mock_client(vec![
                crate::kube_test::get("/apis/kloudlite.io/v1alpha1/ownerbindings", list("OwnerBindingList", serde_json::json!([binding("gone"), binding("bob")]))),
                crate::kube_test::get("/apis/kloudlite.io/v1alpha1/workspaces", list("WorkspaceList", serde_json::json!([]))),
                crate::kube_test::get("/apis/kloudlite.io/v1alpha1/environments", list("EnvironmentList", serde_json::json!([]))),
                crate::kube_test::get("/apis/kloudlite.io/v1alpha1/benches", list("BenchList", serde_json::json!([]))),
            ]);
            let jwt = Arc::new(kloudlite_core::jwt::Jwt::new("test-secret-that-is-at-least-32-bytes-long").unwrap());
            let mut s = ApiState::new(jwt);
            s.kube = Some(client);
            s.directory = Some(Arc::new(AllTeams(answers)));
            prune_bindings(&s).await;
            let deleted: Vec<String> = rec.calls().into_iter().filter(|c| c.starts_with("DELETE")).collect();
            let want = if answers { vec!["DELETE /apis/kloudlite.io/v1alpha1/ownerbindings/r1-gone".to_string()] } else { vec![] };
            assert_eq!(deleted, want, "answers={answers}");
        }
    }

    /// A `ws-` namespace goes only for an answered TEAM: a gone owner's is kept (it holds Secrets,
    /// and "gone" may be a directory data blip), and a person's never goes.
    #[tokio::test]
    async fn only_a_known_teams_ws_namespace_is_pruned_and_a_gone_owners_is_kept() {
        let list = |kind: &str, api: &str, items: serde_json::Value| serde_json::json!({"apiVersion": api, "kind": kind, "metadata": {}, "items": items});
        let ns = |name: &str| serde_json::json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": name, "creationTimestamp": "2020-01-01T00:00:00Z"}});
        let (client, rec) = crate::kube_test::mock_client(vec![
            crate::kube_test::get("/api/v1/namespaces", list("NamespaceList", "v1", serde_json::json!([ns("ws-gone"), ns("ws-bob"), ns("ws-acme")]))),
            crate::kube_test::get("/apis/kloudlite.io/v1alpha1/workspaces", list("WorkspaceList", "kloudlite.io/v1alpha1", serde_json::json!([]))),
            crate::kube_test::get("/apis/kloudlite.io/v1alpha1/benches", list("BenchList", "kloudlite.io/v1alpha1", serde_json::json!([]))),
            crate::kube_test::get("/api/v1/namespaces/ws-acme/pods", list("PodList", "v1", serde_json::json!([]))),
        ]);
        let jwt = Arc::new(kloudlite_core::jwt::Jwt::new("test-secret-that-is-at-least-32-bytes-long").unwrap());
        let mut s = ApiState::new(jwt);
        s.kube = Some(client);
        s.directory = Some(Arc::new(AllTeams(true)));
        prune_namespaces(&s).await;
        let deleted: Vec<String> = rec.calls().into_iter().filter(|c| c.starts_with("DELETE")).collect();
        assert_eq!(deleted, vec!["DELETE /api/v1/namespaces/ws-acme".to_string()]);
    }

    /// Mirrors production: `is_team` is false for a deleted team. `owner_kind` answers `bob` as a
    /// person, `acme` as a live team and anything else as gone — or, with `false`, fails every read.
    struct AllTeams(bool);
    #[async_trait::async_trait]
    impl crate::api::Directory for AllTeams {
        async fn owner_kind(&self, slug: &str) -> Result<crate::api::OwnerKind, String> {
            match (self.0, slug) {
                (false, _) => Err("unreadable".into()),
                (true, "bob") => Ok(crate::api::OwnerKind::Person),
                (true, "acme") => Ok(crate::api::OwnerKind::Team),
                (true, _) => Ok(crate::api::OwnerKind::Gone),
            }
        }
        async fn teams_for(&self, _u: &str) -> Vec<String> {
            Vec::new()
        }
        async fn is_live(&self, _j: &str) -> bool {
            true
        }
        async fn for_owner(&self, _o: &str) -> Option<crate::api::OwnerMaterial> {
            None
        }
        async fn authorized_keys_for_owner(&self, _o: &str) -> Option<String> {
            None
        }
        async fn owners_of(&self, _e: &str) -> Vec<String> {
            Vec::new()
        }
        async fn team_role(&self, _u: &str, _t: &str) -> Option<crate::api::TeamRole> {
            None
        }
        async fn is_team(&self, _s: &str) -> bool {
            true
        }
        async fn ensure_user(&self, _e: &str, _n: &str, _u: &str) -> Result<(), String> {
            Err("no".into())
        }
        async fn add_superadmin(&self, _e: &str, _b: &str) -> Result<(), String> {
            Err("no".into())
        }
    }

    #[test]
    fn the_beat_lease_names_itself_and_stamps_renew_time() {
        let now = k8s_openapi::jiff::Timestamp::from_second(1_000).unwrap();
        let v = serde_json::to_value(beat_lease(now)).unwrap();
        assert_eq!((v["metadata"]["name"].as_str(), v["metadata"]["namespace"].as_str()), (Some(KEYS_BEAT_LEASE), Some("kube-system")));
        assert_eq!(v["spec"]["renewTime"], serde_json::to_value(k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime(now)).unwrap());
    }

    /// The whole rule, and every way a namespace earns its keep. The `wt-bob-dead` case is the
    /// 2026-09-08 leak; every other row is one that must survive it.
    #[test]
    fn only_an_unused_old_team_namespace_is_stale() {
        let keep = BTreeSet::from(["wt-bob-abc".to_string(), "ws-bob".to_string()]);
        let seen = vec![
            ("wt-bob-abc".to_string(), 9999),   // a workspace resolves to it
            ("ws-bob".to_string(), 9999),       // personal, and in use
            ("ws-carol".to_string(), 9999),     // personal and unused — still never a candidate
            ("wt-bob-dead".to_string(), 9999),  // the only stale one
            ("wt-bob-young".to_string(), 10),   // too new to judge
        ];
        assert_eq!(stale_namespaces(&keep, &seen, 300, &BTreeSet::new()), vec!["wt-bob-dead".to_string()]);
    }

    /// The 2026-09-12 leak: a workspace owned by a TEAM slug gets `ws-{team}` from `ws_namespace`,
    /// and the old rule skipped every `ws-` name, so 89 of them piled up. A person's stays exempt
    /// in exactly the same state, and a team's that a Workspace still resolves to is kept.
    #[test]
    fn a_team_owned_ws_namespace_is_stale_but_a_persons_never_is() {
        let teams = BTreeSet::from(["run-hourly-1-team".to_string(), "acme".to_string()]);
        let seen = vec![
            ("ws-run-hourly-1-team".to_string(), 9999), // team-owned, nothing resolves to it
            ("ws-bob".to_string(), 9999),               // a person, same state — exempt
            ("ws-acme".to_string(), 9999),              // team-owned, but a workspace names it
            ("ws-run-hourly-2-team".to_string(), 10),   // team-owned but too new to judge
        ];
        let keep = BTreeSet::from(["ws-acme".to_string()]);
        assert_eq!(
            stale_namespaces(&keep, &seen, 300, &teams),
            vec!["ws-run-hourly-1-team".to_string()]
        );
    }

    /// An empty keep set is the shape a region with no workspaces has, and it must NOT turn every
    /// personal namespace into litter — only the team ones age out.
    #[test]
    fn an_empty_keep_set_still_spares_every_personal_namespace() {
        let seen = vec![("ws-bob".to_string(), 9999), ("wt-bob-x".to_string(), 9999)];
        assert_eq!(stale_namespaces(&BTreeSet::new(), &seen, 300, &BTreeSet::new()), vec!["wt-bob-x".to_string()]);
    }

    /// The object is the file plus a generation and nothing else: no ownerReference (it outlives
    /// every workspace), no node (every node converges it), the owner handle as its name.
    #[test]
    fn the_projection_names_the_owner_and_carries_the_file() {
        let o = owner_keys("acme", 1_700_000_000_000, "ssh-ed25519 AAAA a\n".into());
        assert_eq!(o.metadata.name.as_deref(), Some("acme"));
        assert_eq!(o.spec.generation, 1_700_000_000_000);
        assert_eq!(o.spec.authorized_keys, "ssh-ed25519 AAAA a\n");
        assert!(o.metadata.owner_references.is_none());
    }

    /// A team workspace's namespace wears its OWNER's handle, so the beat's set has to come from
    /// `spec.owner`/`spec.team` — and from every object that already exists, or a key revoked
    /// after the last workspace went away would never reach the file.
    #[test]
    fn the_beat_projects_every_person_and_team_and_prunes_what_nothing_names() {
        let ws = [
            ("karthik".to_string(), String::new()),
            ("karthik".to_string(), "acme".to_string()),
            ("meera".to_string(), "acme".to_string()),
        ];
        let (live, stale) = owner_set(ws, vec!["gone".into(), "karthik".into()]);
        assert_eq!(live, vec!["acme", "karthik", "meera"]);
        assert_eq!(stale, vec!["gone"]);
        assert_eq!(owner_set([(String::new(), String::new())], vec![]), (vec![], vec![]));
    }

    /// The beat's ONLY rotation path for `registry-token`: `refresh_user_key_secrets` — the same
    /// function `keys_changed` uses — must run for every owner a Workspace names, and for no
    /// other owner. Namespace listing is `refresh_user_key_secrets`'s first HTTP call and happens
    /// before it ever checks `s.keys`, so its presence/absence is what this test observes.
    #[tokio::test]
    async fn the_beat_refreshes_user_key_for_an_owner_with_a_workspace_and_no_one_else() {
        let ws_list = serde_json::json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "WorkspaceList", "metadata": {},
            "items": [{
                "apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace",
                "metadata": {"name": "ws-1"},
                "spec": {
                    "owner": "acme", "team": "", "name": "dev", "region": "r1",
                    "image": "", "packages": [], "desiredState": "running",
                },
            }],
        });
        let owner_keys_list = serde_json::json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "OwnerKeysList", "metadata": {}, "items": [],
        });
        let ns_list = serde_json::json!({"apiVersion": "v1", "kind": "NamespaceList", "metadata": {}, "items": []});
        let (client, rec) = crate::kube_test::mock_client(vec![
            crate::kube_test::get("/apis/kloudlite.io/v1alpha1/workspaces", ws_list),
            crate::kube_test::get("/apis/kloudlite.io/v1alpha1/ownerkeys", owner_keys_list),
            crate::kube_test::get("/api/v1/namespaces", ns_list),
        ]);
        let jwt = Arc::new(kloudlite_core::jwt::Jwt::new("test-secret-that-is-at-least-32-bytes-long").unwrap());
        let mut s = ApiState::new(jwt);
        s.kube = Some(client);
        project_all(&s).await;
        assert!(rec.calls().contains(&"GET /api/v1/namespaces".to_string()), "acme has a workspace and must be refreshed");
        // No second owner exists in the fixture at all, so a bug that refreshed every owner
        // regardless of ownership would still pass the assertion above — this counts calls
        // instead, which catches "refreshed acme twice" or "refreshed a phantom owner" either way.
        assert_eq!(rec.calls().iter().filter(|c| *c == "GET /api/v1/namespaces").count(), 1);
    }

    /// Residual C1: a bench-only owner is projected and their namespaces refreshed on the beat; a
    /// failed Bench list prunes nothing.
    #[tokio::test]
    async fn the_beat_refreshes_a_bench_only_owner_and_a_failed_bench_list_prunes_nothing() {
        let list = |kind: &str, items: serde_json::Value| serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": kind, "metadata": {}, "items": items});
        let bench = list("BenchList", serde_json::json!([{
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "Bench", "metadata": {"name": "bench-1"},
            "spec": {"owner": "alice", "team": "acme", "image": "i", "desiredState": "running"},
        }]));
        let stale = list("OwnerKeysList", serde_json::json!([{"apiVersion": "kloudlite.io/v1alpha1", "kind": "OwnerKeys", "metadata": {"name": "gone"}, "spec": {"generation": 1, "authorizedKeys": ""}}]));
        let ns = serde_json::json!({"apiVersion": "v1", "kind": "NamespaceList", "metadata": {}, "items": []});
        let run = |bench_route: crate::kube_test::Route| {
            let (client, rec) = crate::kube_test::mock_client(vec![
                crate::kube_test::get("/apis/kloudlite.io/v1alpha1/workspaces", list("WorkspaceList", serde_json::json!([]))),
                crate::kube_test::get("/apis/kloudlite.io/v1alpha1/ownerkeys", stale.clone()),
                crate::kube_test::get("/api/v1/namespaces", ns.clone()),
                bench_route,
            ]);
            let jwt = Arc::new(kloudlite_core::jwt::Jwt::new("test-secret-that-is-at-least-32-bytes-long").unwrap());
            let mut s = ApiState::new(jwt);
            s.kube = Some(client);
            (s, rec)
        };
        let (s, rec) = run(crate::kube_test::get("/apis/kloudlite.io/v1alpha1/benches", bench.clone()));
        project_all(&s).await;
        // alice and acme: one namespace refresh each.
        assert_eq!(rec.calls().iter().filter(|c| *c == "GET /api/v1/namespaces").count(), 2, "{:?}", rec.calls());
        assert!(rec.calls().contains(&"DELETE /apis/kloudlite.io/v1alpha1/ownerkeys/gone".to_string()));

        let (s, rec) = run(crate::kube_test::Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/benches".into(), status: 500, body: serde_json::json!({}) });
        project_all(&s).await;
        assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE")), "{:?}", rec.calls());
    }

    /// C1: a `wt-` namespace holding only an idle bench (no pod, no workspace) is never pruned.
    #[tokio::test]
    async fn a_namespace_a_bench_resolves_to_is_never_pruned() {
        let ns = crd::ws_namespace("alice", "acme");
        let list = |kind: &str, api: &str, items: serde_json::Value| serde_json::json!({"apiVersion": api, "kind": kind, "metadata": {}, "items": items});
        let (client, rec) = crate::kube_test::mock_client(vec![
            crate::kube_test::get("/api/v1/namespaces", list("NamespaceList", "v1", serde_json::json!([
                {"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": ns, "creationTimestamp": "2020-01-01T00:00:00Z"}}
            ]))),
            crate::kube_test::get("/apis/kloudlite.io/v1alpha1/workspaces", list("WorkspaceList", "kloudlite.io/v1alpha1", serde_json::json!([]))),
            crate::kube_test::get("/apis/kloudlite.io/v1alpha1/benches", list("BenchList", "kloudlite.io/v1alpha1", serde_json::json!([{
                "apiVersion": "kloudlite.io/v1alpha1", "kind": "Bench", "metadata": {"name": "bench-1"},
                "spec": {"owner": "alice", "team": "acme", "image": "i", "desiredState": "running"},
                "status": {"phase": "idle", "nodeName": "node-a"},
            }]))),
            crate::kube_test::get(format!("/api/v1/namespaces/{ns}/pods"), list("PodList", "v1", serde_json::json!([]))),
        ]);
        let jwt = Arc::new(kloudlite_core::jwt::Jwt::new("test-secret-that-is-at-least-32-bytes-long").unwrap());
        let mut s = ApiState::new(jwt);
        s.kube = Some(client);
        prune_namespaces(&s).await;
        assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE")), "{:?}", rec.calls());
        assert!(rec.calls().contains(&"GET /apis/kloudlite.io/v1alpha1/benches".to_string()), "{:?}", rec.calls());
    }

    /// The back-fill: `ensure_builder` used to run only from `create_ws`, so every owner whose
    /// workspaces predate the release had no builder and every build was refused. The beat writes
    /// one per owner SLUG — a team workspace's builder belongs to the team, not to the person.
    #[tokio::test]
    async fn the_beat_writes_a_builder_for_every_owner_a_workspace_names() {
        let ws = |name: &str, owner: &str, team: &str| {
            serde_json::json!({
                "apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace",
                "metadata": {"name": name},
                "spec": {
                    "owner": owner, "team": team, "name": "dev", "region": "r1",
                    "image": "", "packages": [], "desiredState": "running",
                },
            })
        };
        let ws_list = serde_json::json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "WorkspaceList", "metadata": {},
            "items": [ws("ws-1", "acme", ""), ws("ws-2", "meera", "widgets")],
        });
        let empty = |kind: &str| serde_json::json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": kind, "metadata": {}, "items": [],
        });
        let (client, rec) = crate::kube_test::mock_client(vec![
            crate::kube_test::get("/apis/kloudlite.io/v1alpha1/workspaces", ws_list),
            crate::kube_test::get("/apis/kloudlite.io/v1alpha1/ownerkeys", empty("OwnerKeysList")),
            crate::kube_test::get(
                "/api/v1/namespaces",
                serde_json::json!({"apiVersion": "v1", "kind": "NamespaceList", "metadata": {}, "items": []}),
            ),
        ]);
        let jwt = Arc::new(kloudlite_core::jwt::Jwt::new("test-secret-that-is-at-least-32-bytes-long").unwrap());
        let mut s = ApiState::new(jwt);
        s.kube = Some(client);
        project_all(&s).await;
        let patched: Vec<String> = rec
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("PATCH /apis/kloudlite.io/v1alpha1/environments/"))
            .collect();
        assert_eq!(
            patched,
            vec![
                "PATCH /apis/kloudlite.io/v1alpha1/environments/bld-acme".to_string(),
                // The team's slug, not the person's: `bld-meera` would be pruned next beat.
                "PATCH /apis/kloudlite.io/v1alpha1/environments/bld-widgets".to_string(),
            ]
        );
    }
}
