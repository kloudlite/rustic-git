//! Who the caller is, and what they may act on.
//!
//! The load-bearing part of this module is `mine`: a label selector is an INDEX and `spec.owner` is
//! the answer, and three handlers used to get that right while four got it wrong. One function
//! means the rule cannot be half-remembered. `snapshots_on_volume` in `volumes.rs` is the
//! deliberate exception and says so — a decision that destroys data counts everyone's rows.

use super::{kube, kube_err, not_found, ApiState, Caller};
use crate::crd;
use crate::k8s::{OWNER_LABEL, TEAM_LABEL};
use kube::api::{Api, ListParams};
use axum::{http::StatusCode, response::{IntoResponse, Response}};
use std::collections::HashSet;

pub(crate) async fn teams_for(s: &ApiState, caller: &str) -> Vec<String> {
    match &s.directory {
        Some(m) => m.teams_for(caller).await,
        None => Vec::new(),
    }
}

/// Whether `slug` is a team, straight from the directory — no directory means no teams exist to
/// this node, so `false` matches every other unwired-directory answer here.
pub(crate) async fn is_team(s: &ApiState, slug: &str) -> bool {
    match &s.directory {
        Some(m) => m.is_team(slug).await,
        None => false,
    }
}

/// `owner` is the object's actual owner field (a username or a team slug). Their own always
/// passes; a team's passes for a member; and a platform administrator passes for anyone — so
/// support can clean up without impersonating the person.
pub(crate) async fn may_act_on(s: &ApiState, c: &Caller, owner: &str) -> bool {
    if c.name == owner {
        return true;
    }
    if teams_for(s, &c.name).await.iter().any(|t| t == owner) {
        return true;
    }
    if c.superadmin {
        // Every cross-owner access a claim allows is recorded with the caller: the point of the
        // claim is that support never has to impersonate, and an un-logged one would be worse than
        // impersonation, not better.
        tracing::info!(caller = %c.name, %owner, "superadmin.acting");
        return true;
    }
    false
}

/// Whether `caller` may spend `owner`'s quota: themself, or real directory membership — NEVER a
/// superadmin claim. Every allocating `/v1` path (create/clone/restore/push) decides its new
/// object's owner through this, not `may_act_on`: a superadmin's cross-owner power is list/stop/
/// delete/get only (CLAUDE.md), and `may_act_on`'s superadmin arm let a claim spend an arbitrary
/// owner's — even a non-team slug's — quota.
pub(crate) async fn may_allocate_for(s: &ApiState, caller: &Caller, owner: &str) -> bool {
    caller.name == owner || teams_for(s, &caller.name).await.iter().any(|t| t == owner)
}

/// A label selector is the list filter, not a field selector: `metadata.labels` is indexed for
/// selectors by every API server, while an arbitrary spec field needs a `selectableFields` entry —
/// and adding one per query axis is how a CRD becomes a database.
pub(crate) fn owned_by(owner: &str) -> ListParams {
    ListParams::default().labels(&format!("{OWNER_LABEL}={owner}"))
}

/// One person's workspaces in one team (empty = personal). Both labels, so a team page never
/// shows the personal ones and the personal page never shows a team's.
pub(crate) fn owned_in(owner: &str, team: &str) -> ListParams {
    ListParams::default().labels(&format!("{OWNER_LABEL}={owner},{TEAM_LABEL}={team}"))
}

/// The `spec.owner` of anything this API lists. One trait so "narrow by label, DECIDE on spec" is
/// a single function instead of a rule seven handlers each remembered or forgot.
pub trait Owned {
    fn owner(&self) -> &str;
}

impl Owned for crd::Workspace {
    fn owner(&self) -> &str {
        &self.spec.owner
    }
}

impl Owned for crd::Environment {
    fn owner(&self) -> &str {
        &self.spec.owner
    }
}

impl Owned for crd::Snapshot {
    fn owner(&self) -> &str {
        &self.spec.owner
    }
}

