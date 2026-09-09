//! A team's life as members see it: created, invited into, roles set, a member removed, the
//! team deleted and its `wt-` namespace reaped, a team-owned workspace.

use super::*;


/// `team.create`: a person makes a team, and it is listed back.
///
/// The read-back is half the step: `create` reserves the handle and inserts the document in two
/// writes, so a 201 whose team nobody can then open is exactly the failure worth catching.
pub(crate) async fn create(c: &mut Ctx) {
    c.step("team.create", QUICK, |c| {
        let (slug, jwt) = (team_slug(c), c.probe_jwt.clone());
        let url = api(c, "/v1/teams");
        async move {
            let body = serde_json::json!({ "slug": slug, "name": "kloudlite slo probe" });
            post(c, &url, &jwt, body).await.context("could not create the team")?;
            let team = get(c, &api(c, &format!("/v1/teams/{slug}")), &jwt)
                .await
                .context("the team was created but cannot be read back")?;
            if team.get("slug").and_then(Value::as_str) != Some(slug.as_str()) {
                return Err(anyhow!("the team reads back as something else"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}


/// `team.invite.accept`: issue an invitation, preview it as the invited person, accept it once —
/// and prove a second accept is refused.
///
/// The one-shot half is the point: the raw token travels in a URL and an email, so an invitation
/// that could be redeemed twice is a membership anybody who ever saw the link can re-take. The
/// token is never formatted into a detail — `raw`'s errors carry the status and body, never the URL.
pub(crate) async fn invite_accept(c: &mut Ctx) {
    if !team_exists(c).await {
        return c.skip("team.invite.accept", "the team was never created");
    }
    c.step("team.invite.accept", QUICK, |c| {
        let other_email = c.other_email.clone();
        let (slug, jwt, other) = (team_slug(c), c.probe_jwt.clone(), c.other_jwt.clone());
        async move {
            let body = serde_json::json!({ "email": other_email, "role": "member" });
            let issued = post(c, &api(c, &format!("/v1/teams/{slug}/invites")), &jwt, body)
                .await
                .context("could not invite")?;
            let token = issued
                .get("token")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .ok_or_else(|| anyhow!("the invitation carried no token"))?
                .to_string();
            let preview = api(c, &format!("/v1/invites/{token}"));
            let accept = api(c, &format!("/v1/invites/{token}/accept"));
            let seen = get(c, &preview, &other).await.context("the invited person cannot preview it")?;
            if seen.get("team").and_then(Value::as_str) != Some(slug.as_str()) {
                return Err(anyhow!("the preview names a different team"));
            }
            post(c, &accept, &other, Value::Null).await.context("the invitation was not accepted")?;
            // Spent, so the second attempt is `Gone` — a 404, the same answer a made-up token gets.
            refused(c, reqwest::Method::POST, &accept, &other, "a second accept").await
        }
        .boxed()
    })
    .await;
}


/// `team.role.set`: promote the member to admin, and read it back off the team.
pub(crate) async fn role_set(c: &mut Ctx) {
    if !is_member(c).await {
        return c.skip("team.role.set", "the second user never joined the team");
    }
    c.step("team.role.set", QUICK, |c| {
        let other_email = c.other_email.clone();
        let (slug, jwt) = (team_slug(c), c.probe_jwt.clone());
        let url = api(c, &format!("/v1/teams/{slug}/members/{other_email}"));
        async move {
            let body = serde_json::json!({ "role": "admin" });
            super::super::call(c, reqwest::Method::PATCH, &url, &jwt, Some(body))
                .await
                .context("could not change the role")?;
            let team = get(c, &api(c, &format!("/v1/teams/{slug}")), &jwt).await?;
            if role_of(&team, &other_email).as_deref() != Some("admin") {
                return Err(anyhow!("the profile still does not show them as an admin"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}


/// The role one member holds, out of the `TeamDoc` `/v1/teams/{slug}` answers.
pub(crate) fn role_of(team: &Value, email: &str) -> Option<String> {
    team.get("members")?
        .as_array()?
        .iter()
        .find(|m| m.get("email").and_then(Value::as_str).is_some_and(|e| e.eq_ignore_ascii_case(email)))
        .and_then(|m| m.get("role"))
        .and_then(Value::as_str)
        .map(str::to_string)
}


/// `team.workspace`: a workspace created with `team` set lands in the TEAM's namespace and starts.
///
/// The namespace is the whole point — `ws_namespace` sends a team's workspace to `wt-{owner}-…`
/// rather than the owner's own `ws-{owner}` — so the step reads the pod THERE. Without a
/// kubeconfig there is nothing that could tell the two namespaces apart, and a workspace created
/// to measure nothing is a workspace left behind, so the id skips before creating one.
pub(crate) async fn workspace(c: &mut Ctx) {
    if c.kube.is_none() {
        return c.skip("team.workspace", "no kubeconfig");
    }
    if !team_exists(c).await {
        return c.skip("team.workspace", "the team was never created");
    }
    c.step("team.workspace", TEAM_WS_CEILING, |c| {
        let probe = c.probe_user.clone();
        let (slug, jwt) = (team_slug(c), c.probe_jwt.clone());
        let body = serde_json::json!({
            "team": slug,
            "name": format!("{}-teamws", c.prefix()),
            "region": c.cfg.region,
            "quota_gb": QUOTA_GB,
            "packages": [],
        });
        let url = api(c, "/v1/workspaces");
        async move {
            let doc = post(c, &url, &jwt, body).await.context("could not create the team workspace")?;
            let id = doc
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("the answer carried no workspace id"))?
                .to_string();
            let ws = api(c, &format!("/v1/workspaces/{id}"));
            poll_json(c, &ws, &jwt, TEAM_WS_CEILING, |v| {
                v.get("state").and_then(Value::as_str) == Some("ready")
            })
            .await
            .context("the team workspace never became ready")?;

            let ns = kloudlite_workspaces::crd::ws_namespace(&probe, &slug);
            if !ns.starts_with("wt-") {
                return Err(anyhow!("a team workspace's namespace is {ns}, not a team one"));
            }
            let k = c.kube.as_ref().ok_or_else(|| anyhow!("no kubeconfig"))?;
            let pods: kube::Api<k8s_openapi::api::core::v1::Pod> = kube::Api::namespaced(k.clone(), &ns);
            let out = match pods.get_opt(&id).await {
                Ok(Some(_)) => Ok(()),
                Ok(None) => Err(anyhow!("the workspace is ready but has no pod in {ns}")),
                Err(e) => Err(anyhow!("could not read {ns}: {e}")),
            };
            // The pod has been seen, which is the whole assertion — so it is freed here rather than
            // at `team.delete`, minutes later. A pool node is 8 vCPU and a workspace requests 2.
            super::super::lifecycle::park(c, &id).await;
            out
        }
        .boxed()
    })
    .await;
}


/// `team.member.remove`: the removed member loses access to the team's repository.
///
/// Measured on the two surfaces where membership is checked on EVERY request — the browse read
/// through the api tier (`settings_caller` → `may_act_under`) and minting a credential under the
/// team — rather than on a git clone. A token minted while they were a member authenticates as the
/// TEAM (`auth::authorize` compares owners, not people) and removal does not revoke it, so a clone
/// would keep working and this SLO would be green while the access it names was never withdrawn.
pub(crate) async fn member_remove(c: &mut Ctx) {
    if !is_member(c).await {
        return c.skip("team.member.remove", "the second user never joined the team");
    }
    c.step("team.member.remove", QUICK, |c| {
        let other_email = c.other_email.clone();
        let (slug, name) = (team_slug(c), shared_repo(c));
        let (jwt, other) = (c.probe_jwt.clone(), c.other_jwt.clone());
        let remove = api(c, &format!("/v1/teams/{slug}/members/{other_email}"));
        let refs = api(c, &format!("/api/{slug}/{name}/refs"));
        let tokens = api(c, "/v1/tokens");
        async move {
            // Readable while they are still in, so the refusal below is the REMOVAL and not a repo
            // that was never there. A team with no shared repo is not a reason to fail this id.
            let had = get(c, &refs, &other).await.is_ok();
            super::super::call(c, reqwest::Method::DELETE, &remove, &jwt, None)
                .await
                .context("could not remove the member")?;
            if had {
                refused(c, reqwest::Method::GET, &refs, &other, "a removed member's read").await?;
            }
            let (status, body) = raw(
                c,
                reqwest::Method::POST,
                &tokens,
                &other,
                Some(serde_json::json!({ "owner": slug, "name": "after-removal" })),
                &[],
            )
            .await?;
            if matches!(status.as_u16(), 401 | 403 | 404) {
                return Ok(());
            }
            // Best effort: a credential that should never have been issued is worse left standing.
            if let Some(id) = serde_json::from_str::<Value>(&body).ok().and_then(|v| v.get("_id").and_then(Value::as_str).map(str::to_string)) {
                let _ = super::super::call(c, reqwest::Method::DELETE, &api(c, &format!("/v1/tokens/{id}")), &other, None).await;
            }
            Err(anyhow!("a removed member could still mint a team credential: {status}"))
        }
        .boxed()
    })
    .await;
}


/// `team.delete`: the team goes, and its slug stops resolving.
///
/// Its workspaces and its repository go FIRST, and not only to make the delete succeed
/// (`delete_team` refuses 409 while the team owns repositories): a team workspace is billed to the
/// team and listed under it, so teardown's per-user sweep cannot see it — deleting it here is the
/// only thing standing between this suite and one leaked subvolume per hour.
pub(crate) async fn delete(c: &mut Ctx) {
    if !team_exists(c).await {
        return c.skip("team.delete", "the team was never created");
    }
    let slug = team_slug(c);
    let repo = api(c, &format!("/v1/repos/{slug}/{}", shared_repo(c)));
    if let Err(e) = super::super::call(c, reqwest::Method::DELETE, &repo, &c.probe_jwt.clone(), None).await {
        tracing::warn!(kind = "repo", op = "delete", error = %format!("{e:#}"), "slo.teardown.failed");
    }

    c.step("team.delete", DELETE_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let (del, read) = (api(c, &format!("/v1/teams/{slug}")), api(c, &format!("/v1/teams/{slug}")));
        async move {
            // Inside the step, and fatal to it: a team workspace is billed to the team and listed
            // only under it, so deleting the team over one strands a subvolume nothing can find
            // again. A leaked team is the far cheaper failure (`stages::drain_team`).
            drain_team(c, &slug, &jwt).await?;
            super::super::call(c, reqwest::Method::DELETE, &del, &jwt, None)
                .await
                .context("could not delete the team")?;
            refused(c, reqwest::Method::GET, &read, &jwt, "a deleted team").await
        }
        .boxed()
    })
    .await;
}


/// `team.namespace.reaped`: no team namespace outlives the workspaces that used it.
///
/// The FLEET-WIDE invariant, not this run's namespace, and deliberately so: the prune runs on the
/// api's resync beat, so waiting for this run's own namespace to go would cost the suite up to two
/// beats. Asking "is any `wt-` namespace older than two beats that nothing resolves to" is two
/// list calls, it answers instantly, and it catches a leak from ANY source — which is what the
/// 2026-09-08 pile of 101 was, one per hourly run since nothing had ever deleted one.
pub(crate) async fn namespace_reaped(c: &mut Ctx) {
    let Some(client) = c.kube.clone() else {
        return c.skip("team.namespace.reaped", "no kubeconfig");
    };
    c.step("team.namespace.reaped", QUICK, move |_| {
        async move {
            use kube::ResourceExt;
            let workspaces: kube::Api<crd::Workspace> = kube::Api::all(client.clone());
            let keep: std::collections::BTreeSet<String> = workspaces
                .list(&kube::api::ListParams::default())
                .await
                .context("could not list the workspaces")?
                .items
                .iter()
                .map(|w| crd::ws_namespace(&w.spec.owner, &w.spec.team))
                .collect();
            let namespaces: kube::Api<k8s_openapi::api::core::v1::Namespace> = kube::Api::all(client);
            let now = chrono::Utc::now().timestamp();
            // THREE beats, not two: a namespace is only a candidate once it is a beat old, and the
            // beat that then deletes it is up to a beat later again — so two is exactly the worst
            // legitimate case and would alarm seconds after it. The third is the slack.
            let grace = 3 * kloudlite_workspaces::api::keys::KEYS_RESYNC_SECS as i64;
            let leaked: Vec<String> = namespaces
                .list(&kube::api::ListParams::default())
                .await
                .context("could not list the namespaces")?
                .items
                .iter()
                .filter(|n| {
                    let name = n.name_any();
                    let age = n.metadata.creation_timestamp.as_ref().map_or(0, |t| now - t.0.as_second());
                    // A namespace already going is the prune working, not a leak.
                    let terminating = n.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Terminating");
                    name.starts_with("wt-") && !keep.contains(&name) && age > grace && !terminating
                })
                .map(|n| n.name_any())
                .collect();
            if !leaked.is_empty() {
                // The prune also spares a namespace that still holds a POD, and this step does not
                // model that on purpose: a `wt-` namespace with a running pod and no Workspace CR
                // behind it is a workspace nothing can find again, which is worth the page.
                let shown: Vec<&str> = leaked.iter().take(4).map(String::as_str).collect();
                return Err(anyhow!("{} team namespace(s) nothing uses: {}", leaked.len(), shown.join(", ")));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}


/// Whether the team this run creates is there. A read, not a remembered flag: the step that made
/// it reports its own outcome, and every later id wants to know what the platform holds now.
pub(crate) async fn team_exists(c: &Ctx) -> bool {
    get(c, &api(c, &format!("/v1/teams/{}", team_slug(c))), &c.probe_jwt).await.is_ok()
}


/// Whether the second user is in it — asked as THEM, because `/v1/teams/{slug}` answers 404 to a
/// non-member and that is precisely the question.
pub(crate) async fn is_member(c: &Ctx) -> bool {
    get(c, &api(c, &format!("/v1/teams/{}", team_slug(c))), &c.other_jwt).await.is_ok()
}

// ── the repo and pull-request verbs ─────────────────────────────────────────
