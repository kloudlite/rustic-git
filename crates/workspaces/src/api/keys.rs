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
    let (pairs, listed) = match Api::<crd::Workspace>::all(c.clone()).list(&Default::default()).await {
        Ok(l) => (l.items.into_iter().map(|w| (w.spec.owner, w.spec.team)).collect(), true),
        Err(e) => {
            tracing::warn!(kind = "Workspace", error = %e, "listing.failed");
            (Vec::new(), false)
        }
    };
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
/// to everything inside it: `ws-` (a person's own, which holds their `user-key` Secret) is never
/// a candidate whatever else is true, and one younger than a beat is left alone, which closes the
/// window between `apply_binding` creating a namespace and the workspace that needed it becoming
/// listable.
fn stale_namespaces(keep: &BTreeSet<String>, seen: &[(String, i64)], max_age: i64) -> Vec<String> {
    seen.iter()
        .filter(|(name, age)| name.starts_with("wt-") && !keep.contains(name) && *age >= max_age)
        .map(|(name, _)| name.clone())
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
    let now = chrono::Utc::now().timestamp();
    let seen: Vec<(String, i64)> = listed
        .iter()
        .map(|n| {
            // No timestamp reads as age 0 — too young to judge, which is the keeping answer.
            let age = n.metadata.creation_timestamp.as_ref().map_or(0, |t| now - t.0.as_second());
            (n.name_any(), age)
        })
        .collect();
    let api: Api<Namespace> = Api::all(c.clone());
    for name in stale_namespaces(&keep, &seen, KEYS_RESYNC_SECS as i64) {
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
            Ok(_) => tracing::info!(namespace = %name, "keys.namespace.pruned"),
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(e) => tracing::warn!(namespace = %name, error = %e, "keys.namespace.prune.failed"),
        }
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
        assert_eq!(stale_namespaces(&keep, &seen, 300), vec!["wt-bob-dead".to_string()]);
    }

    /// An empty keep set is the shape a region with no workspaces has, and it must NOT turn every
    /// personal namespace into litter — only the team ones age out.
    #[test]
    fn an_empty_keep_set_still_spares_every_personal_namespace() {
        let seen = vec![("ws-bob".to_string(), 9999), ("wt-bob-x".to_string(), 9999)];
        assert_eq!(stale_namespaces(&BTreeSet::new(), &seen, 300), vec!["wt-bob-x".to_string()]);
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
}