/// Keep only what `owners` actually owns. The label selector stays as the INDEX; this is the
/// answer. An object whose label disagrees with its spec — a restored backup, a migration, an
/// operator with kubectl, the window before the controller re-stamps — is somebody else's.
pub fn mine<K: Owned>(items: Vec<K>, owners: &[String]) -> Vec<K> {
    items.into_iter().filter(|k| owners.iter().any(|o| o == k.owner())).collect()
}

/// A name is unique per (owner, team): it is also the directory the workspace mounts at inside
/// the person's shared home (`~/workspaces/<name>`), and two workspaces on one path would be two
/// workspaces one editor session cannot tell apart. The selector narrows the list; the decision
/// reads `spec` (labels are a view). ponytail: a Workspace written by another path without its
/// labels is invisible here until the controller re-stamps them — a window of one reconcile.
pub(crate) async fn refuse_taken_name(c: &kube::Client, owner: &str, team: &str, name: &str) -> Result<(), Response> {
    let api: Api<crd::Workspace> = Api::all(c.clone());
    let list = api.list(&owned_in(owner, team)).await.map_err(kube_err)?;
    if list.items.iter().any(|w| w.spec.owner == owner && w.spec.team == team && w.spec.name == name) {
        return Err((StatusCode::CONFLICT, format!("a workspace named {name:?} already exists here")).into_response());
    }
    Ok(())
}

/// Every namespace name the platform would derive for this owner: their personal one, plus one
/// per team they are in.
///
/// The label is a VIEW and never authority (CLAUDE.md) — the NAME is what says whose namespace
/// this is, so it is checked by RECOMPUTING it rather than by picking the owner back out of the
/// string. `crd::ws_namespace` hashes any name over 63 characters into a DNS label, which no
/// prefix/suffix test can invert: the earlier `ends_with("-{owner}")` heuristic skipped exactly
/// those, so an ssh key add never reached a workspace in a long-named team.
pub(crate) async fn owners_namespaces(s: &ApiState, owner: &str) -> HashSet<String> {
    let mut out = HashSet::from([crd::ws_namespace(owner, "")]);
    out.extend(teams_for(s, owner).await.iter().map(|t| crd::ws_namespace(owner, t)));
    out
}

/// Workspaces are strictly personal — no team ownership — so ownership is a field comparison, and
/// someone else's workspace is a 404, never a 403.
/// Workspaces are strictly personal — no team ownership — but a platform administrator may still
/// act on any owner's, the claim's whole point.
pub(crate) async fn my_ws(s: &ApiState, c: &Caller, id: &str) -> Result<crd::Workspace, Response> {
    let api: Api<crd::Workspace> = Api::all(kube(s)?.clone());
    let w = api.get_opt(id).await.map_err(kube_err)?.ok_or_else(not_found)?;
    if w.spec.owner != c.name && !c.superadmin {
        return Err(not_found());
    }
    Ok(w)
}

/// Resolve `NewEnvironment.owner` against the caller: personal (`None` or `caller`) always
/// passes; a different owner must be a team the caller belongs to, which needs a directory —
/// 503 rather than silently creating an environment nobody but this caller can ever see again.
/// `may_allocate_for`, not `may_act_on`: this NAMES the owner of a new allocation, and a
/// superadmin claim must never let a caller spend a team's quota without being a member.
pub(crate) async fn resolve_new_owner(s: &ApiState, caller: &Caller, owner: Option<String>) -> Result<String, Response> {
    let Some(owner) = owner else { return Ok(caller.name.clone()) };
    if owner == caller.name {
        return Ok(owner);
    }
    match &s.directory {
        None => Err((StatusCode::SERVICE_UNAVAILABLE, "team lookup not configured on this node").into_response()),
        Some(_) if may_allocate_for(s, caller, &owner).await => Ok(owner),
        Some(_) => Err((StatusCode::FORBIDDEN, "not a member of that team").into_response()),
    }
}

