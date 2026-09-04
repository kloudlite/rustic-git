//! The ONE listing every per-node beat shares.
//!
//! `pull_beat_with` used to make ~13 cluster-wide LISTs per node per beat plus a full
//! `VolumeReplica` list per volume inside `pull_volume`, and `sync::live_worktrees` and
//! `snapshot::worktree_heads` each made their own copy of the same "parents on this node, by
//! volumeRef" query. This module is that query, written once and threaded through — the same shape
//! `nodes`/`floor`/`now` already take in `pull_beat_with`, and for the same reason: every decision
//! in one beat must agree on one view of the cluster.
//!
//! The four listings are four separate round trips with no shared resourceVersion, so a `Beat` is
//! a consistent view only by convention: a consumer that finds an object present in one list and
//! absent from another must SKIP it this beat, never delete on the strength of the absence.
//!
//! `None` means the cluster could not be fully listed. It is NOT an empty result: every consumer
//! here decides what to delete, retire or unclaim, and a partial view is exactly the case that
//! would drop a copy nobody else holds.

use crate::controller::Ctx;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::ListParams;
use kube::{Api, ResourceExt};
use kloudlite_git_workspaces::crd;
use std::collections::HashSet;
use std::sync::Arc;

/// A `Workspace` or an `Environment` claimed by this node, flattened to the fields the beats
/// actually read. The two kinds share no status type, which is the whole reason four copies of
/// this query existed.
#[derive(Clone, Debug)]
pub struct Parent {
    pub kind: &'static str,
    /// The CR name, which is also the WORKTREE name (`Pool::worktree`) — never the volume's.
    pub name: String,
    pub volume: String,
    pub owner: String,
    /// `status.nodeName` — the claim, which is how `beat` splits its ONE cluster-wide listing into
    /// "mine" without a second, node-scoped round trip.
    pub node_name: String,
    pub head: Option<String>,
    pub phase: crd::Phase,
    /// `None` for an Environment: it has no single pod, and `is_live_worktree` says so.
    pub pod_ref: Option<String>,
    pub owner_ref: OwnerReference,
    /// The `Replicated` condition's answer, as the OWNER wrote it. Read, never recomputed: the
    /// sweep runs on every node, and a second computation of "is it replicated" on a node that is
    /// not the owner is a second truth that can disagree with the one the UI shows.
    pub replicated: bool,
    /// The parent's definition, as of this listing — stamped onto every cut so a snapshot names
    /// what it was a snapshot OF without a second read back through the parent.
    pub state: crd::SnapshotState,
}

impl Parent {
    /// Something is writing to this worktree right now, so the sync beat has a generation to read.
    /// A workspace needs a pod (without one nothing writes and its last sync point is current);
    /// an environment's StatefulSets are not a single `podRef`, so `Stopped` is the only bar.
    pub fn is_live_worktree(&self) -> bool {
        self.phase != crd::Phase::Stopped && (self.kind == "Environment" || self.pod_ref.is_some())
    }
}

/// Volumes and replicas cluster-wide (placement is a cluster-wide decision), parents scoped to
/// this node.
pub struct Beat {
    pub volumes: Vec<crd::Volume>,
    pub replicas: Vec<crd::VolumeReplica>,
    pub parents: Vec<Parent>,
    /// Every parent in the cluster, unscoped. The dead-node sweep decides per VOLUME, and the
    /// volumes it decides about are owned by a node that is not this one — `parents` would show it
    /// none of them.
    pub all_parents: Vec<Parent>,
}

impl Beat {
    /// The volumes a worktree runs against on this node — never retire or release one of these.
    pub fn hosted_volumes(&self) -> HashSet<String> {
        self.parents.iter().map(|p| p.volume.clone()).collect()
    }
}

/// Both parent kinds, this node's only. Server-side scoping via `status.nodeName`, which both CRDs
/// declare selectable; the local re-check stays because a cluster on an older CRD would hand back
/// every node's objects and this node would act on someone else's.
pub async fn parents_on_node(ctx: &Arc<Ctx>) -> Option<Vec<Parent>> {
    let mine = ListParams::default().fields(&format!("status.nodeName={}", ctx.node));
    parents_matching(ctx, &mine, Some(&ctx.node)).await
}

/// Every parent in the cluster, however placed — the listing the per-volume sweep decides on.
pub async fn all_parents(ctx: &Arc<Ctx>) -> Option<Vec<Parent>> {
    parents_matching(ctx, &ListParams::default(), None).await
}

