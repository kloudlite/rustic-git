//! Which node an owner's data lives on.
//!
//! This is the one place a node is chosen. Everything downstream — the `Volume`'s `spec.nodeName`,
//! the PV's `nodeAffinity`, and therefore where the pod runs — is derived from the answer, because
//! two places allowed to name a node is two places that can disagree about where the data is.
//!
//! The rules are carried over from the Cosmos-era scheduler, which got them right:
//!
//! * An owner is pinned to ONE node per region, and shares it with other owners. Pinning is what
//!   makes a clone a local snapshot instead of a network copy.
//! * Losing the create race means ADOPTING the winner's binding, not retrying the pick, so
//!   concurrent first objects for one owner converge instead of splitting their data.
//! * A bound node that is dead stays bound. Re-homing an owner means migrating their subvolumes; it
//!   is not a scheduling decision, and pretending otherwise silently splits an owner's data across
//!   two pools.
//!
//! What changed with Kubernetes is only the accounting: node allocatable minus the sum of scheduled
//! pod requests, from the API, instead of the old flat per-job estimate that was already marked as
//! not real accounting.

use crate::crd::{binding_name, OwnerBinding, OwnerBindingSpec};
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, ListParams, PostParams};
use kube::ResourceExt;

/// Parse a Kubernetes quantity into bytes. Only the suffixes a node's `allocatable` actually uses.
///
/// An unparseable value yields 0, which makes the node look full and pushes work elsewhere — the
/// safe direction, since the alternative is packing onto a node whose capacity we cannot read.
fn bytes(q: &str) -> i64 {
    let q = q.trim();
    let (num, mult) = match q.strip_suffix("Ki") {
        Some(n) => (n, 1024_i64),
        None => match q.strip_suffix("Mi") {
            Some(n) => (n, 1024_i64.pow(2)),
            None => match q.strip_suffix("Gi") {
                Some(n) => (n, 1024_i64.pow(3)),
                None => match q.strip_suffix("Ti") {
                    Some(n) => (n, 1024_i64.pow(4)),
                    None => (q, 1),
                },
            },
        },
    };
    num.parse::<i64>().unwrap_or(0).saturating_mul(mult)
}

