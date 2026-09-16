//! Where a workspace's sshd actually is.
//!
//! The Workspace object is the source of truth for placement, but it does not carry an address —
//! only `status.podRef`, because a pod IP changes on every recreate and a status field that stale
//! would be worse than none. So this is two GETs, always, and never a cache: a connect is rare
//! (one per ssh session) and a wrong address is a hung handshake, not a fast failure.

use axum::http::StatusCode;
use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use kloudlite_workspaces::crd::{self, Access, Phase, Workspace};
use std::net::SocketAddr;

/// The pod's sshd address, and the owner the tunnel is charged to.
pub struct Target {
    pub addr: SocketAddr,
    pub owner: String,
}

/// The status a refusal becomes, with the reason for the log. 404 is "no such workspace", 409 is
/// "not right now" (starting, stopped, pod gone) — a caller retries the second and not the first.
pub type Refusal = (StatusCode, &'static str);

pub async fn resolve(client: &kube::Client, ws_id: &str, ssh_port: u16) -> Result<Target, Refusal> {
    let ws = workspace(client, ws_id).await?;
    // No `is_bench` refusal here on purpose: a bench IS a workspace, its pod runs the same sshd in
    // the same workspace container, and `kl-connect ws ssh|ide <bench>` is meant to reach it. The
    // separation that matters is between TICKET KINDS, not object kinds — a bench-session ticket
    // never reaches this function, and an ssh ticket never reaches `resolve_bench`'s port.
    // A removed member is stamped on the Workspace itself, and only a stamp the membership
    // manager wrote counts — the same provenance the api's beat decides from. It is the fallback
    // for the window before that beat has paused the workspace.
    let team = ws.spec.team.trim().to_ascii_lowercase();
    if !team.is_empty()
        && team != ws.spec.owner.to_ascii_lowercase()
        && kloudlite_workspaces::api::membership::system_annotation(
            &ws.metadata,
            kloudlite_workspaces::api::membership::REMOVED_AT,
        )
        .is_some()
    {
        return Err((StatusCode::FORBIDDEN, "access removed"));
    }
    target(client, ws, ssh_port, "workspace not ready").await
}

/// The bench's harness address. A bench IS a workspace now, so this is `resolve` plus the one
/// predicate that says the id names a bench — `is_bench`, never the name prefix — and an ordinary
/// workspace is 404 here exactly as a missing object is: the caller asked for a bench.
pub async fn resolve_bench(client: &kube::Client, id: &str, port: u16) -> Result<Target, Refusal> {
    let ws = workspace(client, id).await?;
    if !crd::is_bench(&ws) {
        return Err((StatusCode::NOT_FOUND, "no such object"));
    }
    target(client, ws, port, "bench not ready").await
}

/// The one GET, with the one thing between a token's `ws` claim and the API server's URL space:
/// every workspace id IS a DNS label — it names a cluster-scoped object — so this refuses nothing
/// real (2026-09-12). `Paused` is the membership pause the api's beat writes; it is checked here
/// because the gateway holds no directory of its own.
async fn workspace(client: &kube::Client, id: &str) -> Result<Workspace, Refusal> {
    if !is_dns_label(id) {
        return Err((StatusCode::NOT_FOUND, "no such object"));
    }
    let ws = Api::<Workspace>::all(client.clone()).get(id).await.map_err(api_err)?;
    if ws.spec.access == Access::Paused {
        return Err((StatusCode::FORBIDDEN, "access paused"));
    }
    Ok(ws)
}

/// The second GET: `status.podRef` is the only place a pod IP lives, and a status field that
/// stale would be worse than none.
async fn target(client: &kube::Client, ws: Workspace, port: u16, not_ready: &'static str) -> Result<Target, Refusal> {
    let owner = ws.spec.owner;
    let status = ws.status.ok_or((StatusCode::CONFLICT, "no status yet"))?;
    if status.phase != Phase::Ready {
        return Err((StatusCode::CONFLICT, not_ready));
    }
    let pod_ref = status.pod_ref.ok_or((StatusCode::CONFLICT, "no podRef"))?;
    let (ns, name) = pod_ref.split_once('/').ok_or((StatusCode::CONFLICT, "malformed podRef"))?;
    // A MISSING pod is 409, not 404: the workspace exists, it is simply between pods. Only the
    // workspace itself being absent is a 404, and that was already decided above.
    let pod = Api::<Pod>::namespaced(client.clone(), ns)
        .get(name)
        .await
        .map_err(|e| match api_err(e) {
            (StatusCode::NOT_FOUND, _) => (StatusCode::CONFLICT, "pod gone"),
            other => other,
        })?;
    let ip = pod
        .status
        .and_then(|s| s.pod_ip)
        .ok_or((StatusCode::CONFLICT, "pod has no IP"))?;
    let ip = ip.parse().map_err(|_| (StatusCode::CONFLICT, "pod IP is not an address"))?;
    Ok(Target { addr: SocketAddr::new(ip, port), owner })
}

/// RFC 1123: at most 63 characters of lowercase alphanumerics and dashes, starting and ending
/// with an alphanumeric. Kubernetes' own rule for the name of a cluster-scoped object.
fn is_dns_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

/// A 404 from the API server is the only one that means "there is no such thing"; every other
/// failure is the API server's, not the caller's, and must not read as "your workspace is gone".
fn api_err(e: kube::Error) -> Refusal {
    match e {
        kube::Error::Api(ae) if ae.code == 404 => (StatusCode::NOT_FOUND, "no such object"),
        _ => (StatusCode::BAD_GATEWAY, "kube api error"),
    }
}

#[cfg(test)]
mod label_tests {
    use super::is_dns_label;

    #[test]
    fn only_a_dns_label_reaches_the_api_server() {
        assert!(is_dns_label("ws-abc123"));
        for bad in ["", "-x", "x-", "Ws1", "a/b", "a?b", "../secrets", &"a".repeat(64)] {
            assert!(!is_dns_label(bad), "{bad} must be refused");
        }
    }
}

#[cfg(test)]
mod paused_tests {
    use super::*;
    use kloudlite_workspaces::kube_test::{get, mock_client};
    use serde_json::json;

    const W: &str = "/apis/kloudlite.io/v1alpha1/workspaces/";

    /// `bench: {}` is what makes it a bench — `is_bench`, never the name.
    fn ws(name: &str, access: &str, bench: bool) -> serde_json::Value {
        let mut spec = json!({"owner":"paula","team":"acme","name":name,"region":"r","image":"i",
            "desiredState":"running","access":access});
        if bench {
            spec["bench"] = json!({"model":"m"});
        }
        json!({"apiVersion":"kloudlite.io/v1alpha1","kind":"Workspace","metadata":{"name":name},
            "spec":spec,"status":{"phase":"ready","podRef":"ns/p"}})
    }

    fn routes(obj: serde_json::Value) -> Vec<kloudlite_workspaces::kube_test::Route> {
        let name = obj["metadata"]["name"].as_str().expect("named").to_string();
        let pod = json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":"p","namespace":"ns"},"status":{"podIP":"10.0.0.1"}});
        vec![get(format!("{W}{name}"), obj), get("/api/v1/namespaces/ns/pods/p", pod)]
    }

    #[tokio::test]
    async fn a_paused_workspace_is_403_on_both_paths() {
        let (c, _) = mock_client(routes(ws("bench-x", "paused", true)));
        assert_eq!(resolve_bench(&c, "bench-x", 1).await.err(), Some((StatusCode::FORBIDDEN, "access paused")));
        let (c, _) = mock_client(routes(ws("w1", "paused", false)));
        assert_eq!(resolve(&c, "w1", 22).await.err(), Some((StatusCode::FORBIDDEN, "access paused")));
    }

    #[tokio::test]
    async fn an_active_workspace_resolves() {
        let (c, _) = mock_client(routes(ws("w1", "full", false)));
        assert_eq!(resolve(&c, "w1", 22).await.ok().map(|t| t.owner), Some("paula".into()));
        let (c, _) = mock_client(routes(ws("bench-x", "full", true)));
        assert_eq!(resolve_bench(&c, "bench-x", 1).await.ok().map(|t| t.addr.port()), Some(1));
    }

    /// The bench path answers for benches only: an ordinary workspace is "no such object", so a
    /// bench ticket can never be pointed at somebody's dev workspace.
    #[tokio::test]
    async fn an_ordinary_workspace_is_not_a_bench() {
        let (c, _) = mock_client(routes(ws("w1", "full", false)));
        assert_eq!(resolve_bench(&c, "w1", 1).await.err(), Some((StatusCode::NOT_FOUND, "no such object")));
    }

    #[tokio::test]
    async fn a_removed_members_stamped_workspace_is_403() {
        use kloudlite_workspaces::api::membership::{MEMBERSHIP_FIELD_MANAGER, REMOVED_AT};
        let mut w = ws("w1", "full", false);
        w["metadata"]["annotations"] = json!({REMOVED_AT:"2026-09-15T00:00:00Z"});
        w["metadata"]["managedFields"] = json!([{"manager":MEMBERSHIP_FIELD_MANAGER,"operation":"Apply",
            "apiVersion":"kloudlite.io/v1alpha1","fieldsType":"FieldsV1",
            "fieldsV1":{"f:metadata":{"f:annotations":{format!("f:{REMOVED_AT}"):{}}}}}]);
        let (c, _) = mock_client(routes(w.clone()));
        assert_eq!(resolve(&c, "w1", 22).await.err(), Some((StatusCode::FORBIDDEN, "access removed")));
        // A stamp anyone else wrote is no stamp.
        w["metadata"]["managedFields"][0]["manager"] = json!("kubectl");
        let (c, _) = mock_client(routes(w));
        assert!(resolve(&c, "w1", 22).await.is_ok());
    }
}
