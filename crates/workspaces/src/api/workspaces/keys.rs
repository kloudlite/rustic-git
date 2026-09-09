//! The `user-key` Secret a workspace pod mounts — platform key, git identity, registry token —
//! written once the workspace is placed and re-projected on every key change and resync beat.

use super::*;


/// Put the owner's platform key in their workspace namespace, once a node has taken the workspace.
///
/// The namespace is the CONTROLLER's to make, so on a first workspace it does not exist at the
/// moment of the create. Waiting for the `Placed` condition — not for the namespace — is the
/// cheapest signal that a node has claimed the object and its OwnerBinding reconciler is running.
///
/// Best effort with a 5 s ceiling, because the key install is load-bearing but not worth failing a
/// create over: `list_ws` re-installs it when the Secret is absent, and that retry is what closes
/// the first-workspace-without-a-key gap for good.
///
/// The install itself stays best effort: the pod's key mount is optional (`k8s::user_key_volume`),
/// so a key that lands late — or never — costs the workspace its git identity, not its existence.
pub(crate) async fn install_user_key_after_placed(s: &ApiState, c: &kube::Client, owner: &str, team: &str, id: &str) {
    // Nothing to install and nothing to wait for.
    if s.keys.is_none() {
        return;
    }
    let api: Api<crd::Workspace> = Api::all(c.clone());
    for _ in 0..10 {
        if let Ok(Some(w)) = api.get_opt(id).await {
            if w.status.is_some_and(|st| {
                st.conditions.iter().any(|cd| cd.type_ == "Placed" && cd.status == "True")
            }) {
                write_user_key(s, c, &crd::ws_namespace(owner, team), owner).await;
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    tracing::info!(%owner, workspace = %id, reason = "not-placed", "workspace.keys.deferred");
}


/// A key or membership change names a PERSON (their email: the namespaces touched are their own
/// handle and every team they are in) — or, from a platform-key rotation, one OWNER handle. Both
/// do the same two things per owner: re-project `OwnerKeys`, and rewrite the `user-key` Secret in
/// every namespace of that owner, because the platform PRIVATE key lives only in that Secret and a
/// rotation has just revoked the one every running pod is mounting.
pub async fn keys_changed(s: &ApiState, principal: &str) {
    let owners = if principal.contains('@') {
        let Some(dir) = s.directory.as_ref() else { return };
        dir.owners_of(principal).await
    } else {
        vec![principal.to_string()]
    };
    // Worth a line: a key change that reaches nothing is either an unclaimed handle or a
    // directory read that failed, and both look identical from the outside otherwise.
    if owners.is_empty() {
        tracing::warn!(%principal, reason = "no-owners", "keys.project.skipped");
    }
    for o in owners {
        if let Err(e) = super::super::keys::project(s, &o).await {
            tracing::warn!(owner = %o, error = %e, "keys.project.failed");
        }
        refresh_user_key_secrets(s, &o).await;
    }
}


/// Rewrite the owner's `user-key` Secret in EVERY workspace namespace they have. The namespaces
/// are found by the owner label the controller stamps, so a team the api tier has never heard of
/// is still covered. Transitional with the Secret's `authorized_keys` entry (spec §5 step 4); the
/// private-key half stays for as long as workspaces push git with a platform key.
///
/// `pub(crate)`, not private: `keys::run_beat`'s pass reuses this directly rather than
/// reimplementing "every namespace of this owner" — it is also the registry token's ONLY
/// rotation path, since nothing else re-mints the 24h token `write_user_key` puts in the Secret.
pub(crate) async fn refresh_user_key_secrets(s: &ApiState, owner: &str) {
    let Some(c) = s.kube.as_ref() else { return };
    let api: Api<k8s_openapi::api::core::v1::Namespace> = Api::all(c.clone());
    let sel = format!("{}={owner},{}=workspace", crate::k8s::OWNER_LABEL, crate::k8s::KIND_LABEL);
    let list = match api.list(&ListParams::default().labels(&sel)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(kind = "Namespace", %owner, error = %e, "listing.failed");
            return;
        }
    };
    for ns in list.items.iter().map(|n| n.name_any()) {
        write_user_key(s, c, &ns, owner).await;
    }
}


pub(crate) async fn write_user_key(s: &ApiState, c: &kube::Client, ns: &str, owner: &str) {
    let Some(store) = &s.keys else { return };
    let private = match store.user_key(owner).await {
        Ok(Some(p)) => p,
        Ok(None) => return, // never generated one; /v1/platform-key makes it on first read
        Err(e) => {
            tracing::warn!(%owner, reason = "platform-key", error = %e, "key.read.failed");
            return;
        }
    };
    let api: Api<k8s_openapi::api::core::v1::Secret> = Api::namespaced(c.clone(), ns);
    // A failed lookup writes NOTHING rather than an empty file: an empty `authorized_keys` locks
    // the owner out of a workspace they can otherwise reach, and the next call rewrites it anyway.
    // Unwired (dev, no directory) writes NOTHING for the same reason a failed lookup does: an
    // empty `authorized_keys` is not "no keys yet", it is the owner locked out of their workspace.
    let Some(lookup) = &s.directory else { return };
    let Some(material) = lookup.for_owner(owner).await else {
        tracing::warn!(%owner, reason = "owner-keys", "key.read.failed");
        return;
    };
    // Transitional, same rule: a failed lookup writes NOTHING. The Secret carries the union
    // `OwnerKeys` carries so a pod of a not-yet-upgraded agent keeps admitting the same keys;
    // dropped in the release after every region's agent is on this build (spec §5 step 4).
    let Some(authorized) = lookup.authorized_keys_for_owner(owner).await else {
        tracing::warn!(%owner, reason = "authorized-keys", "key.read.failed");
        return;
    };
    // Re-minted every rewrite of this Secret (this beat, or a key change): rotation is just the
    // next projection, no revocation code needed. `"*"` because a registry request re-checks
    // authorization against the image itself, never trusts the scope a token claims.
    let registry_token = match s.jwt.mint_registry(owner, "*", 86_400) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(%owner, error = %e, "registry-token.mint.failed");
            return;
        }
    };
    let secret = crate::k8s::user_key_secret(owner, ns, &private, &material, &authorized, &registry_token);
    if let Err(e) = api
        .patch(
            crate::k8s::USER_KEY_SECRET,
            &kube::api::PatchParams::apply("kloudlite-api").force(),
            &kube::api::Patch::Apply(&secret),
        )
        .await
    {
        tracing::warn!(%owner, error = %e, "key.install.failed");
    }
}
