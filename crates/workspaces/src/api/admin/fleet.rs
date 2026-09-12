//! The admin process's view of the fleet: one reflector store per kind it reads, fed by a watch,
//! so an admin page reads memory instead of listing six kinds cluster-wide per request. See
//! `docs/superpowers/specs/2026-09-10-admin-fleet-cache-design.md`.
//!
//! `all` is the one seam every reader goes through: a store that is ready answers; no cache (the
//! `user` role, the mock-client tests) or one still initialising falls back to the list the reader
//! made before this existed, so a reader never carries two code paths of its own.

use super::kube_err;
use crate::crd;
use axum::response::Response;
use futures::StreamExt;
use k8s_openapi::api::core::v1::Node;
use kube::runtime::reflector::{self, Store};
use kube::runtime::{watcher, WatchStreamExt};
use kube::{Api, Resource};
use std::sync::Arc;
use std::time::Duration;

/// How long boot waits for the stores to fill before serving anyway. An unreachable cluster must
/// not keep the admin process from coming up; its readers list until the watch catches up.
const READY_CAP: Duration = Duration::from_secs(30);

pub struct FleetCache {
    pub nodes: Store<Node>,
    pub regions: Store<crd::Region>,
    pub workspaces: Store<crd::Workspace>,
    pub environments: Store<crd::Environment>,
    pub volumes: Store<crd::Volume>,
    pub replicas: Store<crd::VolumeReplica>,
    pub snapshots: Store<crd::Snapshot>,
    pub quotas: Store<crd::Quota>,
    pub quota_requests: Store<crd::QuotaRequest>,
    pub requests: Store<crd::Request>,
    /// Every store has landed its first list. Read per request instead of awaiting readiness
    /// there: a cluster that never answers must cost a reader a list, never a hang.
    ready: std::sync::atomic::AtomicBool,
}

fn watch<K>(client: &kube::Client) -> Store<K>
where
    K: Resource<DynamicType = ()> + Clone + std::fmt::Debug + Send + Sync + serde::de::DeserializeOwned + 'static,
{
    let (store, writer) = reflector::store();
    let api: Api<K> = Api::all(client.clone());
    // The runtime's own backoff on a watch that drops; the stream is only driven, never read —
    // the store is the reader.
    let stream = reflector::reflector(writer, watcher(api, watcher::Config::default()).default_backoff());
    tokio::spawn(stream.for_each(|_| std::future::ready(())));
    store
}

impl FleetCache {
    /// Start every watch and wait (bounded) for the first list of each to land.
    pub async fn spawn(client: &kube::Client) -> Arc<FleetCache> {
        let cache = Arc::new(FleetCache {
            nodes: watch(client),
            regions: watch(client),
            workspaces: watch(client),
            environments: watch(client),
            volumes: watch(client),
            replicas: watch(client),
            snapshots: watch(client),
            quotas: watch(client),
            quota_requests: watch(client),
            requests: watch(client),
            ready: std::sync::atomic::AtomicBool::new(false),
        });
        let c = cache.clone();
        let ready = tokio::spawn(async move {
            c.nodes.wait_until_ready().await.ok();
            c.regions.wait_until_ready().await.ok();
            c.workspaces.wait_until_ready().await.ok();
            c.environments.wait_until_ready().await.ok();
            c.volumes.wait_until_ready().await.ok();
            c.replicas.wait_until_ready().await.ok();
            c.snapshots.wait_until_ready().await.ok();
            c.quotas.wait_until_ready().await.ok();
            c.quota_requests.wait_until_ready().await.ok();
            c.requests.wait_until_ready().await.ok();
            c.ready.store(true, std::sync::atomic::Ordering::Release);
            tracing::info!("admin.fleet.cache.ready");
        });
        // Bounded: the task keeps waiting past the cap and flips the flag whenever the cluster
        // answers; until then every reader lists.
        if tokio::time::timeout(READY_CAP, ready).await.is_err() {
            tracing::warn!(cap_secs = READY_CAP.as_secs(), "admin.fleet.cache.not_ready");
        }
        cache
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// Every object of a kind, from the store when the admin process has one that is ready, else
/// listed from the API server. `pick` names the store; `client` is what the list would use.
pub(crate) async fn all<K>(
    cache: Option<&FleetCache>,
    client: &kube::Client,
    pick: impl Fn(&FleetCache) -> &Store<K>,
) -> Result<Vec<Arc<K>>, Response>
where
    K: Resource<DynamicType = ()> + Clone + std::fmt::Debug + Send + Sync + serde::de::DeserializeOwned + 'static,
{
    if let Some(c) = cache.filter(|c| c.is_ready()) {
        // `state()` already hands back the store's own `Arc<K>`s — returning them means a reader
        // shares the objects instead of deep-copying every one of them per request, which on the
        // Owners page was six full copies of the fleet (2026-09-12).
        return Ok(pick(c).state());
    }
    let api: Api<K> = Api::all(client.clone());
    Ok(api.list(&kube::api::ListParams::default()).await.map_err(kube_err)?.items.into_iter().map(Arc::new).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kube_test::{get, mock_client};
    use kube::runtime::reflector::store;

    fn region(id: &str) -> crd::Region {
        let mut r = crd::Region::new(id, crd::RegionSpec { name: id.to_string(), status: "active".into() });
        r.metadata.uid = Some(format!("uid-{id}"));
        r
    }

    /// Every store empty except `regions`, which holds `fed`; `ready` as given.
    fn cache(fed: Vec<crd::Region>, ready: bool) -> FleetCache {
        let (regions, mut w) = store::<crd::Region>();
        w.apply_watcher_event(&watcher::Event::Init);
        for r in fed {
            w.apply_watcher_event(&watcher::Event::InitApply(r));
        }
        w.apply_watcher_event(&watcher::Event::InitDone);
        FleetCache {
            nodes: store().0,
            regions,
            workspaces: store().0,
            environments: store().0,
            volumes: store().0,
            replicas: store().0,
            snapshots: store().0,
            quotas: store().0,
            quota_requests: store().0,
            requests: store().0,
            ready: std::sync::atomic::AtomicBool::new(ready),
        }
    }

    fn listing(id: &str) -> (kube::Client, crate::kube_test::Recorder) {
        mock_client(vec![get(
            "/apis/kloudlite.io/v1alpha1/regions",
            serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "RegionList", "metadata": {},
                "items": [serde_json::to_value(region(id)).unwrap()]}),
        )])
    }

    #[tokio::test]
    async fn a_ready_store_answers_and_the_api_server_is_not_asked() {
        let (client, rec) = listing("from-list");
        let c = cache(vec![region("from-store")], true);
        let got = all(Some(&c), &client, |c| &c.regions).await.unwrap();
        assert_eq!(got.iter().map(|r| r.metadata.name.as_deref().unwrap()).collect::<Vec<_>>(), ["from-store"]);
        assert!(rec.calls().is_empty(), "{:?}", rec.calls());
    }

    #[tokio::test]
    async fn no_cache_or_a_store_still_filling_lists_instead() {
        let (client, rec) = listing("from-list");
        let got = all(None, &client, |c| &c.regions).await.unwrap();
        assert_eq!(got[0].metadata.name.as_deref(), Some("from-list"));
        let c = cache(vec![region("from-store")], false);
        let got = all(Some(&c), &client, |c| &c.regions).await.unwrap();
        assert_eq!(got[0].metadata.name.as_deref(), Some("from-list"), "an unready store must never read as empty");
        assert_eq!(rec.calls().len(), 2);
    }
}
