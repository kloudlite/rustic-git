//! The `user-key` Secret a workspace pod mounts — platform key, git identity, registry and
//! workspace tokens —
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
    install_user_key_when::<crd::Workspace>(s, c, owner, team, id, |w| {
        w.status.as_ref().is_some_and(|st| st.conditions.iter().any(|cd| cd.type_ == "Placed" && cd.status == "True"))
    })
    .await
}


/// The same wait for any parent that lands in `ws_namespace(owner, team)`: a bench is claimed by a
/// node the way a workspace is, and `placed` says how each kind reports it.
pub(crate) async fn install_user_key_when<K>(s: &ApiState, c: &kube::Client, owner: &str, team: &str, id: &str, placed: fn(&K) -> bool)
where
    K: kube::Resource<Scope = kube::core::ClusterResourceScope, DynamicType = ()> + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    // Nothing to install and nothing to wait for.
    if s.keys.is_none() {
        return;
    }
    let api: Api<K> = Api::all(c.clone());
    for _ in 0..10 {
        if let Ok(Some(o)) = api.get_opt(id).await {
            if placed(&o) {
                write_user_key(s, c, &crd::ws_namespace(owner, team), owner).await;
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    tracing::info!(%owner, parent = %id, reason = "not-placed", "workspace.keys.deferred");
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


/// The space (the team slug, or the owner's own handle for their personal space) a workspace
/// namespace belongs to.
///
/// `ws_namespace` HASHES the team into the name, so a team namespace cannot be read back from its
/// own name — the personal one is computed, and a team one is recovered from an object that lives
/// there. `None` when nothing does, which is a namespace the prune beat is about to delete.
async fn space_of(c: &kube::Client, ns: &str, owner: &str) -> Option<String> {
    if ns == crd::ws_namespace(owner, "") {
        return Some(owner.to_lowercase());
    }
    let sel = ListParams::default().labels(&format!("{}={owner}", crate::k8s::OWNER_LABEL));
    // Workspaces only: a bench is one of them, and carries the same `OWNER_LABEL`.
    Api::<crd::Workspace>::all(c.clone())
        .list(&sel)
        .await
        .ok()?
        .items
        .into_iter()
        .map(|w| w.spec.team)
        .find(|t| crd::ws_namespace(owner, t) == ns)
        .map(|t| t.to_lowercase())
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
    // The pod's own platform credential, scoped to the SPACE the namespace belongs to — the token
    // covers the owner's workspaces there, because `user-key` is per owner namespace and nothing
    // finer would survive a beat that rewrites one Secret for every workspace in it.
    let workspace_token = match space_of(c, ns, owner).await {
        Some(space) => match keep_or_mint(&s.jwt, workspace_token(c, ns).await.as_deref(), owner, &space) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(%owner, error = %e, "workspace-token.mint.failed");
                return;
            }
        },
        // Fail closed rather than guess a space: an empty item is a credential `/v1` refuses,
        // where a wrong one would be a token refused on every route with a confusing reason.
        None => {
            tracing::warn!(%owner, %ns, "workspace-token.space.unknown");
            String::new()
        }
    };
    let secret = crate::k8s::user_key_secret(owner, ns, &private, &material, &authorized, &registry_token, &workspace_token);
    // A separate Secret, never folded into user-key (that one is mounted whole into every
    // workspace pod). Written only when there is something to write — no delete route exists
    // on this api, so an empty map leaves whatever is already there alone.
    if !s.bench_engine.is_empty() {
        let engine_secret = crate::k8s::bench_engine_secret(owner, ns, &s.bench_engine);
        if let Err(e) = api
            .patch(
                crate::k8s::BENCH_ENGINE_SECRET,
                &kube::api::PatchParams::apply("kloudlite-api").force(),
                &kube::api::Patch::Apply(&engine_secret),
            )
            .await
        {
            tracing::warn!(%owner, error = %e, "bench-engine.install.failed");
        }
    }
    for attempt in 1u32.. {
        let Err(e) = api
            .patch(
                crate::k8s::USER_KEY_SECRET,
                &kube::api::PatchParams::apply("kloudlite-api").force(),
                &kube::api::Patch::Apply(&secret),
            )
            .await
        else {
            return;
        };
        // Refused in a namespace being torn down, or one so new the controller has not bound
        // `api-secrets` in it yet: expected, and the next beat or claim writes it. Only a refusal
        // the namespace cannot explain is worth a warning.
        let code = match &e {
            kube::Error::Api(st) => st.code,
            _ => 0,
        };
        if matches!(code, 403 | 404) {
            let read = Api::<k8s_openapi::api::core::v1::Namespace>::all(c.clone()).get_opt(ns).await;
            match install_refusal(read.as_ref().map(Option::as_ref).ok(), k8s_openapi::jiff::Timestamp::now().as_second()) {
                // The RBAC cache trails the RoleBinding by seconds, not minutes: on 2026-09-16
                // 12:58 the namespace, its bindings and this apply all landed in the same second,
                // and deferring to the 300 s beat left a bench waiting 118 s for its key. A few
                // in-call retries close that gap; past them the beat is still the backstop.
                Refusal::Young => match retry_backoff(attempt) {
                    Some(d) => {
                        tracing::info!(%owner, namespace = %ns, code, attempt, "key.install.retried");
                        tokio::time::sleep(d).await;
                        continue;
                    }
                    None => {
                        tracing::info!(%owner, namespace = %ns, code, "key.install.deferred");
                        return;
                    }
                },
                // Terminating or gone: nothing a retry can change.
                Refusal::Settled => {
                    tracing::info!(%owner, namespace = %ns, code, "key.install.deferred");
                    return;
                }
                Refusal::Fault => {}
            }
        }
        tracing::warn!(%owner, error = %e, "key.install.failed");
        return;
    }
}

/// Why a 403/404 on the `user-key` apply happened, as far as the namespace explains it.
enum Refusal {
    /// Nothing about the namespace explains the refusal — a real fault, worth a warning.
    Fault,
    /// Terminating or gone: expected, and waiting changes nothing.
    Settled,
    /// Live but younger than one keys beat: its RoleBinding is still on the way, so a retry lands.
    Young,
}

/// 1 s, 3 s, 9 s — three retries, ~13 s in all, which covers the RBAC cache lag seen on
/// 2026-09-16 12:58 without holding a create open anywhere near the 300 s beat.
fn retry_backoff(attempt: u32) -> Option<std::time::Duration> {
    (attempt <= 3).then(|| std::time::Duration::from_secs(3u64.pow(attempt - 1)))
}

/// Whether a 403/404 on the `user-key` write is the namespace's lifecycle rather than a fault: gone,
/// terminating, or younger than one keys beat (its RoleBinding is still on the way). `read` is the
/// namespace GET: `None` when that read FAILED, which explains nothing and so still warns;
/// `Some(None)` is a 404, gone.
fn install_refusal(read: Option<Option<&k8s_openapi::api::core::v1::Namespace>>, now_secs: i64) -> Refusal {
    let Some(read) = read else { return Refusal::Fault };
    let Some(ns) = read else { return Refusal::Settled };
    if ns.metadata.deletion_timestamp.is_some() {
        return Refusal::Settled;
    }
    let young = ns
        .metadata
        .creation_timestamp
        .as_ref()
        .is_some_and(|t| now_secs - t.0.as_second() < crate::api::keys::KEYS_RESYNC_SECS as i64);
    if young { Refusal::Young } else { Refusal::Fault }
}

/// The workspace-token to project: the one the Secret already holds while it verifies for this
/// owner and space with more than half its day left, else a fresh one. Re-minting on every rewrite
/// had `/v1/workspaces/{id}/tools` hand out a token the pod's mounted file only sees after the
/// kubelet's Secret sync (up to a minute), and the tool server admits only that file: a bench
/// placement rewrote the Secret and the hourly 2026-09-23 23:01 IST run's tool round trip got 401
/// twice. Keeping it revokes nothing less: a rewrite never invalidated the old token anyway.
///
/// ponytail: the two still disagree for one kubelet sync after each twelve-hourly rotation; have
/// the tool server admit the previous token too if a caller ever hits that window.
fn keep_or_mint(jwt: &kloudlite_core::jwt::Jwt, current: Option<&str>, owner: &str, space: &str) -> kloudlite_core::Result<String> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let fresh = |c: &kloudlite_core::jwt::WorkspaceToolClaims| {
        c.sub == owner && c.space == space && c.exp > now + kloudlite_core::jwt::WORKSPACE_TOOL_TTL_SECS / 2
    };
    if let Some(tok) = current.filter(|t| jwt.verify_workspace_tool(t).is_ok_and(|c| fresh(&c))) {
        return Ok(tok.to_string());
    }
    Ok(jwt.mint_workspace_tool(owner, space)?.0)
}

