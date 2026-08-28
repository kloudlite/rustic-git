//! Where a workspace's sshd actually is.
//!
//! The Workspace object is the source of truth for placement, but it does not carry an address —
//! only `status.podRef`, because a pod IP changes on every recreate and a status field that stale
//! would be worse than none. So this is two GETs, always, and never a cache: a connect is rare
//! (one per ssh session) and a wrong address is a hung handshake, not a fast failure.

use axum::http::StatusCode;
use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use rustic_git_workspaces::crd::{Phase, Workspace};
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
    let ws = Api::<Workspace>::all(client.clone()).get(ws_id).await.map_err(api_err)?;
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

/// A 404 from the API server is the only one that means "there is no such thing"; every other
/// failure is the API server's, not the caller's, and must not read as "your workspace is gone".
fn api_err(e: kube::Error) -> Refusal {
    match e {
        kube::Error::Api(ae) if ae.code == 404 => (StatusCode::NOT_FOUND, "no such object"),
        _ => (StatusCode::BAD_GATEWAY, "kube api error"),
    }
}
