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
    // First, before the superadmin arm: a scope is a ceiling nothing else may lift.
    if !in_scope(c, owner) {
        return false;
    }
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
    in_scope(caller, owner) && (caller.name == owner || teams_for(s, &caller.name).await.iter().any(|t| t == owner))
}

/// The refusal after a `may_act_on`/`may_allocate_for` false: a paused member is told so (they
/// know the team; "not found" would read as a removal), everyone else gets `otherwise` unchanged.
/// An unreadable directory is `otherwise` too — refused either way, never granted.
pub(crate) async fn denial(s: &ApiState, c: &Caller, owner: &str, otherwise: Response) -> Response {
    // Through the same 30 s cache `team_access` uses: a caller hammering a refused owner used to be
    // one uncached directory call per refusal.
    match verdict(s, owner, &c.name).await {
        Some(super::Judged::Member(super::MemberState::Paused)) => {
            (StatusCode::FORBIDDEN, format!("your access to {owner} is paused")).into_response()
        }
        _ => otherwise,
    }
}

/// How long one `(team, handle)` verdict is reused.
const VERDICT_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// The cached strict membership answer. `None` means no directory, or one that could not be read —
/// never a grant; each caller keeps its own keep-or-refuse bias.
///
/// ponytail: 30 s per-state cache, invalidated only by `reconcile_pair` on the replica that wrote a
/// pause — elsewhere a pause or removal takes effect within TTL + one beat; move to a
/// directory-pushed invalidation if that window ever matters.
pub(crate) async fn verdict(s: &ApiState, team: &str, user: &str) -> Option<super::Judged> {
    let d = s.directory.as_ref()?;
    let key = (super::membership::norm(team), super::membership::norm(user));
    let cache = &s.member_verdicts;
    let hit = cache.lock().unwrap_or_else(|p| p.into_inner()).get(&key).filter(|(at, _)| at.elapsed() < VERDICT_TTL).map(|(_, j)| *j);
    if let Some(j) = hit {
        return Some(j);
    }
    match d.membership(&key.0, user).await {
        Ok(j) => {
            let mut g = cache.lock().unwrap_or_else(|p| p.into_inner());
            // Swept here and nowhere else: without it an expired entry lives for the process's
            // life, one per distinct (team, handle) this node ever asked about.
            g.retain(|_, (at, _)| at.elapsed() < VERDICT_TTL);
            g.insert(key, (std::time::Instant::now(), j));
            Some(j)
        }
        Err(e) => {
            tracing::warn!(owner = %user, %team, error = %e, "scope.membership.unavailable");
            None
        }
    }
}

/// A caller's OWN object in a team (`owner` is theirs, `team` a real team): refused only on a
/// KNOWN non-active verdict — paused, or (when `owns` says the caller has an object there, so a
/// stranger learns nothing) no longer a member. A directory error serves the object as before:
/// it is the caller's own, and the membership beat stops or pauses the pair within one beat, so a
/// directory blip must not lock every active owner out of their own workspace.
pub(crate) async fn team_access(s: &ApiState, c: &Caller, owner: &str, team: &str, owns: bool) -> Result<(), (StatusCode, String)> {
    use super::{Judged, MemberState};
    if !super::membership::team_pair(owner, team) {
        return Ok(());
    }
    let team = super::membership::norm(team);
    // An unreadable directory serves the caller's own object as before (see above).
    let Some(judged) = verdict(s, &team, &c.name).await else { return Ok(()) };
    match judged {
        Judged::Member(MemberState::Paused) => Err((StatusCode::FORBIDDEN, format!("your access to {team} is paused"))),
        Judged::NotMember | Judged::TeamGone if owns => Err((StatusCode::FORBIDDEN, format!("you are no longer a member of {team}"))),
        _ => Ok(()),
    }
}

/// A bench-tool caller acts only for its own handle and the bench's team, even for another team
/// the person really belongs to: the token lives in a pod, and a leak must not reach every team.
pub(crate) fn in_scope(c: &Caller, owner: &str) -> bool {
    // Slugs are lowercase at write; a query's casing must not move a caller out of (or into) scope.
    c.scope.as_deref().is_none_or(|team| owner.eq_ignore_ascii_case(&c.name) || owner.eq_ignore_ascii_case(team))
}

