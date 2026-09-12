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
    let (specs, listed) = match Api::<crd::Workspace>::all(c.clone()).list(&Default::default()).await {
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
    let pairs: Vec<(String, String)> =
        specs.into_iter().map(|w| (w.owner, w.team)).collect();
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
        prune_namespaces(&s).await;
        prune_builders(&s).await;
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
    let answers = futures::future::join_all(owners.iter().map(|o| super::scope::is_team(s, o))).await;
    owners.into_iter().zip(answers).filter(|(_, team)| *team).map(|(o, _)| o).collect()
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
