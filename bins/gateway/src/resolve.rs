//! Where a workspace's sshd actually is.
//!
//! The Workspace object is the source of truth for placement, but it does not carry an address —
//! only `status.podRef`, because a pod IP changes on every recreate and a status field that stale
//! would be worse than none. So this is two GETs, always, and never a cache: a connect is rare
//! (one per ssh session) and a wrong address is a hung handshake, not a fast failure.

use axum::http::StatusCode;
use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use kloudlite_workspaces::crd::{self, Bench, BenchAccess, Phase, Workspace};
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
    // Checked before it becomes a path segment of a kube API URL. Every workspace id IS a DNS
    // label — it names a cluster-scoped object — so this refuses nothing real, and it is the one
    // thing between a token's `ws` claim and the API server's URL space (2026-09-12).
    if !is_dns_label(ws_id) {
        return Err((StatusCode::NOT_FOUND, "no such object"));
    }
    let ws = Api::<Workspace>::all(client.clone()).get(ws_id).await.map_err(api_err)?;
    // The gateway holds no directory, so a paused (or removed, which pauses) member is read off
    // the pair's Bench, which the api pauses at judgement time. No Bench is allowed — a member
    // may never have made one; an unreadable one refuses the tunnel (409, retryable), never data.
    let team = ws.spec.team.trim().to_ascii_lowercase();
    if !team.is_empty() && team != ws.spec.owner.to_ascii_lowercase() {
        // A removed member with no Bench is stamped on the Workspace itself; only a stamp the
        // membership manager wrote counts, the same provenance the api's beat decides from.
        if kloudlite_workspaces::api::membership::system_annotation(&ws.metadata, kloudlite_workspaces::api::membership::REMOVED_AT).is_some() {
            return Err((StatusCode::FORBIDDEN, "access removed"));
        }
        match Api::<Bench>::all(client.clone()).get_opt(&crd::bench_id(&ws.spec.owner, &team)).await {
            Ok(Some(b)) if b.spec.access == BenchAccess::Paused => return Err((StatusCode::FORBIDDEN, "access paused")),
            Ok(_) => {}
            Err(_) => return Err((StatusCode::CONFLICT, "bench unreadable")),
        }
    }
    let status = ws.status.ok_or((StatusCode::CONFLICT, "no status yet"))?;
    if status.phase != Phase::Ready {
        return Err((StatusCode::CONFLICT, "workspace not ready"));
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
    Ok(Target { addr: SocketAddr::new(ip, ssh_port), owner: ws.spec.owner })
}