/// The one sentence a listing answers when `?owner=`/`?team=` leaves a scoped caller's scope.
pub(crate) fn scope_refusal(c: &Caller) -> Response {
    let team = c.scope.as_deref().unwrap_or_default();
    if c.scope.is_some() {
        tracing::info!(owner = %c.name, jti8 = c.jti8.as_deref().unwrap_or_default(), reason = "scope", "bench.tool.refused");
    }
    (StatusCode::FORBIDDEN, format!("bench tools act only for {} and {team}", c.name)).into_response()
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

/// A name is unique per (owner, team): source lives at `~/workspace` inside the workspace's own
/// volume, one volume per workspace, so the name only has to be unique for listing and lookup, not
/// for a shared mount path. The selector narrows the list; the decision
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

/// Workspaces are strictly personal — no team ownership — so ownership is a field comparison, and
/// someone else's workspace is a 404, never a 403.
/// Workspaces are strictly personal — no team ownership — but a platform administrator may still
/// act on any owner's, the claim's whole point.
pub(crate) async fn my_ws(s: &ApiState, c: &Caller, id: &str) -> Result<crd::Workspace, Response> {
    let api: Api<crd::Workspace> = Api::all(kube(s)?.clone());
    let w = super::admin::timing::step("kube.get.workspace", api.get_opt(id)).await.map_err(kube_err)?.ok_or_else(not_found)?;
    // Through `may_act_on`, not a hand-rolled `c.superadmin` arm: that arm reached another
    // owner's workspace without leaving the `superadmin.acting` line the claim's whole design
    // rests on, so support's cross-owner reads were the only unlogged ones (2026-09-12).
    if !super::admin::timing::step("directory.may_act_on", may_act_on(s, c, &w.spec.owner)).await {
        return Err(denial(s, c, &w.spec.owner, not_found()).await);
    }
    // `owner == caller` admits a person to their own TEAM workspace too, so a paused or removed
    // membership must be checked here, or start/push/tools stay open until the beat re-stops it.
    // Every workspace verb resolves through this function; the claim path is untouched.
    if w.spec.owner == c.name && !c.superadmin {
        team_access(s, c, &w.spec.owner, &w.spec.team, true).await.map_err(IntoResponse::into_response)?;
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
        Some(_) => Err(denial(s, caller, &owner, (StatusCode::FORBIDDEN, "not a member of that team").into_response()).await),
    }
}

/// Finds an environment by id and authorizes the caller against its owner: their own always
/// passes, a team's passes when they are a member, and a platform administrator's claim passes
/// for anyone. An environment they may not act on is a 404, never a 403 — the caller learns
/// nothing about environments that are not theirs.
pub(crate) async fn find_env(s: &ApiState, caller: &Caller, id: &str) -> Result<crd::Environment, Response> {
    let api: Api<crd::Environment> = Api::all(kube(s)?.clone());
    let e = api.get_opt(id).await.map_err(kube_err)?.ok_or_else(not_found)?;
    // The ONE visibility guard, in the lookup every environment route shares — get, start, stop,
    // delete, clone, push, restore-in-place, the intercepts and `attach_ws` all come through here,
    // so a route added later inherits it rather than having to remember it. A `system`
    // environment (the hidden per-owner builder) is indistinguishable from an id that does not
    // exist; the build gate reaches it through `/v1/internal/builders`, which never calls this.
    if !super::environments::visible_env(&e) {
        return Err(not_found());
    }
    if !may_act_on(s, caller, &e.spec.owner).await {
        return Err(denial(s, caller, &e.spec.owner, not_found()).await);
    }
    Ok(e)
}

/// Every owner label the caller may read volumes under: themselves, plus each team they belong to
/// (team-owned environments). Membership is verified HERE — the server tier trusts whatever owner
/// this tier names in `OWNER_HEADER`, so an unverified value would be a data leak.
pub(crate) async fn caller_owners(s: &ApiState, caller: &Caller) -> Vec<String> {
    let mut v = vec![caller.name.clone()];
    v.extend(teams_for(s, &caller.name).await.into_iter().filter(|t| in_scope(caller, t)));
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
    use super::*;
    use crate::api::{Directory, OwnerMaterial, TeamRole};
    use std::sync::Arc;

    struct Member;
    #[async_trait::async_trait]
    impl Directory for Member {
        async fn teams_for(&self, _u: &str) -> Vec<String> {
            vec!["t1".into(), "t2".into()]
        }
        async fn is_live(&self, _j: &str) -> bool {
            true
        }
        async fn for_owner(&self, _o: &str) -> Option<OwnerMaterial> {
            None
        }
        async fn authorized_keys_for_owner(&self, _o: &str) -> Option<String> {
            None
        }
        async fn owners_of(&self, _e: &str) -> Vec<String> {
            Vec::new()
        }
        async fn team_role(&self, _u: &str, _t: &str) -> Option<TeamRole> {
            None
        }
        async fn is_team(&self, _s: &str) -> bool {
            true
        }
        async fn ensure_user(&self, _e: &str, _n: &str, _u: &str) -> Result<(), String> {
            Err("no".into())
        }
        async fn add_superadmin(&self, _e: &str, _b: &str) -> Result<(), String> {
            Err("no".into())
        }
    }

    fn state() -> ApiState {
        let jwt = Arc::new(kloudlite_core::jwt::Jwt::new("test-secret-that-is-at-least-32-bytes-long").unwrap());
        let mut s = ApiState::new(jwt);
        s.directory = Some(Arc::new(Member));
        s
    }

    fn scoped(team: &str, superadmin: bool) -> Caller {
        Caller { name: "meera".into(), superadmin, parent: None, scope: Some(team.into()), jti8: Some("abcd1234".into()) }
    }

    #[test]
    fn a_scope_refusal_logs_the_bench_tool_refusal() {
        #[derive(Clone, Default)]
        struct Buf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf::default();
        let w = buf.clone();
        let sub = tracing_subscriber::fmt().with_ansi(false).with_writer(move || w.clone()).finish();
        {
            let _g = tracing::subscriber::set_default(sub);
            assert_eq!(scope_refusal(&scoped("t1", false)).status(), StatusCode::FORBIDDEN);
        }
        let logs = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("bench.tool.refused") && logs.contains("scope") && logs.contains("abcd1234"), "{logs}");
        assert!(logs.contains("meera") && !logs.contains("t1"), "owner and jti8 only: {logs}");
    }

    #[test]
    fn in_scope_is_own_handle_and_team_only() {
        let c = scoped("t1", false);
        assert!(in_scope(&c, "meera") && in_scope(&c, "t1") && in_scope(&c, "T1"));
        assert!(!in_scope(&c, "t2") && !in_scope(&c, "bob"));
        let open = Caller { scope: None, ..c };
        assert!(in_scope(&open, "t2") && in_scope(&open, "bob"));
    }

    #[tokio::test]
    async fn a_scoped_caller_is_refused_another_team_it_belongs_to() {
        let s = state();
        let c = scoped("t1", false);
        assert!(may_allocate_for(&s, &c, "t1").await && may_act_on(&s, &c, "t1").await);
        assert!(!may_allocate_for(&s, &c, "t2").await);
        assert!(!may_act_on(&s, &c, "t2").await);
        assert_eq!(caller_owners(&s, &c).await, vec!["meera".to_string(), "t1".to_string()]);
    }

    #[tokio::test]
    async fn a_superadmin_flag_never_widens_a_scoped_caller() {
        let s = state();
        let c = scoped("t1", true);
        assert!(!may_act_on(&s, &c, "t2").await);
        assert!(!may_act_on(&s, &c, "someone-else").await);
        assert!(!may_allocate_for(&s, &c, "t2").await);
    }

    struct Paused;
    #[async_trait::async_trait]
    impl Directory for Paused {
        async fn teams_for(&self, _u: &str) -> Vec<String> {
            Vec::new()
        }
        async fn is_live(&self, _j: &str) -> bool {
            true
        }
        async fn for_owner(&self, _o: &str) -> Option<OwnerMaterial> {
            None
        }
        async fn authorized_keys_for_owner(&self, _o: &str) -> Option<String> {
            None
        }
        async fn owners_of(&self, _e: &str) -> Vec<String> {
            Vec::new()
        }
        async fn team_role(&self, _u: &str, _t: &str) -> Option<TeamRole> {
            None
        }
        async fn is_team(&self, _s: &str) -> bool {
            true
        }
        async fn ensure_user(&self, _e: &str, _n: &str, _u: &str) -> Result<(), String> {
            Err("no".into())
        }
        async fn add_superadmin(&self, _e: &str, _b: &str) -> Result<(), String> {
            Err("no".into())
        }
        async fn membership(&self, team: &str, user: &str) -> Result<crate::api::Judged, String> {
            use crate::api::{Judged, MemberState};
            match (team, user) {
                ("down", _) => Err("unreachable".into()),
                ("gone", _) => Ok(Judged::TeamGone),
                (_, "paula") => Ok(Judged::Member(MemberState::Paused)),
                (_, "rex") => Ok(Judged::NotMember),
                _ => Ok(Judged::Member(MemberState::Active)),
            }
        }
    }

    async fn ws_status(owner: &str, team: &str, superadmin: bool) -> u16 {
        let spec = serde_json::json!({"owner": owner, "team": team, "name": "w1", "region": "r", "image": "i", "desiredState": "running"});
        let body = serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace", "metadata": {"name": "w1"}, "spec": spec});
        let (client, _) = crate::kube_test::mock_client(vec![crate::kube_test::get("/apis/kloudlite.io/v1alpha1/workspaces/w1", body)]);
        let jwt = Arc::new(kloudlite_core::jwt::Jwt::new("test-secret-that-is-at-least-32-bytes-long").unwrap());
        let s = ApiState::new(jwt).with_kube(client).with_directory(Arc::new(Paused));
        let c = Caller { name: owner.into(), superadmin, parent: None, scope: None, jti8: None };
        match my_ws(&s, &c, "w1").await {
            Ok(_) => 200,
            Err(r) => r.status().as_u16(),
        }
    }

    #[tokio::test]
    async fn a_paused_member_is_refused_their_own_team_workspace() {
        assert_eq!(ws_status("paula", "acme", false).await, 403);
        assert_eq!(ws_status("paula", "", false).await, 200, "a personal workspace is unaffected");
        assert_eq!(ws_status("paula", "paula", false).await, 200, "team == owner is personal");
        assert_eq!(ws_status("alice", "acme", false).await, 200, "an active member is unaffected");
        assert_eq!(ws_status("alice", "down", false).await, 200, "an unreadable directory serves the owner's own");
    }

    #[tokio::test]
    async fn a_removed_member_is_refused_their_own_team_workspace() {
        assert_eq!(ws_status("rex", "acme", false).await, 403);
        assert_eq!(ws_status("rex", "gone", false).await, 403, "a deleted team too");
        assert_eq!(ws_status("rex", "", false).await, 200, "a personal workspace is unaffected");
    }
}