fn is_ready(n: &Node) -> bool {
    n.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .is_some_and(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
}

/// Allocatable memory minus everything already requested on the node.
///
/// Requests, not usage: requests are what the scheduler itself packs against, so placing against
/// anything else would disagree with the component that has the final say.
async fn free_mem_bytes(client: &kube::Client, node: &str, allocatable: i64) -> Result<i64, kube::Error> {
    let pods: Api<Pod> = Api::all(client.clone());
    let lp = ListParams::default().fields(&format!("spec.nodeName={node}"));
    let mut requested = 0i64;
    for p in pods.list(&lp).await?.items {
        let Some(spec) = p.spec else { continue };
        for c in spec.containers {
            let Some(r) = c.resources.and_then(|r| r.requests) else { continue };
            if let Some(m) = r.get("memory") {
                requested = requested.saturating_add(bytes(&m.0));
            }
        }
    }
    Ok(allocatable.saturating_sub(requested))
}

/// The node this owner's data lives on, creating the binding on their first object in the region.
///
/// `Ok(None)` means no candidate node exists — the caller answers 503 and the user retries. It
/// never picks a node it cannot verify.
pub async fn place(
    client: &kube::Client,
    region: &str,
    owner: &str,
    role: &str,
) -> Result<Option<String>, kube::Error> {
    let bindings: Api<OwnerBinding> = Api::all(client.clone());
    let name = binding_name(region, owner);

    if let Some(b) = bindings.get_opt(&name).await? {
        // Two owner slugs differing only in case flatten to one RFC-1123 name. Adopting a binding
        // that names a DIFFERENT owner would hand this owner someone else's node, and their data
        // would land beside it.
        // ponytail: case-flattened binding names; a hash suffix if slugs ever collide for real.
        if b.spec.owner != owner {
            return Err(kube::Error::Api(Box::new(kube::core::Status {
                status: Some(kube::core::response::StatusSummary::Failure),
                code: 409,
                message: format!("binding {name} belongs to {}, not {owner}", b.spec.owner),
                reason: "Conflict".into(),
                details: None,
                metadata: None,
            })));
        }
        // A dead or NotReady bound node still returns its name. Re-homing an owner is a migration
        // of their subvolumes, not something placement decides — under Kubernetes that surfaces as
        // a pod stuck Pending, which is visible, unlike a job sitting Queued forever.
        return Ok(Some(b.spec.node_name));
    }

    let Some(node) = pick(client, role).await? else {
        return Ok(None);
    };

    let binding = OwnerBinding::new(
        &name,
        OwnerBindingSpec {
            owner: owner.to_string(),
            region: region.to_string(),
            node_name: node.clone(),
        },
    );
    match bindings.create(&PostParams::default(), &binding).await {
        Ok(_) => Ok(Some(node)),
        // Someone raced us to the first binding for this owner — adopt theirs rather than erroring,
        // so both callers converge on the same node. `resourceVersion` gives this the same
        // optimistic-concurrency guarantee the Cosmos etag did.
        Err(kube::Error::Api(ae)) if ae.code == 409 => match bindings.get_opt(&name).await? {
            Some(b) => Ok(Some(b.spec.node_name)),
            // Vanished between the conflict and the re-read; our own pick is as good as any.
            None => Ok(Some(node)),
        },
        Err(e) => Err(e),
    }
}

/// The Ready node of this role with the most free memory.
async fn pick(client: &kube::Client, role: &str) -> Result<Option<String>, kube::Error> {
    let nodes: Api<Node> = Api::all(client.clone());
    let lp = ListParams::default().labels(&format!("rustic-git.io/role={role}"));
    let mut best: Option<(i64, String)> = None;
    for n in nodes.list(&lp).await?.items {
        if !is_ready(&n) {
            continue;
        }
        let name = n.name_any();
        let allocatable = n
            .status
            .as_ref()
            .and_then(|s| s.allocatable.as_ref())
            .and_then(|a| a.get("memory"))
            .map(|q| bytes(&q.0))
            .unwrap_or(0);
        let free = free_mem_bytes(client, &name, allocatable).await?;
        if best.as_ref().is_none_or(|(b, _)| free > *b) {
            best = Some((free, name));
        }
    }
    Ok(best.map(|(_, n)| n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kube_test::{conflict, get, mock_client, not_found, post, Route};
    use serde_json::json;

    const BINDINGS: &str = "/apis/rustic-git.io/v1alpha1/ownerbindings";

    fn node(name: &str, mem: &str, ready: bool) -> serde_json::Value {
        json!({
            "metadata": {"name": name, "labels": {"rustic-git.io/role": "session"}},
            "status": {
                "allocatable": {"memory": mem},
                "conditions": [{"type": "Ready", "status": if ready {"True"} else {"False"}}]
            }
        })
    }

    fn node_list(items: Vec<serde_json::Value>) -> serde_json::Value {
        json!({"apiVersion": "v1", "kind": "NodeList", "metadata": {}, "items": items})
    }

    fn empty_pods() -> Route {
        get("/api/v1/pods", json!({"apiVersion": "v1", "kind": "PodList", "metadata": {}, "items": []}))
    }

    fn binding(name: &str, owner: &str, node: &str) -> serde_json::Value {
        json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "OwnerBinding",
            "metadata": {"name": name},
            "spec": {"owner": owner, "region": "centralindia", "nodeName": node}
        })
    }

    #[tokio::test]
    async fn first_object_creates_binding_to_sole_node() {
        let (c, rec) = mock_client(vec![
            not_found(format!("{BINDINGS}/centralindia-alice")),
            get("/api/v1/nodes", node_list(vec![node("session-0", "128Gi", true)])),
            empty_pods(),
            post(BINDINGS, binding("centralindia-alice", "alice", "session-0")),
        ]);
        let got = place(&c, "centralindia", "alice", "session").await.unwrap();
        assert_eq!(got.as_deref(), Some("session-0"));
        assert!(rec.calls().iter().any(|c| c.starts_with("POST")), "the binding must be created");
    }

    #[tokio::test]
    async fn second_owner_may_share_the_same_node() {
        // Sharing is the point: a node hosts many owners. Only the owner-to-node mapping is
        // exclusive, never the node.
        let (c, _) = mock_client(vec![
            not_found(format!("{BINDINGS}/centralindia-bob")),
            get("/api/v1/nodes", node_list(vec![node("session-0", "128Gi", true)])),
            empty_pods(),
            post(BINDINGS, binding("centralindia-bob", "bob", "session-0")),
        ]);
        assert_eq!(place(&c, "centralindia", "bob", "session").await.unwrap().as_deref(), Some("session-0"));
    }

    #[tokio::test]
    async fn owner_pins_to_their_binding_regardless_of_later_load_changes() {
        // An existing binding is answered without consulting nodes at all — pinning must not drift
        // when a roomier node appears, or a clone stops being a local snapshot.
        let (c, rec) = mock_client(vec![get(
            format!("{BINDINGS}/centralindia-alice"),
            binding("centralindia-alice", "alice", "session-0"),
        )]);
        assert_eq!(
            place(&c, "centralindia", "alice", "session").await.unwrap().as_deref(),
            Some("session-0")
        );
        assert!(
            !rec.calls().iter().any(|c| c.contains("/nodes")),
            "a pinned owner must not even look at node load"
        );
    }

    #[tokio::test]
    async fn concurrent_first_objects_for_one_owner_converge_on_one_binding() {
        // We lose the create race; the winner picked session-1. Adopting theirs is what stops one
        // owner's data being split across two pools.
        let (c, _) = mock_client(vec![
            not_found(format!("{BINDINGS}/centralindia-alice")),
            get(
                "/api/v1/nodes",
                node_list(vec![node("session-0", "128Gi", true)]),
            ),
            empty_pods(),
            conflict(BINDINGS),
            get(
                format!("{BINDINGS}/centralindia-alice"),
                binding("centralindia-alice", "alice", "session-1"),
            ),
        ]);
        assert_eq!(
            place(&c, "centralindia", "alice", "session").await.unwrap().as_deref(),
            Some("session-1"),
            "the loser of the race must adopt the winner's node, not its own pick"
        );
    }

    #[tokio::test]
    async fn dead_bound_node_leaves_placement_pinned_without_rehoming() {
        // The subvolumes are on that box. Re-homing is a migration, so placement keeps pointing at
        // it and the pod stays Pending — visible — rather than silently starting elsewhere.
        let (c, rec) = mock_client(vec![get(
            format!("{BINDINGS}/centralindia-alice"),
            binding("centralindia-alice", "alice", "session-dead"),
        )]);
        assert_eq!(
            place(&c, "centralindia", "alice", "session").await.unwrap().as_deref(),
            Some("session-dead")
        );
        assert!(!rec.calls().iter().any(|c| c.starts_with("POST")), "must not rebind");
    }

    #[tokio::test]
    async fn no_ready_node_places_nothing() {
        // `None` means the caller answers 503. Never pick a node that cannot be verified Ready.
        let (c, _) = mock_client(vec![
            not_found(format!("{BINDINGS}/centralindia-alice")),
            get("/api/v1/nodes", node_list(vec![node("session-0", "128Gi", false)])),
        ]);
        assert_eq!(place(&c, "centralindia", "alice", "session").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_binding_naming_another_owner_is_refused_not_adopted() {
        // Two slugs can flatten to one RFC-1123 name. Adopting would put this owner's data beside
        // an unrelated owner's.
        let (c, _) = mock_client(vec![get(
            format!("{BINDINGS}/centralindia-alice"),
            binding("centralindia-alice", "Alice", "session-0"),
        )]);
        assert!(place(&c, "centralindia", "alice", "session").await.is_err());
    }

    #[test]
    fn quantities_parse_and_unreadable_ones_look_full() {
        assert_eq!(bytes("1Ki"), 1024);
        assert_eq!(bytes("2Mi"), 2 * 1024 * 1024);
        assert_eq!(bytes("128Gi"), 128 * 1024_i64.pow(3));
        assert_eq!(bytes("1000"), 1000);
        // Unreadable reads as zero free, which pushes work to a node we CAN read.
        assert_eq!(bytes("12.5Gi"), 0);
        assert_eq!(bytes(""), 0);
    }
}
