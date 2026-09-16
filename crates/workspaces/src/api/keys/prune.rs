//! The beat's prunes: the objects nothing names any more — workspace namespaces, `OwnerBinding`s
//! and hidden builders. Split out of `keys.rs`, which had grown to three concerns (projection,
//! beat, prunes); the rules and their incident history live on each function.
//!
//! All three are keep-biased in the same way: a failed listing returns before deleting anything,
//! and only a directory that ANSWERS moves an owner into a prunable set.

use super::{ApiState, KEYS_RESYNC_SECS};
use crate::crd;
use crate::k8s;
use k8s_openapi::api::core::v1::{Namespace, Pod};
use kube::api::{Api, ListParams};
use kube::ResourceExt;
use std::collections::BTreeSet;

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
pub(super) fn stale_namespaces(
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
pub(super) async fn team_owners(s: &ApiState, keep: &BTreeSet<String>, seen: &[(String, i64)], max_age: i64) -> BTreeSet<String> {
    let owners: Vec<String> = seen
        .iter()
        .filter(|(name, age)| !keep.contains(name) && *age >= max_age)
        .filter_map(|(name, _)| name.strip_prefix("ws-").map(str::to_string))
        .collect();
    // An answered TEAM, never "gone": a `ws-` namespace holds a person's `user-key` Secret, and
    // "no such person" can be a transient directory data problem — a binding is recreated on
    // demand, a deleted Secret is not. The one exception is a gone SLO probe run team (115 of them
    // by 2026-09-16, since the probe deletes its teams): its slug is a shape no person's handle
    // takes, so "gone" there cannot be a person's glitch. Pod and age guards still apply.
    let (probe, rest): (Vec<String>, Vec<String>) = owners.into_iter().partition(|o| is_probe_run_team(o));
    let mut out = prunable_owners(s, rest, false).await;
    out.extend(prunable_owners(s, probe, true).await);
    out
}

/// `run-{suite}-{unix}[-g{n}]-{suffix}` — the probe's run id (`bins/slo/src/ctx.rs`, suites from
/// `slo::catalogue::Suite::as_str`) and a team suffix (`-team`, `-icept`, …). Equivalent regex:
/// `^run-(fast|hourly|weekly|monthly)-[0-9]+(-g[0-9]+)?-[a-z0-9-]+$`.
pub(super) fn is_probe_run_team(owner: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let Some((suite, rest)) = owner.strip_prefix("run-").and_then(|r| r.split_once('-')) else { return false };
    let Some((ts, suffix)) = rest.split_once('-') else { return false };
    ["fast", "hourly", "weekly", "monthly"].contains(&suite)
        && digits(ts)
        && !suffix.is_empty()
        && suffix.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The owners a prune may act on: the directory ANSWERED, and named a team — or, with
/// `allow_gone`, nothing at all. A person, a failed read and a missing directory all keep.
pub(super) async fn prunable_owners(s: &ApiState, owners: impl IntoIterator<Item = String>, allow_gone: bool) -> BTreeSet<String> {
    let Some(dir) = s.directory.as_ref() else { return BTreeSet::new() };
    let owners: Vec<String> = owners.into_iter().collect();
    let answers = futures::future::join_all(owners.iter().map(|o| dir.owner_kind(o))).await;
    owners
        .into_iter()
        .zip(answers)
        .filter(|(_, k)| matches!(k, Ok(crate::api::OwnerKind::Team)) || (allow_gone && matches!(k, Ok(crate::api::OwnerKind::Gone))))
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
                let reason = match name.strip_prefix("ws-") {
                    Some(o) if is_probe_run_team(o) => "probe_team",
                    Some(_) => "team",
                    None => "member",
                };
                tracing::info!(namespace = %name, %reason, "keys.namespace.pruned");
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
/// PAIRED RULE, in another binary: `binding.rs::namespace_ready` (`bins/agent`) is a read-shaped
/// gate that WRITES — it recreates a missing binding on every miss, so a claim that raced this
/// prune is healed on the spot rather than stalling a workspace in `ensure_ssh`'s 60 s retry. The
/// two rules cannot ping-pong only because of `keep`: an owner that reaches that line still holds a
/// Bench, Workspace or Environment, and `keep` spares exactly those owners, so a binding the agent
/// would recreate is never a candidate here. Loosen `keep` — or stop building it from all three
/// kinds — and the beat deletes what the agent recreates, every beat, forever. Change the two
/// together.
///
/// What a delete takes, by ownerReference: the four NetworkPolicies and two RoleBindings
/// `apply_binding` stamps in the owner's namespaces — nothing else names it as owner. With nothing
/// left naming the owner there is no pod they fence or grant for, and the next claim recreates the
/// binding, which re-applies all six.
pub(super) fn stale_bindings(keep: &BTreeSet<String>, seen: &[(String, String, i64)], max_age: i64, teams: &BTreeSet<String>) -> Vec<String> {
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
pub(super) fn stale_builders(keep: &BTreeSet<String>, seen: &[(String, String)]) -> Vec<String> {
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
