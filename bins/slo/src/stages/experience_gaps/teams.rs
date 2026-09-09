//! Experience probes on teams: invitations revoked, a team-owned environment, an attached pair.

use super::*;


/// `team.invite.revoke`: a revoked invitation cannot be redeemed.
///
/// Its own invitation, never `team.invite.accept`'s: that one is SPENT by the time this runs, and
/// a spent token is refused whether or not revocation works at all. The token is never formatted
/// into a detail — `raw` carries the status and the body, never the URL.
pub(crate) async fn invite_revoke(c: &mut Ctx) {
    let slug = super::super::experience_teams::team_slug(c);
    if get(c, &api(c, &format!("/v1/teams/{slug}")), &c.probe_jwt).await.is_err() {
        return c.skip("team.invite.revoke", "the team was never created");
    }
    c.step("team.invite.revoke", QUICK, move |c| {
        let other_email = c.other_email.clone();
        let (jwt, other) = (c.probe_jwt.clone(), c.other_jwt.clone());
        let invites = api(c, &format!("/v1/teams/{slug}/invites"));
        async move {
            let issued = post(c, &invites, &jwt, json!({ "email": other_email, "role": "member" }))
                .await
                .context("could not invite")?;
            let token = issued
                .get("token")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .ok_or_else(|| anyhow!("the invitation carried no token"))?
                .to_string();
            let id = issued
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("the invitation carried no id"))?
                .to_string();
            // Readable BEFORE the revoke, so the refusal below is the revocation and not an
            // invitation that was never there.
            get(c, &api(c, &format!("/v1/invites/{token}")), &other)
                .await
                .context("a fresh invitation could not be previewed")?;
            // `invites` is ALREADY absolute; wrapping it in `api()` again concatenated two full
            // URLs and reqwest refused to send the result — a transport error, which is why this
            // read as "could not revoke" rather than as any answer the api gave.
            call(c, reqwest::Method::DELETE, &format!("{invites}/{id}"), &jwt, None)
                .await
                .context("could not revoke the invitation")?;
            let accept = api(c, &format!("/v1/invites/{token}/accept"));
            let (status, body) = raw(c, reqwest::Method::POST, &accept, &other, None, &[]).await?;
            match status.as_u16() {
                401 | 403 | 404 | 410 => Ok(()),
                other => Err(anyhow!("a revoked invitation answered {other}: {}", clip(&body))),
            }
        }
        .boxed()
    })
    .await;
}


/// `team.environment`: the workspace twin every other team verb already has.
///
/// The namespace is the whole claim — `env_namespace` sends an environment to `env-{id}` whoever
/// owns it, so what makes this a TEAM environment is that `/v1` accepted it under the team at all
/// and that its service comes up and resolves there. Deleted inside the step: a team environment
/// is billed to the team and listed only under it, so teardown's per-user sweep cannot see one.
pub(crate) async fn team_environment(c: &mut Ctx) {
    if c.kube.is_none() {
        return c.skip("team.environment", "no kubeconfig");
    }
    let slug = super::super::experience_teams::team_slug(c);
    if get(c, &api(c, &format!("/v1/teams/{slug}")), &c.probe_jwt).await.is_err() {
        return c.skip("team.environment", "the team was never created");
    }
    let name = format!("{}-teamenv", c.prefix());
    c.step("team.environment", TEAM_ENV_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, "/v1/environments");
        let body = json!({
            "team": slug,
            "name": name,
            "region": c.cfg.region,
            "quota_gb": QUOTA_GB,
            "services": [{
                "name": "redis",
                "image": "redis:7-alpine",
                "command": [],
                "env": {},
                "mounts": [],
                "ports": [6379],
            }],
        });
        async move {
            let doc = post(c, &url, &jwt, body).await.context("could not create the team environment")?;
            let id = doc
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("the answer carried no environment id"))?
                .to_string();
            // Registered BEFORE anything else can fail: a team environment is billed to the TEAM
            // and listed only under it, so teardown's per-user prefix sweep cannot see one — this
            // is the seam that does (`drop_extra_volumes`, by name, after the sweep).
            c.state.extra_volumes.push(id.clone());
            let one = api(c, &format!("/v1/environments/{id}"));
            let out = team_env_ready(c, &id, &one, &jwt).await;
            // And the delete FAILS the step rather than warning: a leaked team environment is a
            // subvolume under an owner that will not outlive the team, and a green SLO on top of
            // it is how it would go unnoticed. The `extra_volumes` entry above is the backstop for
            // the run that is killed before it gets here.
            let dropped = call(c, reqwest::Method::DELETE, &one, &jwt, None)
                .await
                .map(|_| ())
                .with_context(|| format!("the team environment {id} was left standing"));
            out.and(dropped)
        }
        .boxed()
    })
    .await;
}


