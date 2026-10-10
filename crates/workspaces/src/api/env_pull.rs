//! The `registry-pull` Secret in an environment namespace.
//!
//! Incident 2026-10-10: a private image in the owner's registry namespace sat in
//! `ImagePullBackOff` (401) because env namespaces had no pull credential. The api writes it
//! (the agent's `kloudlite-api-secrets` RoleBinding gates the namespace) once the environment is
//! placed, and the keys beat renews it: the token inside lives 24 h, the beat runs every 300 s.

use super::ApiState;
use crate::{crd, k8s};
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::ResourceExt;

/// Host only, no scheme. Unset means no secret is written: there is no host to key it by.
fn registry_host() -> Option<String> {
    std::env::var("KLOUDLITE_REGISTRY_HOST").ok().map(|h| h.trim().to_string()).filter(|h| !h.is_empty())
}

/// Best effort by design: kubelet retries `ImagePullBackOff`, so a late or missing Secret heals on
/// a later beat; failing an environment create over it would be worse.
pub(crate) async fn write_registry_pull(s: &ApiState, c: &kube::Client, ns: &str, owner: &str) {
    let Some(host) = registry_host() else {
        tracing::warn!(namespace = ns, owner, reason = "no-registry-host", "registry_pull.skipped");
        return;
    };
    // `"*"` because a registry request re-checks authorization against the image itself.
    let token = match s.jwt.mint_registry(owner, "*", 86_400) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(namespace = ns, owner, error = %e, "registry_pull.failed");
            return;
        }
    };
    let secret = k8s::registry_pull_secret(owner, ns, &host, &token);
    let api: Api<k8s_openapi::api::core::v1::Secret> = Api::namespaced(c.clone(), ns);
    if let Err(e) = api.patch(k8s::REGISTRY_PULL_SECRET, &PatchParams::apply("kloudlite-api").force(), &Patch::Apply(&secret)).await {
        tracing::warn!(namespace = ns, owner, error = %e, "registry_pull.failed");
    }
}

/// The keys beat's pass: every PLACED environment (the agent made its namespace and RoleBinding),
/// builders included — one code path. A failed list skips the pass; the next beat retries.
pub(crate) async fn refresh_registry_pulls(s: &ApiState) {
    let Some(c) = s.kube.as_ref() else { return };
    // Once per beat, not once per environment.
    if registry_host().is_none() {
        tracing::warn!(reason = "no-registry-host", "registry_pull.skipped");
        return;
    }
    let envs = match Api::<crd::Environment>::all(c.clone()).list(&ListParams::default()).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, "registry_pull.list.failed");
            return;
        }
    };
    for e in envs.items.iter().filter(|e| e.status.as_ref().is_some_and(|st| !st.node_name.is_empty())) {
        write_registry_pull(s, c, &crd::env_namespace(&e.name_any()), &e.spec.owner).await;
    }
}

/// Wait up to 30 s for a node to claim the environment, then write. Spawned from create/clone.
pub(crate) async fn pull_after_placed(s: std::sync::Arc<ApiState>, c: kube::Client, env_id: String, owner: String) {
    let api: Api<crd::Environment> = Api::all(c.clone());
    for _ in 0..30 {
        if let Ok(Some(e)) = api.get_opt(&env_id).await {
            if e.status.as_ref().is_some_and(|st| !st.node_name.is_empty()) {
                write_registry_pull(&s, &c, &crd::env_namespace(&env_id), &owner).await;
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    tracing::info!(env = %env_id, owner = %owner, reason = "not-placed", "registry_pull.deferred");
}