#[cfg(test)]
mod tests {
    use super::{Refusal, install_refusal, keep_or_mint, retry_backoff};

    #[test]
    fn a_fresh_token_for_the_same_owner_and_space_is_kept() {
        let jwt = kloudlite_core::jwt::Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap();
        let (tok, _) = jwt.mint_workspace_tool("alice", "acme").unwrap();
        assert_eq!(keep_or_mint(&jwt, Some(&tok), "alice", "acme").unwrap(), tok, "kept: the pod's file still matches");
        assert_ne!(keep_or_mint(&jwt, Some(&tok), "alice", "other").unwrap(), tok, "another space mints");
        assert_ne!(keep_or_mint(&jwt, Some(&tok), "bob", "acme").unwrap(), tok, "another owner mints");
        let minted = keep_or_mint(&jwt, Some("garbage"), "alice", "acme").unwrap();
        assert!(jwt.verify_workspace_tool(&minted).is_ok(), "an unreadable token is replaced");
        assert!(keep_or_mint(&jwt, None, "alice", "acme").is_ok());
    }
    use k8s_openapi::api::core::v1::Namespace;

    fn ns(age: i64, now: i64, terminating: bool) -> Namespace {
        let mut v = serde_json::json!({"metadata": {"name": "wt-a-1",
            "creationTimestamp": k8s_openapi::jiff::Timestamp::from_second(now - age).unwrap().to_string()}});
        if terminating {
            v["metadata"]["deletionTimestamp"] = v["metadata"]["creationTimestamp"].clone();
        }
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn only_a_settled_live_namespace_makes_a_refused_install_a_fault() {
        let now = 2_000_000_000;
        assert!(matches!(install_refusal(None, now), Refusal::Fault), "a failed namespace read explains nothing");
        assert!(matches!(install_refusal(Some(None), now), Refusal::Settled), "gone");
        assert!(matches!(install_refusal(Some(Some(&ns(3600, now, true))), now), Refusal::Settled), "terminating");
        assert!(matches!(install_refusal(Some(Some(&ns(10, now, false))), now), Refusal::Young), "binding still on the way");
        assert!(matches!(install_refusal(Some(Some(&ns(3600, now, false))), now), Refusal::Fault), "an old live namespace refusing is real");
    }

    /// Three retries, ~13 s in all — a young namespace is waited on, never waited out.
    #[test]
    fn the_retry_schedule_is_one_three_nine_and_then_the_beat() {
        let secs: Vec<_> = (1..=4).map(|a| retry_backoff(a).map(|d| d.as_secs())).collect();
        assert_eq!(secs, vec![Some(1), Some(3), Some(9), None]);
    }
}