/// The bench's harness address, same two-GET shape as `resolve`: a Bench's `status.podRef` is
/// the only place a bench's pod IP lives, and it is just as stale-prone as a workspace's.
pub async fn resolve_bench(client: &kube::Client, id: &str, port: u16) -> Result<Target, Refusal> {
    if !is_dns_label(id) {
        return Err((StatusCode::NOT_FOUND, "no such object"));
    }
    let bench = Api::<Bench>::all(client.clone()).get(id).await.map_err(api_err)?;
    if bench.spec.access == BenchAccess::Paused {
        return Err((StatusCode::FORBIDDEN, "access paused"));
    }
    let status = bench.status.ok_or((StatusCode::CONFLICT, "no status yet"))?;
    if status.phase != Phase::Ready {
        return Err((StatusCode::CONFLICT, "bench not ready"));
    }
    let pod_ref = status.pod_ref.ok_or((StatusCode::CONFLICT, "no podRef"))?;
    let (ns, name) = pod_ref.split_once('/').ok_or((StatusCode::CONFLICT, "malformed podRef"))?;
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
    Ok(Target { addr: SocketAddr::new(ip, port), owner: bench.spec.owner })
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
    use kloudlite_workspaces::kube_test::{get, mock_client, not_found};
    use serde_json::json;

    const B: &str = "/apis/kloudlite.io/v1alpha1/benches/";

    fn bench(id: &str, access: &str) -> serde_json::Value {
        json!({"apiVersion":"kloudlite.io/v1alpha1","kind":"Bench","metadata":{"name":id},
            "spec":{"owner":"paula","team":"acme","image":"i","desiredState":"running","access":access},
            "status":{"phase":"ready","podRef":"ns/p"}})
    }

    fn routes_for_ws(bench_route: Option<kloudlite_workspaces::kube_test::Route>) -> Vec<kloudlite_workspaces::kube_test::Route> {
        let ws = json!({"apiVersion":"kloudlite.io/v1alpha1","kind":"Workspace","metadata":{"name":"w1"},
            "spec":{"owner":"paula","team":"acme","name":"w1","region":"r","image":"i","desiredState":"running"},
            "status":{"phase":"ready","podRef":"ns/p"}});
        let pod = json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":"p","namespace":"ns"},"status":{"podIP":"10.0.0.1"}});
        let mut r = vec![get("/apis/kloudlite.io/v1alpha1/workspaces/w1", ws), get("/api/v1/namespaces/ns/pods/p", pod)];
        r.extend(bench_route);
        r
    }

    #[tokio::test]
    async fn a_paused_bench_is_403() {
        let (c, _) = mock_client(vec![get(format!("{B}bench-x"), bench("bench-x", "paused"))]);
        assert_eq!(resolve_bench(&c, "bench-x", 1).await.err().map(|e| e.0), Some(StatusCode::FORBIDDEN));
    }

    #[tokio::test]
    async fn a_workspace_whose_team_bench_is_paused_is_403() {
        let id = crd::bench_id("paula", "acme");
        let (c, _) = mock_client(routes_for_ws(Some(get(format!("{B}{id}"), bench(&id, "paused")))));
        assert_eq!(resolve(&c, "w1", 22).await.err().map(|e| e.0), Some(StatusCode::FORBIDDEN));
        let (c, _) = mock_client(routes_for_ws(Some(get(format!("{B}{id}"), bench(&id, "full")))));
        assert!(resolve(&c, "w1", 22).await.is_ok(), "an active member's bench admits");
    }

    #[tokio::test]
    async fn a_removed_members_stamped_workspace_is_403_without_a_bench() {
        use kloudlite_workspaces::api::membership::{MEMBERSHIP_FIELD_MANAGER, REMOVED_AT};
        let id = crd::bench_id("paula", "acme");
        let mut routes = routes_for_ws(Some(not_found(format!("{B}{id}"))));
        let mut ws = json!({"apiVersion":"kloudlite.io/v1alpha1","kind":"Workspace","metadata":{"name":"w1",
                "annotations":{REMOVED_AT:"2026-09-15T00:00:00Z"},
                "managedFields":[{"manager":MEMBERSHIP_FIELD_MANAGER,"operation":"Apply","apiVersion":"kloudlite.io/v1alpha1","fieldsType":"FieldsV1",
                    "fieldsV1":{"f:metadata":{"f:annotations":{format!("f:{REMOVED_AT}"):{}}}}}]},
            "spec":{"owner":"paula","team":"acme","name":"w1","region":"r","image":"i","desiredState":"running"},
            "status":{"phase":"ready","podRef":"ns/p"}});
        routes[0] = get("/apis/kloudlite.io/v1alpha1/workspaces/w1", ws.clone());
        let (c, _) = mock_client(routes);
        assert_eq!(resolve(&c, "w1", 22).await.err().map(|e| e.0), Some(StatusCode::FORBIDDEN));
        // A stamp anyone else wrote is no stamp.
        ws["metadata"]["managedFields"][0]["manager"] = json!("kubectl");
        let mut routes = routes_for_ws(Some(not_found(format!("{B}{id}"))));
        routes[0] = get("/apis/kloudlite.io/v1alpha1/workspaces/w1", ws);
        let (c, _) = mock_client(routes);
        assert!(resolve(&c, "w1", 22).await.is_ok());
    }

    #[tokio::test]
    async fn a_workspace_with_no_bench_resolves() {
        let id = crd::bench_id("paula", "acme");
        let (c, _) = mock_client(routes_for_ws(Some(not_found(format!("{B}{id}")))));
        assert_eq!(resolve(&c, "w1", 22).await.ok().map(|t| t.owner), Some("paula".into()));
    }
}
