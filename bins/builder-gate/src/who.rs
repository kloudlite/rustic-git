//! Which owner is on the other end of a TCP connection.
//!
//! There is nothing in a buildkit connection to authenticate — so the answer is the pod IP, read
//! back through the workspace pods the api server already knows about. A reflector rather than a
//! GET per connection: a `docker build` opens many connections, and the API server is not a
//! per-connection dependency this path can afford (nor one it should be able to overload).

use k8s_openapi::api::core::v1::Pod;
use kloudlite_workspaces::k8s;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};

/// `(owner, team)` — the pair, not the fold, because `owner_slug` is the one place that folds.
pub trait Resolver: Send + Sync {
    fn resolve(&self, ip: IpAddr) -> Option<(String, String)>;
}

/// The fold every other tier uses: a team's builder belongs to the team, a personal one to the
/// person, and `team == owner` is the personal case spelled the long way.
pub fn slug_of(owner: &str, team: &str) -> String {
    k8s::owner_slug(owner, team).to_string()
}

#[derive(Clone, Default)]
pub struct Pods(Arc<RwLock<HashMap<IpAddr, (String, String)>>>);

impl Resolver for Pods {
    fn resolve(&self, ip: IpAddr) -> Option<(String, String)> {
        self.0.read().ok()?.get(&ip).cloned()
    }
}

impl Pods {
    /// Watch every workspace pod in the cluster and keep the IP index current.
    ///
    /// Deletes are handled, not just applies: a pod IP is recycled, and a stale entry would hand
    /// one tenant's build to another tenant's builder. That is the whole reason this is a full
    /// `watcher::Event` match rather than `applied_objects()`.
    pub fn spawn(&self, client: kube::Client) {
        let index = self.0.clone();
        tokio::spawn(async move {
            use futures::StreamExt;
            use kube::runtime::{watcher, watcher::Event, WatchStreamExt};
            let api: kube::Api<Pod> = kube::Api::all(client);
            let cfg = watcher::Config::default().labels(&format!("{}=workspace", k8s::KIND_LABEL));
            let mut stream = watcher(api, cfg).default_backoff().boxed();
            // A relist is a whole new truth: collected aside and swapped in at `InitDone`, so a
            // resync never leaves the index momentarily empty for a connection arriving mid-list.
            let mut relist: HashMap<IpAddr, (String, String)> = HashMap::new();
            while let Some(event) = stream.next().await {
                match event {
                    Ok(Event::Init) => relist.clear(),
                    Ok(Event::InitApply(p)) => {
                        if let Some((ip, who)) = entry(&p) {
                            relist.insert(ip, who);
                        }
                    }
                    Ok(Event::InitDone) => {
                        if let Ok(mut w) = index.write() {
                            *w = std::mem::take(&mut relist);
                        }
                    }
                    Ok(Event::Apply(p)) => {
                        if let (Some((ip, who)), Ok(mut w)) = (entry(&p), index.write()) {
                            w.insert(ip, who);
                        }
                    }
                    Ok(Event::Delete(p)) => {
                        if let (Some((ip, _)), Ok(mut w)) = (entry(&p), index.write()) {
                            w.remove(&ip);
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "gate.pods.watch"),
                }
            }
            // A watch that ends is a bug worth seeing rather than a gate that silently resolves
            // nobody from then on.
            tracing::error!("gate.pods.watch.ended");
        });
    }
}

/// A pod with no IP yet, or no owner label, indexes nothing — the labels are a view of
/// `spec.owner`/`spec.team` (`heal_labels` re-stamps them), which is exactly what this needs.
fn entry(p: &Pod) -> Option<(IpAddr, (String, String))> {
    let ip: IpAddr = p.status.as_ref()?.pod_ip.as_ref()?.parse().ok()?;
    let labels = p.metadata.labels.as_ref()?;
    let owner = labels.get(k8s::OWNER_LABEL)?.clone();
    let team = labels.get(k8s::TEAM_LABEL).cloned().unwrap_or_default();
    Some((ip, (owner, team)))
}