/// The team environment is running, and its service resolves inside its own namespace.
///
/// `running` is the RECORD, not a pod that answers — the same distinction `env.create.p95` makes
/// by waiting on the StatefulSet before it execs. Without that wait this exec'd `redis-0` the
/// instant the environment reported running, hit a pod that did not exist yet, and reported an
/// empty stderr in 600 ms as "the service does not resolve".
///
/// The namespace is not the difference: `env_namespace` answers `env-{id}` for a team's
/// environment exactly as it does for a person's, which the failure's own `env-d874…` shows.
pub(crate) async fn team_env_ready(c: &Ctx, id: &str, one: &str, jwt: &str) -> Result<()> {
    let cap = TEAM_ENV_CEILING - Duration::from_secs(60);
    poll_json(c, one, jwt, cap, |v| v.get("state").and_then(Value::as_str) == Some("running"))
        .await
        .context("the team environment never reported running")?;
    // The StatefulSet's own ready replica, through the same helper stage 6 uses.
    super::super::environment::service_ready(c, id, cap)
        .await
        .context("the team environment's service never became ready")?;
    let ns = kloudlite_workspaces::crd::env_namespace(id);
    // And retried, like `env.dns`'s own loop: a pod that is Ready is a pod the kubelet has
    // started, which is a beat ahead of redis being willing to answer on its port.
    let start = std::time::Instant::now();
    let mut why;
    loop {
        let k = c.kube.as_ref().ok_or_else(|| anyhow!("no kubeconfig"))?;
        match crate::kube::exec(
            k,
            &ns,
            "redis-0",
            None,
            &["sh", "-c", "getent hosts redis >/dev/null && redis-cli -h redis ping"],
            Duration::from_secs(20),
        )
        .await
        {
            Ok((0, out, _)) if out.trim().eq_ignore_ascii_case("pong") => return Ok(()),
            Ok((code, _, err)) => why = format!("exit {code}: {}", err.trim()),
            Err(e) => why = format!("{e:#}"),
        }
        if start.elapsed() >= SVC_ANSWERS {
            return Err(anyhow!("the service in {ns} does not resolve and answer: {why}"));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}


/// `env.attach.pair`: deleting an attached workspace takes the ENVIRONMENT-side policy with it.
///
/// `env.attach`/`env.detach` only ever exercise the workspace side. The environment-side
/// `attach-{ws}` NetworkPolicy lives in the environment's namespace and cannot carry an
/// ownerReference across it, so `delete_ws` removes it BY HAND while the spec is still readable —
/// which is precisely the kind of hand-written cleanup that stops happening unnoticed. The policy
/// is read from Kubernetes rather than inferred: there is no API that reports one.
pub(crate) async fn attach_pair(c: &mut Ctx) {
    let (Some(env), Some(k)) = (c.state.env_multi.clone(), c.kube.clone()) else {
        let why = if c.kube.is_none() { "no kubeconfig" } else { "no environment to attach to" };
        return c.skip("env.attach.pair", why);
    };
    let name = format!("{}-att", c.prefix());
    c.step("env.attach.pair", ATTACH_PAIR_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, "/v1/workspaces");
        let region = c.cfg.region.clone();
        async move {
            let body = json!({ "name": name, "region": region, "quota_gb": QUOTA_GB, "packages": [] });
            let doc = post(c, &url, &jwt, body).await.context("could not create the workspace")?;
            let id = doc
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("the answer carried no workspace id"))?
                .to_string();
            let one = api(c, &format!("/v1/workspaces/{id}"));
            poll_json(c, &one, &jwt, ATTACH_PAIR_CEILING - Duration::from_secs(30), |v| {
                v.get("state").and_then(Value::as_str) == Some("ready")
            })
            .await
            .context("the workspace never became ready")?;
            post(c, &format!("{one}/attach"), &jwt, json!({ "environment": env }))
                .await
                .context("could not attach")?;
            let ns = kloudlite_workspaces::crd::env_namespace(&env);
            let policy = format!("attach-{id}");
            // Present first, or the absence below says nothing: a policy that was never written is
            // gone after the delete whether or not `delete_ws` removes anything.
            netpol_is(&k, &ns, &policy, true, Duration::from_secs(30))
                .await
                .context("attaching wrote no environment-side policy")?;
            call(c, reqwest::Method::DELETE, &one, &jwt, None).await.context("could not delete the workspace")?;
            netpol_is(&k, &ns, &policy, false, Duration::from_secs(30))
                .await
                .context("deleting the attached workspace left the environment-side policy standing")
        }
        .boxed()
    })
    .await;
}


/// Wait until a NetworkPolicy is there, or is not.
pub(crate) async fn netpol_is(k: &kube::Client, ns: &str, name: &str, want: bool, cap: Duration) -> Result<()> {
    let api: kube::Api<k8s_openapi::api::networking::v1::NetworkPolicy> =
        kube::Api::namespaced(k.clone(), ns);
    let start = std::time::Instant::now();
    loop {
        // An unreadable namespace is not an answer: reading an error as "it is gone" would pass
        // this id through an API server that stopped answering.
        let there = api.get_opt(name).await.map_err(|e| anyhow!("could not read {ns}/{name}: {e}"))?.is_some();
        if there == want {
            return Ok(());
        }
        if start.elapsed() >= cap {
            let what = if want { "never appeared" } else { "is still there" };
            return Err(anyhow!("{ns}/{name} {what} after {} ms", cap.as_millis()));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
