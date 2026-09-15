//! Pausing a team member: `POST /v1/teams/{slug}/members/{email}/pause` and `/unpause`.
//!
//! A pause keeps the membership row and its role and changes nothing else — no CR, no setting —
//! it only stops the person acting under the team until an admin unpauses them. The reach is
//! `remove_member`'s (`may_grant` on the target's role), a superadmin reaches any team, a paused
//! admin reaches nobody, and nobody pauses or unpauses themself: a paused person reinstating
//! themself would make the pause meaningless — a superadmin included.
//!
//! Reach: `/v1` and the team routes refuse at once, but `App::may_act` (git over ssh and http, the
//! registry) caches membership for `MEMBERSHIP_TTL`, so a pause reaches those up to 60 s late —
//! the same lag as a removal.

use super::*;
use kloudlite_pulls::directory::MemberState;

pub(crate) async fn pause_member(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path((slug, email)): axum::extract::Path<(String, String)>,
) -> Response {
    set_state(&api, &headers, &slug, &email, MemberState::Paused).await
}

pub(crate) async fn unpause_member(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path((slug, email)): axum::extract::Path<(String, String)>,
) -> Response {
    set_state(&api, &headers, &slug, &email, MemberState::Active).await
}

async fn set_state(api: &Arc<Api>, headers: &axum::http::HeaderMap, slug: &str, email: &str, state: MemberState) -> Response {
    let user = match caller(api, headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let db = match directory(api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let team = match db.get(slug).await {
        Ok(Some(t)) => t,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such team").into_response(),
        Err(e) => return db_err("read team", slug, e),
    };
    // The directory row, not the 12 h token claim: a revoked superadmin must not keep this reach.
    let superadmin = match db.is_superadmin(&user).await {
        Ok(a) => a,
        Err(e) => return db_err("check admin", slug, e),
    };
    // Same 404 as `team_for` for a non-member, BEFORE the self-check: a stranger pausing themself
    // on a real slug must not learn it exists from a 403.
    if !superadmin && kloudlite_pulls::directory::Directory::role_of(&team, &user).is_none() {
        return (StatusCode::NOT_FOUND, "no such team").into_response();
    }
    if user.eq_ignore_ascii_case(email.trim()) {
        let verb = if state == MemberState::Paused { "pause" } else { "unpause" };
        return (StatusCode::FORBIDDEN, format!("you cannot {verb} yourself")).into_response();
    }
    let active = |who: &str| team.members.iter().find(|m| m.user.eq_ignore_ascii_case(who)).filter(|m| m.state == MemberState::Active);
    if !superadmin {
        let target = kloudlite_pulls::directory::Directory::role_of(&team, email);
        let reach = active(&user).is_some_and(|me| target.is_some_and(|t| may_grant(me.role, t)));
        if !reach {
            return (StatusCode::FORBIDDEN, "your role does not allow that").into_response();
        }
    }
    // A no-op is not an admin write: no audit row, no keys refresh.
    if team.members.iter().any(|m| m.user.eq_ignore_ascii_case(email.trim()) && m.state == state) {
        return StatusCode::NO_CONTENT.into_response();
    }
    let (action, event) = match state {
        MemberState::Paused => ("pause-member", "member.paused"),
        MemberState::Active => ("unpause-member", "member.unpaused"),
    };
    match db.set_member_state(slug, email, state, &user).await {
        Ok(Membership::Done) => {
            let email = email.trim().to_lowercase();
            api.membership.forget(&email, slug);
            crate::credentials::spawn_keys_changed(api, &email);
            // Awaited, so the answer means the Bench already reads paused (or full) again: an
            // unpaused person's start is otherwise parked on `access: paused` for up to a beat.
            if let Some(hook) = api.on_member_state.clone() {
                match db.user(&email).await {
                    Ok(Some(u)) => {
                        if let Some(handle) = u.username {
                            hook(handle, slug.to_string()).await
                        }
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!(team = %slug, member = %email, error = %e, "member.reconcile.skipped"),
                }
            }
            tracing::info!(team = %slug, member = %email, by = %user, "{event}");
            // After the write: a refused pause (last owner, not a member) is not an admin write.
            if let Err(r) = write_audit(api, &user, action, &format!("{slug}/{email}"), String::new(), "ok").await {
                return r;
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(Membership::NotAMember) => (StatusCode::NOT_FOUND, "not a member").into_response(),
        Ok(Membership::LastOwner) => (StatusCode::CONFLICT, "a team must keep at least one active owner").into_response(),
        Ok(Membership::NoSuchTeam) => (StatusCode::NOT_FOUND, "no such team").into_response(),
        Err(e) => db_err("change member state", slug, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kloudlite_pulls::directory::Directory;

    /// acme: owner o@x, admin a@x, members m@x and n@x.
    async fn fixture() -> Arc<Api> {
        let db = Directory::in_memory();
        for u in ["o@x", "a@x", "m@x", "n@x", "root@x"] {
            db.upsert_user(u, u).await.unwrap();
        }
        db.create("acme", "Acme", "o@x", "").await.unwrap().unwrap();
        db.add_member("acme", "a@x", Role::Admin).await.unwrap();
        db.add_member("acme", "m@x", Role::Member).await.unwrap();
        db.add_member("acme", "n@x", Role::Member).await.unwrap();
        db.add_superadmin("root@x", "boot").await.unwrap();
        let mut api = crate::testing::test_api_with_secret("peer").await;
        api.jwt = Some(Arc::new(kloudlite_core::jwt::Jwt::new("0123456789012345678901234567890123456789").unwrap()));
        api.directory = Some(Arc::new(db));
        Arc::new(api)
    }

    fn as_user(api: &Api, who: &str) -> axum::http::HeaderMap {
        let tok = api.jwt.as_ref().unwrap().mint(who, who, None).unwrap();
        let mut h = axum::http::HeaderMap::new();
        h.insert(axum::http::header::AUTHORIZATION, format!("Bearer {tok}").parse().unwrap());
        h
    }

    async fn pause(api: &Arc<Api>, by: &str, who: &str) -> (StatusCode, String) {
        let p = axum::extract::Path(("acme".to_string(), who.to_string()));
        body(pause_member(State(api.clone()), as_user(api, by), p).await).await
    }

    async fn unpause(api: &Arc<Api>, by: &str, who: &str) -> (StatusCode, String) {
        let p = axum::extract::Path(("acme".to_string(), who.to_string()));
        body(unpause_member(State(api.clone()), as_user(api, by), p).await).await
    }

    async fn body(r: Response) -> (StatusCode, String) {
        let s = r.status();
        let b = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
        (s, String::from_utf8_lossy(&b).into_owned())
    }

    fn state(api: &Api, t: &kloudlite_pulls::directory::Team, who: &str) -> MemberState {
        let _ = api;
        t.members.iter().find(|m| m.user == who).unwrap().state
    }

    async fn team(api: &Api) -> kloudlite_pulls::directory::Team {
        api.directory.as_ref().unwrap().get("acme").await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn an_admin_pauses_a_member_and_an_owner_pauses_an_admin() {
        let api = fixture().await;
        assert_eq!(pause(&api, "a@x", "m@x").await.0, StatusCode::NO_CONTENT);
        assert_eq!(pause(&api, "o@x", "a@x").await.0, StatusCode::NO_CONTENT);
        let t = team(&api).await;
        assert_eq!((state(&api, &t, "m@x"), state(&api, &t, "a@x")), (MemberState::Paused, MemberState::Paused));
        // A superadmin reaches a team they are not in; and the last active owner stays.
        assert_eq!(pause(&api, "root@x", "n@x").await.0, StatusCode::NO_CONTENT);
        assert_eq!(pause(&api, "root@x", "o@x").await, (StatusCode::CONFLICT, "a team must keep at least one active owner".into()));
        let audit = api.store.os.list(Some(&"audit".into()));
        use futures::StreamExt;
        assert_eq!(audit.collect::<Vec<_>>().await.len(), 3, "one audit row per successful write only");
    }

    #[tokio::test]
    async fn a_member_cannot_pause_and_nobody_pauses_themself() {
        let api = fixture().await;
        assert_eq!(pause(&api, "m@x", "n@x").await.0, StatusCode::FORBIDDEN);
        assert_eq!(pause(&api, "a@x", "o@x").await.0, StatusCode::FORBIDDEN, "an admin does not reach an owner");
        assert_eq!(pause(&api, "o@x", "o@x").await, (StatusCode::FORBIDDEN, "you cannot pause yourself".into()));
        assert_eq!(pause(&api, "a@x", "A@x").await, (StatusCode::FORBIDDEN, "you cannot pause yourself".into()));
        assert_eq!(pause(&api, "stranger@x", "stranger@x").await.0, StatusCode::NOT_FOUND, "a non-member cannot probe slugs");
        assert_eq!(pause(&api, "root@x", "m@x").await.0, StatusCode::NO_CONTENT);
        assert_eq!(pause(&api, "o@x", "a@x").await.0, StatusCode::NO_CONTENT);
        assert_eq!(pause(&api, "a@x", "n@x").await.0, StatusCode::FORBIDDEN, "a paused admin reaches nobody");
        assert_eq!(unpause(&api, "a@x", "a@x").await.0, StatusCode::FORBIDDEN);
        let p = axum::extract::Path(("acme".to_string(), "m@x".to_string()));
        assert_eq!(pause_member(State(api.clone()), as_user(&api, "stranger@x"), p).await.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_superadmin_member_cannot_pause_themself_and_a_repause_writes_nothing() {
        let api = fixture().await;
        api.directory.as_ref().unwrap().add_member("acme", "root@x", Role::Member).await.unwrap();
        assert_eq!(pause(&api, "root@x", "root@x").await, (StatusCode::FORBIDDEN, "you cannot pause yourself".into()));
        assert_eq!(pause(&api, "a@x", "m@x").await.0, StatusCode::NO_CONTENT);
        assert_eq!(pause(&api, "a@x", "m@x").await.0, StatusCode::NO_CONTENT);
        assert_eq!(unpause(&api, "a@x", "n@x").await.0, StatusCode::NO_CONTENT);
        use futures::StreamExt;
        assert_eq!(api.store.os.list(Some(&"audit".into())).collect::<Vec<_>>().await.len(), 1, "only the real pause");
    }

    #[tokio::test]
    async fn a_paused_member_gets_403_on_team_routes_and_cannot_leave_but_can_be_removed() {
        let api = fixture().await;
        assert_eq!(pause(&api, "a@x", "m@x").await.0, StatusCode::NO_CONTENT);
        let get = get_team(State(api.clone()), as_user(&api, "m@x"), axum::extract::Path("acme".to_string())).await;
        assert_eq!(body(get).await, (StatusCode::FORBIDDEN, "your access to this team is paused".into()));
        let leave = axum::extract::Path(("acme".to_string(), "m@x".to_string()));
        assert_eq!(remove_member(State(api.clone()), as_user(&api, "m@x"), leave).await.status(), StatusCode::FORBIDDEN);
        let p = axum::extract::Path(("acme".to_string(), "m@x".to_string()));
        assert_eq!(remove_member(State(api.clone()), as_user(&api, "a@x"), p).await.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn listing_teams_badges_a_paused_one() {
        let api = fixture().await;
        assert_eq!(pause(&api, "a@x", "m@x").await.0, StatusCode::NO_CONTENT);
        let (st, b) = body(list_teams(State(api.clone()), as_user(&api, "m@x")).await).await;
        assert_eq!(st, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&b).unwrap();
        assert_eq!(v[0]["state"], "paused");
        let (_, b) = body(list_teams(State(api.clone()), as_user(&api, "n@x")).await).await;
        assert_eq!(serde_json::from_str::<serde_json::Value>(&b).unwrap()[0]["state"], "active");
    }

    #[tokio::test]
    async fn pausing_forgets_the_membership_cache() {
        let api = fixture().await;
        let db = api.directory.clone().unwrap();
        assert!(crate::browse::may_read_under(&api, &db, "m@x", "acme").await.unwrap(), "cached yes");
        assert_eq!(pause(&api, "a@x", "m@x").await.0, StatusCode::NO_CONTENT);
        assert!(!crate::browse::may_read_under(&api, &db, "m@x", "acme").await.unwrap());
    }

    #[tokio::test]
    async fn pause_and_unpause_reconcile_the_members_pair_at_once() {
        let mut api = Arc::try_unwrap(fixture().await).ok().unwrap();
        api.directory.as_ref().unwrap().claim_username("m@x", "mem").await.unwrap().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
        let rec = seen.clone();
        api.on_member_state = Some(Arc::new(move |o: String, t: String| {
            rec.lock().unwrap().push((o, t));
            Box::pin(async {})
        }));
        let api = Arc::new(api);
        assert_eq!(pause(&api, "a@x", "m@x").await.0, StatusCode::NO_CONTENT);
        assert_eq!(pause(&api, "a@x", "m@x").await.0, StatusCode::NO_CONTENT, "a no-op");
        assert_eq!(unpause(&api, "a@x", "m@x").await.0, StatusCode::NO_CONTENT);
        let want = ("mem".to_string(), "acme".to_string());
        assert_eq!(*seen.lock().unwrap(), vec![want.clone(), want]);
    }

    #[tokio::test]
    async fn unpause_restores_membership() {
        let api = fixture().await;
        let db = api.directory.clone().unwrap();
        assert_eq!(pause(&api, "a@x", "m@x").await.0, StatusCode::NO_CONTENT);
        assert!(!crate::browse::may_read_under(&api, &db, "m@x", "acme").await.unwrap(), "cached no");
        assert_eq!(unpause(&api, "a@x", "m@x").await.0, StatusCode::NO_CONTENT);
        assert!(crate::browse::may_read_under(&api, &db, "m@x", "acme").await.unwrap());
        assert_eq!(state(&api, &team(&api).await, "m@x"), MemberState::Active);
    }
}
