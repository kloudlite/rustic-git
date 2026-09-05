//! Stage 9 · Security: the refusals. Every id here passes only when something is DENIED.
//!
//! That inversion is the whole reason this stage is written differently from the others: a step
//! whose success is a 4xx must treat a 2xx as a failure AND a 5xx as a failure, because a tier
//! that fell over refused nothing — it just could not answer. `refused` is the one place that
//! judgement is written down, and every step here goes through it.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;
use kloudlite_git_workspaces::crd;

use super::{admin, api, raw};
use crate::ctx::{Ctx, PROBE_USER};

/// Six refusals, 10 s each: every one is a single request against a tier that either says no
/// immediately or is not answering at all, and 60 s is this stage's whole share of the fast
/// suite's 540 s deadline.
const REFUSAL_CEILING: Duration = Duration::from_secs(10);

/// The agent's identity, exactly as `deploy/k3s/agent-rbac.yaml` declares it.
const AGENT_SA: &str = "system:serviceaccount:kube-system:kloudlite-git-agent";

/// The statuses that mean "refused" — `allowed` names which ones this caller accepts. A 5xx is
/// never one of them: the SLI is that the platform says no, and a tier that is down says nothing
/// at all.
fn refused_with(what: &str, status: reqwest::StatusCode, allowed: &[u16]) -> Result<()> {
    match status.as_u16() {
        code if allowed.contains(&code) => Ok(()),
        _ if status.is_success() => Err(anyhow!("{what} was ALLOWED ({status})")),
        _ => Err(anyhow!("{what} answered {status}, which is not a refusal")),
    }
}

/// A read that must not happen: 404 counts, because hiding an object is a refusal.
fn refused(what: &str, status: reqwest::StatusCode) -> Result<()> {
    refused_with(what, status, &[401, 403, 404])
}

/// The admin router's own claim check. A 404 is NOT a pass here — that is `sec.user.process`'s
/// answer, and reading it as one would let a missing route stand in for a working guard.
fn refused_by_claim(what: &str, status: reqwest::StatusCode) -> Result<()> {
    refused_with(what, status, &[401, 403])
}

/// What the API server said to the impersonated spec patch.
///
/// Only a 403 that is NOT about impersonation is a pass: that is the admission policy (or the
/// agent's ClusterRole) refusing the write, which is the SLI. A 400 or 422 means the probe sent a
/// patch the API server could not even apply — our bug, and a refusal of nothing.
fn refused_by_admission(code: u16, message: &str) -> Result<()> {
    match code {
        403 if message.contains("impersonate") => {
            Err(anyhow!("the probe could not impersonate at all: {message}"))
        }
        403 => Ok(()),
        _ => Err(anyhow!("the attempt answered {code}: {message}")),
    }
}

pub async fn run(c: &mut Ctx) {
    private_repo(c).await;
    cross_owner(c).await;
    admin_claim(c).await;
    user_process(c).await;
    agent_spec(c).await;
    token_revoked(c).await;
}

/// The clone handshake, as a plain GET.
///
/// `git ls-remote` makes exactly this request first, and its exit code cannot tell a 401 from a
/// 500 — which is the one distinction every step in this stage rests on.
fn refs_url(c: &Ctx, repo: &str) -> String {
    format!(
        "{}/{PROBE_USER}/{repo}.git/info/refs?service=git-upload-pack",
        c.cfg.git_url.trim_end_matches('/')
    )
}