/// Every parent on ONE volume, cluster-wide — the sibling set a start's placement decision needs.
/// Cluster-wide because the decision is per volume and a parent of it may still be placed on the
/// node this one is about to hand the volume to.
pub async fn parents_on_volume(ctx: &Arc<Ctx>, volume: &str) -> Option<Vec<Parent>> {
    Some(all_parents(ctx).await?.into_iter().filter(|p| p.volume == volume).collect())
}

/// Both listings' one body. `on_node` is the local re-check, not the selector: a cluster on an
/// older CRD would ignore the field selector and hand back every node's objects.
async fn parents_matching(ctx: &Arc<Ctx>, mine: &ListParams, on_node: Option<&str>) -> Option<Vec<Parent>> {
    let mut out = Vec::new();
    match Api::<crd::Workspace>::all(ctx.client.clone()).list(mine).await {
        Ok(list) => {
            for w in &list.items {
                let Some(st) = w.status.as_ref() else { continue };
                let (Some(volume), Ok(owner_ref)) =
                    (st.volume_ref.clone(), crate::controller::owner_ref_of_kind(w))
                else {
                    continue;
                };
                if on_node.is_some_and(|n| st.node_name != n) {
                    continue;
                }
                out.push(Parent {
                    kind: "Workspace",
                    name: w.name_any(),
                    volume,
                    owner: w.spec.owner.clone(),
                    node_name: st.node_name.clone(),
                    head: st.head.clone(),
                    phase: st.phase,
                    pod_ref: st.pod_ref.clone(),
                    owner_ref,
                    replicated: is_replicated(&st.conditions),
                    state: crd::SnapshotState::of_workspace(w),
                });
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "listing this node's workspaces; this beat does nothing");
            return None;
        }
    }
    match Api::<crd::Environment>::all(ctx.client.clone()).list(mine).await {
        Ok(list) => {
            for e in &list.items {
                let Some(st) = e.status.as_ref() else { continue };
                let (Some(volume), Ok(owner_ref)) =
                    (st.volume_ref.clone(), crate::controller::owner_ref_of_kind(e))
                else {
                    continue;
                };
                if on_node.is_some_and(|n| st.node_name != n) {
                    continue;
                }
                out.push(Parent {
                    kind: "Environment",
                    name: e.name_any(),
                    volume,
                    owner: e.spec.owner.clone(),
                    node_name: st.node_name.clone(),
                    head: st.head.clone(),
                    phase: st.phase,
                    pod_ref: None,
                    owner_ref,
                    replicated: is_replicated(&st.conditions),
                    state: crd::SnapshotState::of_environment(e),
                });
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "listing this node's environments; this beat does nothing");
            return None;
        }
    }
    Some(out)
}

fn is_replicated(conditions: &[crd::Condition]) -> bool {
    conditions.iter().any(|c| c.type_ == "Replicated" && c.status == "True")
}

