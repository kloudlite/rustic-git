//! One-off: bind every team created before a region was required to the region, when there is
//! exactly one to choose. With zero or several the right answer is a person's, so it does nothing.
//! Idempotent through `bind_region`'s set-once CAS — a bound team is never touched, so running it
//! on every admin boot is safe.

use kloudlite_pulls::directory::Directory;
use kloudlite_workspaces::audit::{record, AuditEntry};
use slatedb::object_store::ObjectStore;
use std::sync::Arc;

/// At boot: read the active regions off the cluster, then `run`.
pub async fn at_boot(kube: kube::Client, dir: Arc<Directory>, os: Arc<dyn ObjectStore>) {
    let api: kube::Api<kloudlite_workspaces::crd::Region> = kube::Api::all(kube);
    match api.list(&Default::default()).await {
        Ok(list) => {
            let active: Vec<String> = list
                .items
                .iter()
                .filter(|r| r.spec.status == "active")
                .map(kube::ResourceExt::name_any)
                .collect();
            run(&active, &dir, &os).await;
        }
        Err(e) => tracing::warn!(error = %e, "team.region.backfill.failed"),
    }
}

/// How many teams it bound.
pub async fn run(active: &[String], dir: &Directory, os: &Arc<dyn ObjectStore>) -> usize {
    let [region] = active else {
        tracing::info!(regions = active.len(), "team.region.backfill.skipped");
        return 0;
    };
    let slugs = match dir.unbound_teams().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "team.region.backfill.failed");
            return 0;
        }
    };
    let mut bound = 0;
    for slug in slugs {
        match dir.bind_region(&slug, region).await {
            // Some(other) means a concurrent bind won the CAS; not ours to record.
            Ok(Some(r)) if &r == region => {
                bound += 1;
                let entry = AuditEntry {
                    ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    actor: "system".into(),
                    action: "owner.region.backfill".into(),
                    target: slug.clone(),
                    reason: Some(region.clone()),
                    result: "ok".into(),
                };
                if let Err(e) = record(os, &entry).await {
                    tracing::error!(team = %slug, error = %e, "audit.write.failed");
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(team = %slug, error = %e, "team.region.backfill.failed"),
        }
    }
    tracing::info!(bound, "team.region.backfill.done");
    bound
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> (Directory, Arc<dyn ObjectStore>) {
        let d = Directory::in_memory();
        d.upsert_user("a@x.io", "A").await.unwrap();
        d.claim_username("a@x.io", "alice").await.unwrap().unwrap();
        d.create("old", "Old", "a@x.io", "").await.unwrap().unwrap();
        d.create("placed", "Placed", "a@x.io", "elsewhere").await.unwrap().unwrap();
        (d, Arc::new(slatedb::object_store::memory::InMemory::new()))
    }

    #[tokio::test]
    async fn one_region_binds_unbound_teams_and_leaves_bound_ones() {
        let (d, os) = fixture().await;
        assert_eq!(run(&["r1".into()], &d, &os).await, 1);
        assert_eq!(d.get("old").await.unwrap().unwrap().region, "r1");
        assert_eq!(d.get("placed").await.unwrap().unwrap().region, "elsewhere");
        assert_eq!(d.user_by_handle("alice").await.unwrap().unwrap().region, "", "a person is not a team");
        assert_eq!(run(&["r1".into()], &d, &os).await, 0, "idempotent");
    }

    #[tokio::test]
    async fn zero_or_several_regions_do_nothing() {
        let (d, os) = fixture().await;
        assert_eq!(run(&[], &d, &os).await, 0);
        assert_eq!(run(&["r1".into(), "r2".into()], &d, &os).await, 0);
        assert_eq!(d.get("old").await.unwrap().unwrap().region, "");
    }
}
