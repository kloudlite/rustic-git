//! Keys are the directory's; a cluster sees one `OwnerKeys` per owner namespace, written here and
//! only here. Two triggers, one function: a key or membership change projects the owners it
//! touched, and a beat re-projects every owner, so a write that was lost is at most one beat late.

use super::ApiState;
use crate::crd;
use kube::api::{Api, Patch, PatchParams};
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