/// Everything one pull beat reads about the cluster: four listings, once.
pub async fn beat(ctx: &Arc<Ctx>) -> Option<Beat> {
    let volumes = match Api::<crd::Volume>::all(ctx.client.clone()).list(&ListParams::default()).await {
        Ok(l) => l.items,
        Err(e) => {
            tracing::warn!(error = %e, "pull: listing volumes; this beat does nothing");
            return None;
        }
    };
    let replicas = match Api::<crd::VolumeReplica>::all(ctx.client.clone()).list(&ListParams::default()).await {
        Ok(l) => l.items,
        Err(e) => {
            tracing::warn!(error = %e, "pull: listing replicas; this beat does nothing");
            return None;
        }
    };
    // ONE parent listing, split locally: the sweep decides per volume cluster-wide, and a second
    // node-scoped listing would be the same two round trips again for a subset of these rows.
    let all_parents = all_parents(ctx).await?;
    let parents = all_parents.iter().filter(|p| p.node_name == ctx.node).cloned().collect();
    Some(Beat { volumes, replicas, parents, all_parents })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::test_ctx;
    use kloudlite_git_workspaces::kube_test::Route;

    fn list_of(kind: &str, items: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({"apiVersion": "v1", "kind": format!("{kind}List"), "items": items})
    }

    const VOLUMES: &str = "/apis/kloudlite-git.io/v1alpha1/volumes";
    const VOLREPLICAS: &str = "/apis/kloudlite-git.io/v1alpha1/volumereplicas";
    const WORKSPACES: &str = "/apis/kloudlite-git.io/v1alpha1/workspaces";
    const ENVIRONMENTS: &str = "/apis/kloudlite-git.io/v1alpha1/environments";

    fn ws(name: &str, node: &str, volume: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "Workspace",
            "metadata": {"name": name, "uid": format!("{name}-uid")},
            "spec": {"owner": "alice", "team": "", "name": name, "region": "r1",
                     "image": "", "packages": [], "desiredState": "running"},
            "status": {"phase": "ready", "nodeName": node, "volumeRef": volume,
                       "podRef": format!("ws-alice/{name}"), "head": "v1-aaaa"},
        })
    }

    /// FOUR listings, not thirteen: one Volume, one VolumeReplica, one Workspace, one Environment.
    /// The parent listing is cluster-wide (the per-volume sweep decides about volumes this node
    /// does not own) and split locally into `parents` — a second node-scoped listing would be the
    /// same two round trips again for a subset of these rows.
    #[tokio::test]
    async fn one_beat_is_four_listings_and_the_parents_are_split_locally() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![
            Route { method: "GET", path: VOLUMES.into(), status: 200, body: list_of("Volume", vec![]) },
            Route { method: "GET", path: VOLREPLICAS.into(), status: 200, body: list_of("VolumeReplica", vec![]) },
            Route { method: "GET", path: WORKSPACES.into(), status: 200, body: list_of("Workspace", vec![ws("ws-1", "node-a", "v1"), ws("ws-2", "node-b", "v2")]) },
            Route { method: "GET", path: ENVIRONMENTS.into(), status: 200, body: list_of("Environment", vec![]) },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        let b = beat(&ctx).await.expect("a full listing");
        assert_eq!(b.all_parents.len(), 2, "every parent in the cluster");
        assert_eq!(b.parents.len(), 1);
        assert_eq!(b.parents[0].volume, "v1");
        assert_eq!(b.hosted_volumes(), ["v1".to_string()].into_iter().collect());
        assert_eq!(rec.calls().iter().filter(|c| c.starts_with("GET /apis")).count(), 4, "{:?}", rec.calls());
    }

    fn parent(kind: &'static str, phase: crd::Phase, pod_ref: Option<&str>) -> Parent {
        Parent {
            kind,
            name: "p".into(),
            volume: "v1".into(),
            owner: "alice".into(),
            node_name: "node-a".into(),
            head: None,
            phase,
            pod_ref: pod_ref.map(Into::into),
            owner_ref: OwnerReference::default(),
            replicated: false,
            state: crd::SnapshotState::Workspace {
                image: "alpine:3.20".into(),
                packages: vec![],
                resources: Default::default(),
                quota_gb: 5,
                attached_environment: None,
            },
        }
    }

    /// A workspace with no pod is writing nothing, so its last sync point is already current; an
    /// environment has no single `podRef` to lose, so only `Stopped` takes it out of the beat.
    #[test]
    fn only_a_running_worktree_with_something_writing_to_it_is_live() {
        assert!(parent("Workspace", crd::Phase::Ready, Some("ws-alice/p")).is_live_worktree());
        assert!(!parent("Workspace", crd::Phase::Ready, None).is_live_worktree());
        assert!(parent("Environment", crd::Phase::Ready, None).is_live_worktree());
        assert!(!parent("Environment", crd::Phase::Stopped, None).is_live_worktree());
    }

    /// Keep-bias: a listing that could not be completed is `None`, never a short list — every
    /// consumer of this decides what to delete, retire or unclaim from it.
    #[tokio::test]
    async fn a_failed_listing_is_none_not_an_empty_beat() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![
            Route { method: "GET", path: VOLUMES.into(), status: 500, body: serde_json::json!({}) },
        ];
        let (ctx, _rec) = test_ctx(tmp.path(), "node-a", routes);
        assert!(beat(&ctx).await.is_none());
    }

    /// A parent whose status names another node is not on this node, whatever the selector did:
    /// the field selector narrows the query, the check decides.
    #[tokio::test]
    async fn a_parent_claimed_elsewhere_is_not_mine() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![
            Route { method: "GET", path: WORKSPACES.into(), status: 200, body: list_of("Workspace", vec![ws("ws-1", "node-b", "v1")]) },
            Route { method: "GET", path: ENVIRONMENTS.into(), status: 200, body: list_of("Environment", vec![]) },
        ];
        let (ctx, _rec) = test_ctx(tmp.path(), "node-a", routes);
        assert!(parents_on_node(&ctx).await.expect("listed").is_empty());
    }
}