/// Finds an environment by id and authorizes the caller against its owner: their own always
/// passes, a team's passes when they are a member, and a platform administrator's claim passes
/// for anyone. An environment they may not act on is a 404, never a 403 — the caller learns
/// nothing about environments that are not theirs.
pub(crate) async fn find_env(s: &ApiState, caller: &Caller, id: &str) -> Result<crd::Environment, Response> {
    let api: Api<crd::Environment> = Api::all(kube(s)?.clone());
    let e = api.get_opt(id).await.map_err(kube_err)?.ok_or_else(not_found)?;
    if !may_act_on(s, caller, &e.spec.owner).await {
        return Err(not_found());
    }
    Ok(e)
}

/// Every owner label the caller may read volumes under: themselves, plus each team they belong to
/// (team-owned environments). Membership is verified HERE — the server tier trusts whatever owner
/// this tier names in `OWNER_HEADER`, so an unverified value would be a data leak.
pub(crate) async fn caller_owners(s: &ApiState, caller: &Caller) -> Vec<String> {
    let mut v = vec![caller.name.clone()];
    v.extend(teams_for(s, &caller.name).await);
    v
}

/// `OWNER_LABEL in (…)`, built only from slugs that are single validated segments.
///
/// `in (a,b)` is comma-delimited and paren-terminated, so one slug carrying `,` or `)` widens or
/// breaks the set — on a listing that decides whether a row says "source deleted". Slugs are
/// directory-validated today; every other selector in this file takes a single validated value,
/// and this one now does too.
pub fn owner_set_selector(owners: &[String]) -> String {
    // `owners` is always `caller_owners`'s output, and that always starts with the caller's own
    // (already-validated) owner — so this never filters down to an empty set.
    let safe: Vec<&str> =
        owners.iter().filter(|o| kloudlite_storage::store::valid_owner(o)).map(String::as_str).collect();
    format!("{OWNER_LABEL} in ({})", safe.join(","))
}

#[cfg(test)]
mod tests {
    /// A team namespace is `wt-{owner}-{hash}` (and a long personal one is DNS-hashed), so it is
    /// exactly the case the old `ends_with("-{owner}")` heuristic dropped — and dropping it meant
    /// an ssh key add never reached that team's workspaces.
    #[tokio::test]
    async fn a_dns_truncated_team_namespace_is_still_the_owners() {
        use super::{owners_namespaces, ApiState};
        use crate::api::Directory;
        use crate::crd;
        use std::sync::Arc;

        let long = "a".repeat(60);
        struct Stub(String);
        #[async_trait::async_trait]
        impl Directory for Stub {
            async fn teams_for(&self, _user: &str) -> Vec<String> {
                vec![self.0.clone()]
            }

            // This stub exercises namespace-to-owner matching only; CLI tokens and ssh keys are
            // not part of its case, and an unwired revocation list must refuse rather than admit.
            async fn is_live(&self, _jti: &str) -> bool {
                false
            }

            // No keys in this case: `None` is "the lookup failed", which is what an unwired
            // directory is.
            async fn for_owner(&self, _owner: &str) -> Option<crate::api::OwnerMaterial> {
                None
            }

            // Not exercised here — this test is about namespace hashing, not rank.
            async fn team_role(&self, _user: &str, _team: &str) -> Option<crate::api::TeamRole> {
                None
            }

            // Not exercised here either.
            async fn is_team(&self, _slug: &str) -> bool {
                false
            }

            async fn ensure_user(&self, _e: &str, _n: &str, _u: &str) -> Result<(), String> {
                Err("no directory".into())
            }
        }
        let state = ApiState::new(
            Arc::new(kloudlite_core::jwt::Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap()),
        )
        .with_directory(Arc::new(Stub(long.clone())));

        let ns = crd::ws_namespace("karthik", &long);
        assert!(ns.len() <= 63 && !ns.ends_with("-karthik"), "this team must be hashed: {ns}");
        let mine = owners_namespaces(&state, "karthik").await;
        assert!(mine.contains(&ns), "{ns} must be recognised as karthik's");
        assert!(mine.contains(&crd::ws_namespace("karthik", "")));
        assert!(!mine.contains(&crd::ws_namespace("someone-else", "")));
    }
}