/// `sec.private.repo`: this run's own repo — private again after stage 2 published and restored
/// it — is unreadable both to the second tenant and to nobody at all.
///
/// Both halves in one step because they are one SLI: a repo readable by either is leaked.
async fn private_repo(c: &mut Ctx) {
    let Some(repo) = c.state.repo.clone() else {
        return c.skip("sec.private.repo", "no repo");
    };
    c.step("sec.private.repo", REFUSAL_CEILING, move |c| {
        let url = refs_url(c, &repo);
        let other = c.other_jwt.clone();
        async move {
            for (who, token) in [("another owner", other.as_str()), ("an anonymous client", "")] {
                let (status, _) = raw(c, reqwest::Method::GET, &url, token, None, &[]).await?;
                refused(&format!("cloning the private repo as {who}"), status)?;
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `sec.cross.owner`: one owner's object is invisible to another.
///
/// The environment is preferred over the workspace because stage 7 collects every worktree it
/// made: a deleted id answers 404 to its OWNER too, and a step that passes on an object nobody can
/// read proves nothing.
async fn cross_owner(c: &mut Ctx) {
    let target = c
        .state
        .environment
        .clone()
        .map(|id| format!("/v1/environments/{id}"))
        .or_else(|| c.state.workspace.clone().map(|id| format!("/v1/workspaces/{id}")));
    let Some(path) = target else {
        return c.skip("sec.cross.owner", "no environment or workspace of ours to read");
    };
    c.step("sec.cross.owner", REFUSAL_CEILING, move |c| {
        let url = api(c, &path);
        let other = c.other_jwt.clone();
        async move {
            let (status, _) = raw(c, reqwest::Method::GET, &url, &other, None, &[]).await?;
            refused("reading our object as another owner", status)
        }
        .boxed()
    })
    .await;
}

/// `sec.admin.claim`: an admin route refuses a token WITHOUT the superadmin claim. The probe's own
/// ordinary JWT is exactly that token, which is why this needs no second identity.
async fn admin_claim(c: &mut Ctx) {
    c.step("sec.admin.claim", REFUSAL_CEILING, |c| {
        let url = admin(c, "/admin/overview");
        let jwt = c.probe_jwt.clone();
        async move {
            let (status, _) = raw(c, reqwest::Method::GET, &url, &jwt, None, &[]).await?;
            refused_by_claim("an admin route reached without the superadmin claim", status)
        }
        .boxed()
    })
    .await;
}

/// `sec.user.process`: the SAME path on the ordinary `/v1` process, with a token that WOULD pass
/// the claim check — so a 404 here is the router split (`api::router` mounts no admin handler at
/// all), not the credential.
async fn user_process(c: &mut Ctx) {
    c.step("sec.user.process", REFUSAL_CEILING, |c| {
        let url = api(c, "/admin/overview");
        let jwt = c.admin_jwt.clone();
        async move {
            let (status, _) = raw(c, reqwest::Method::GET, &url, &jwt, None, &[]).await?;
            if status.as_u16() != 404 {
                return Err(anyhow!("the user process answered {status} for an admin path"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `sec.agent.spec`: impersonating the agent's ServiceAccount, a spec write the admission policy
/// does not allow is refused.
///
/// `--dry-run=server` runs admission and writes nothing, which is what makes it safe to point at
/// a workspace this run does not own — and the probe often owns none by now, since stage 7
/// collects everything it made.
async fn agent_spec(c: &mut Ctx) {
    let Some(client) = c.kube.clone() else {
        return c.skip("sec.agent.spec", "no kubeconfig: the admission policy cannot be tested");
    };
    // Pre-flight: without the `impersonate` verb every attempt below is refused for the WRONG
    // reason, and a step that passes on the probe's own missing grant measures nothing.
    if !may_impersonate(&client).await {
        return c.skip("sec.agent.spec", "probe identity cannot impersonate");
    }
    let Some(name) = a_workspace(&client, c.state.workspace.clone()).await else {
        return c.skip("sec.agent.spec", "no Workspace exists to attempt a spec write on");
    };
    c.step("sec.agent.spec", REFUSAL_CEILING, move |_| {
        async move {
            let mut cfg = kube::Config::infer().await.context("no kubeconfig")?;
            cfg.auth_info.impersonate = Some(AGENT_SA.to_string());
            let as_agent = kube::Client::try_from(cfg).context("could not build the client")?;
            let api: kube::Api<crd::Workspace> = kube::Api::all(as_agent);
            let params = kube::api::PatchParams { dry_run: true, ..Default::default() };
            let patch = serde_json::json!({ "spec": { "desiredState": "Stopped" } });
            match api.patch(&name, &params, &kube::api::Patch::Merge(&patch)).await {
                // A 2xx IS the failure here: admission let the agent rewrite desired state.
                Ok(_) => Err(anyhow!("the agent was ALLOWED to write spec.desiredState")),
                Err(kube::Error::Api(e)) => refused_by_admission(e.code, &e.message),
                Err(e) => Err(anyhow!("the attempt answered {e}, which is not a refusal")),
            }
        }
        .boxed()
    })
    .await;
}

/// Whether the probe's own identity may impersonate a ServiceAccount at all. A review the API
/// server would not answer is a `false`: the step then skips rather than reporting a refusal that
/// was really our own missing grant.
async fn may_impersonate(client: &kube::Client) -> bool {
    use k8s_openapi::api::authorization::v1::{
        ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec,
    };
    let review = SelfSubjectAccessReview {
        spec: SelfSubjectAccessReviewSpec {
            resource_attributes: Some(ResourceAttributes {
                resource: Some("serviceaccounts".into()),
                verb: Some("impersonate".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let api: kube::Api<SelfSubjectAccessReview> = kube::Api::all(client.clone());
    api.create(&kube::api::PostParams::default(), &review)
        .await
        .ok()
        .and_then(|r| r.status)
        .is_some_and(|s| s.allowed)
}

/// A Workspace to attempt the write on: ours if it is still there, otherwise any — the policy is
/// cluster-wide, so any object proves the same thing. A listing that fails is no candidate, and
/// the step skips.
async fn a_workspace(client: &kube::Client, ours: Option<String>) -> Option<String> {
    use kube::api::ResourceExt;
    let api: kube::Api<crd::Workspace> = kube::Api::all(client.clone());
    let items = api.list(&kube::api::ListParams::default()).await.ok()?.items;
    let names: Vec<String> = items.iter().map(|w| w.name_any()).collect();
    ours.filter(|o| names.contains(o)).or_else(|| names.into_iter().next())
}

/// `id.token.revoked`: the personal token stage 1 minted stops working the moment it is revoked.
///
/// Sent the way git sends one — Basic `x:<token>` — because that is the shape a leaked token would
/// actually be replayed in.
async fn token_revoked(c: &mut Ctx) {
    let (Some(id), Some(secret), Some(repo)) =
        (c.state.token.clone(), c.state.token_value.clone(), c.state.repo.clone())
    else {
        return c.skip("id.token.revoked", "no personal token, or no repo to try it against");
    };
    // Gone from the state either way: teardown lists tokens by name and a second delete of a
    // revoked one is a warning nobody needs.
    c.state.token = None;
    c.step("id.token.revoked", REFUSAL_CEILING, move |c| {
        let url = api(c, &format!("/v1/tokens/{id}"));
        let refs = refs_url(c, &repo);
        let jwt = c.probe_jwt.clone();
        let basic = basic(&secret);
        async move {
            super::call(c, reqwest::Method::DELETE, &url, &jwt, None)
                .await
                .context("could not revoke the token")?;
            let (status, _) =
                raw(c, reqwest::Method::GET, &refs, "", None, &[("authorization", basic)]).await?;
            refused("a revoked token", status)
        }
        .boxed()
    })
    .await;
}

/// `Basic x:<token>` — git's own shape (`httpx::basic_token`'s placeholder username).
fn basic(secret: &str) -> String {
    use base64::Engine;
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("x:{secret}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::routing::{any, get};

    #[test]
    fn only_a_4xx_refusal_passes() {
        assert!(refused("x", StatusCode::FORBIDDEN).is_ok());
        assert!(refused("x", StatusCode::UNAUTHORIZED).is_ok());
        assert!(refused("x", StatusCode::NOT_FOUND).is_ok());
        assert!(refused("x", StatusCode::OK).is_err());
        assert!(refused("x", StatusCode::INTERNAL_SERVER_ERROR).is_err());
        assert!(refused("x", StatusCode::BAD_GATEWAY).is_err());
    }

    #[test]
    fn only_an_admission_403_is_a_refusal_of_the_spec_write() {
        assert!(refused_by_admission(403, "denied by validating admission policy").is_ok());
        // Our own missing grant, and our own malformed patch: neither is the platform refusing.
        assert!(refused_by_admission(403, "cannot impersonate resource serviceaccounts").is_err());
        assert!(refused_by_admission(400, "invalid patch").is_err());
        assert!(refused_by_admission(422, "unprocessable").is_err());
    }

    #[test]
    fn the_claim_step_does_not_accept_a_404() {
        assert!(refused_by_claim("x", StatusCode::FORBIDDEN).is_ok());
        assert!(refused_by_claim("x", StatusCode::NOT_FOUND).is_err());
    }

    /// A tier that ALLOWS the thing fails the step; one that refuses passes it. The whole stage's
    /// meaning is this inversion, so it is checked end to end through a real step rather than only
    /// on `refused`.
    #[tokio::test]
    async fn security_steps_pass_only_on_refusal() {
        for (code, want_ok) in [(StatusCode::OK, false), (StatusCode::FORBIDDEN, true)] {
            let app = axum::Router::new().fallback(any(move || async move { code }));
            let mut c = crate::testkit::ctx_against(app).await;
            c.cfg.git_url = c.cfg.api_url.clone();
            c.state.repo = Some("run-fast-1-repo".into());
            c.state.environment = Some("env-1".into());
            private_repo(&mut c).await;
            cross_owner(&mut c).await;
            for s in &c.steps {
                assert_eq!(s.ok, want_ok, "{code}: {s:?}");
            }
        }
    }

    /// `sec.user.process` is the one step a 403 must NOT satisfy: the claim check answering
    /// proves the admin router is mounted, which is precisely what it exists to refute.
    #[tokio::test]
    async fn the_user_process_step_wants_a_404_and_nothing_else() {
        for (code, want_ok) in [(StatusCode::NOT_FOUND, true), (StatusCode::FORBIDDEN, false)] {
            let app = axum::Router::new().route("/admin/overview", get(move || async move { code }));
            let mut c = crate::testkit::ctx_against(app).await;
            user_process(&mut c).await;
            assert_eq!(c.steps[0].ok, want_ok, "{code}");
        }
    }
}
