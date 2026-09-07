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

/// Every owner that has a namespace in this region — the owner label is what the controller
/// stamps, so a team the api never heard of is still covered.
pub async fn project_all(s: &ApiState) {
    let Some(c) = s.kube.as_ref() else { return };
    let api: Api<k8s_openapi::api::core::v1::Namespace> = Api::all(c.clone());
    let sel = format!("{}=workspace", crate::k8s::KIND_LABEL);
    let list = match api.list(&kube::api::ListParams::default().labels(&sel)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(kind = "Namespace", error = %e, "listing.failed");
            return;
        }
    };
    let mut owners: Vec<String> = list
        .items
        .iter()
        .filter_map(|n| n.metadata.labels.as_ref()?.get(crate::k8s::OWNER_LABEL).cloned())
        .collect();
    owners.sort();
    owners.dedup();
    for o in owners {
        if let Err(e) = project(s, &o).await {
            tracing::warn!(owner = %o, error = %e, "keys.project.failed");
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
}
