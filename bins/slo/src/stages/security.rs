//! Stage 9 · Security: the refusals. Every id here passes only when something is DENIED.
//!
//! That inversion is the whole reason this stage is written differently from the others: a step
//! whose success is a 4xx must treat a 2xx as a failure AND a 5xx as a failure, because a tier
//! that fell over refused nothing — it just could not answer. `refused` is the one place that
//! judgement is written down, and every step here goes through it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;
use kloudlite_workspaces::crd;

use super::{admin, api, raw};
use crate::ctx::Ctx;

/// Six refusals, 10 s each: every one is a single request against a tier that either says no
/// immediately or is not answering at all, and 60 s is this stage's whole share of the fast
/// suite's 540 s deadline.
const REFUSAL_CEILING: Duration = Duration::from_secs(10);

/// The agent's identity, exactly as `deploy/k3s/agent-rbac.yaml` declares it.
const AGENT_SA: &str = "system:serviceaccount:kube-system:kloudlite-agent";

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
        // A ValidatingAdmissionPolicy denial is a 422 whose message names the policy — the
        // refusal this step exists to see; a 422 from the schema names a field instead.
        422 if message.contains("ValidatingAdmissionPolicy") => Ok(()),
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
    visibility(c).await;
}

/// `repo.visibility`: the FLIP, in both directions.
///
/// `sec.private.repo` only ever reads a repo that was private from birth, so a `visibility` write
/// that took a value and stored nothing would keep it green forever. This is the same repo, moved
/// through both states with the read that has to change answer each time — and the second tenant
/// is the reader, because "hidden from a non-collaborator" is what private means to a person.
///
/// It restores PRIVATE inside the step, and outside the cancellable region: stage 9 is the last
/// stage before teardown, and a probe repo left public is a hole the next run's `sec.private.repo`
/// would read as a passing security check.
async fn visibility(c: &mut Ctx) {
    let (Some(repo), probe) = (c.state.repo.clone(), c.probe_user.clone()) else {
        c.skip("repo.visibility", "no repo");
        return c.skip("repo.visibility.public", "no repo");
    };
    // What the public flip did, for the second id. `Arc<Mutex<_>>` rather than a return value
    // because the flip happens inside the 100 % step and its own outcome must not depend on it.
    let seen: Arc<Mutex<Option<String>>> = Arc::default();
    let public = seen.clone();
    let name = repo.clone();
    let owner = probe.clone();
    c.step("repo.visibility", Duration::from_secs(15), move |c| {
        let jwt = c.probe_jwt.clone();
        let other = c.other_jwt.clone();
        let patch = api(c, &format!("/v1/repos/{owner}/{name}"));
        let refs = refs_url(c, &name);
        async move {
            // Private first — where stage 2 left it — so the two reads below are the flip itself
            // rather than whatever state some earlier step happened to leave.
            let (status, _) = raw(c, reqwest::Method::GET, &refs, &other, None, &[]).await?;
            refused("reading the private repo as another owner", status)?;
            let hide = || async {
                flip(c, &patch, &jwt, "private").await.context("the repo was left PUBLIC")
            };
            let shown = async {
                flip(c, &patch, &jwt, "public").await.context("could not publish the repo")?;
                // Recorded, NOT judged here: whether a public repo reads is a positive, and this
                // id is at 100 % — where only refusals belong, because a 100 % budget cannot
                // absorb one flake. `repo.visibility.public` is the id that judges it.
                let (status, body) = raw(c, reqwest::Method::GET, &refs, &other, None, &[]).await?;
                *public.lock().expect("lock") = Some(match status.is_success() {
                    true => String::new(),
                    false => format!("{status}: {}", body.chars().take(200).collect::<String>()),
                });
                Ok(())
            };
            crate::drill::undoing(Duration::from_secs(10), shown, hide).await?;
            // And the flip back took effect, not merely answered: a `visibility` write that
            // reported success and stored nothing is the failure this whole id is about.
            let (status, _) = raw(c, reqwest::Method::GET, &refs, &other, None, &[]).await?;
            refused("reading the repo after it was made private again", status)
        }
        .boxed()
    })
    .await;
    public_readable(c, seen).await;
}

/// `repo.visibility.public`: the other half of the flip, at 99.9 %.
///
/// Split off `repo.visibility` deliberately. The two halves fail for opposite reasons — a private
/// repo that leaks is a security breach, a public repo that will not read is an availability one —
/// and a 100 % target has no budget for the second. So the refusal keeps the 100 % id and this
/// carries the positive, judged on what the flip above already observed rather than by flipping a
/// second time.
async fn public_readable(c: &mut Ctx, seen: Arc<Mutex<Option<String>>>) {
    let out = seen.lock().expect("lock").clone();
    let Some(detail) = out else {
        return c.skip("repo.visibility.public", "the repo was never made public");
    };
    c.step("repo.visibility.public", Duration::from_secs(5), move |_| {
        async move {
            match detail.is_empty() {
                true => Ok(()),
                false => Err(anyhow!("a public repo refused another owner: {detail}")),
            }
        }
        .boxed()
    })
    .await;
}

async fn flip(c: &Ctx, url: &str, jwt: &str, to: &str) -> Result<()> {
    let body = serde_json::json!({ "visibility": to });
    super::call(c, reqwest::Method::PATCH, url, jwt, Some(body)).await.map(|_| ())
}

/// The clone handshake, as a plain GET.
///
/// `git ls-remote` makes exactly this request first, and its exit code cannot tell a 401 from a
/// 500 — which is the one distinction every step in this stage rests on.
fn refs_url(c: &Ctx, repo: &str) -> String {
    let probe = c.probe_user.clone();
    format!(
        "{}/{probe}/{repo}.git/info/refs?service=git-upload-pack",
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
/// Runs from the WORKSPACE stage while the probe's own workspace exists (the security stage comes
/// after lifecycle has deleted it, and an empty fleet has no other object to try); the security
/// stage only re-runs it when that early attempt never happened. `agent.spec.allowed` — the
/// positive half, at 99.9 % — rides along with it for the same reason.
pub(crate) async fn agent_spec(c: &mut Ctx) {
    if c.state.agent_spec_done {
        return;
    }
    c.state.agent_spec_done = true;
    let Some(client) = c.kube.clone() else {
        c.skip("sec.agent.spec", "no kubeconfig: the admission policy cannot be tested");
        return c.skip("agent.spec.allowed", "no kubeconfig: the admission policy cannot be tested");
    };
    // Pre-flight: without the `impersonate` verb every attempt below is refused for the WRONG
    // reason, and a step that passes on the probe's own missing grant measures nothing.
    if !may_impersonate(&client).await {
        c.skip("sec.agent.spec", "probe identity cannot impersonate");
        return c.skip("agent.spec.allowed", "probe identity cannot impersonate");
    }
    let Some(name) = a_workspace(&client, c.state.workspace.clone()).await else {
        c.skip("sec.agent.spec", "no Workspace exists to attempt a spec write on");
        return c.skip("agent.spec.allowed", "no Workspace exists to attempt a spec write on");
    };
    let volume = a_volume(&client).await;
    c.step("sec.agent.spec", REFUSAL_CEILING, move |_| {
        async move {
            let mut cfg = kube::Config::infer().await.context("no kubeconfig")?;
            cfg.auth_info.impersonate = Some(AGENT_SA.to_string());
            let as_agent = kube::Client::try_from(cfg).context("could not build the client")?;
            let api: kube::Api<crd::Workspace> = kube::Api::all(as_agent.clone());
            let params = kube::api::PatchParams { dry_run: true, ..Default::default() };
            // Lowercase: the CRD's enum is `running|stopped`; a wrong case is a 422 from the schema,
            // which is not the admission policy refusing anything.
            let patch = serde_json::json!({ "spec": { "desiredState": "stopped" } });
            match api.patch(&name, &params, &kube::api::Patch::Merge(&patch)).await {
                // A 2xx IS the failure here: admission let the agent rewrite desired state.
                Ok(_) => return Err(anyhow!("the agent was ALLOWED to write spec.desiredState")),
                Err(kube::Error::Api(e)) => refused_by_admission(e.code, &e.message)?,
                Err(e) => return Err(anyhow!("the attempt answered {e}, which is not a refusal")),
            }
            Ok(())
        }
        .boxed()
    })
    .await;
    agent_spec_allowed(c, volume).await;
}

/// `agent.spec.allowed`: the two spec writes the agent's ClusterRole DOES allow, at 99.9 %.
///
/// Split off `sec.agent.spec` deliberately. The refusal belongs at 100 % — a policy that let the
/// agent rewrite desired state is a breach with no acceptable rate — but this half is a POSITIVE
/// against an object the probe did not create, over a kube transport that can flake, and a 100 %
/// budget cannot absorb that. It is still worth an id: `restoreTo` and `quotaGb` are what
/// `deploy/k3s/agent-rbac.yaml`'s header table grants and what the ValidatingAdmissionPolicy lets
/// through, so a policy tightened into refusing EVERYTHING would pass the refusal check while
/// stopping every restore and every quota change on the fleet.
///
/// Both patches are dry-run, so nothing is written either way.
async fn agent_spec_allowed(c: &mut Ctx, volume: Option<String>) {
    if c.kube.is_none() {
        return c.skip("agent.spec.allowed", "no kubeconfig");
    }
    let Some(volume) = volume else {
        return c.skip("agent.spec.allowed", "no Volume exists to attempt an allowed write on");
    };
    c.step("agent.spec.allowed", REFUSAL_CEILING, move |_| {
        async move {
            let mut cfg = kube::Config::infer().await.context("no kubeconfig")?;
            cfg.auth_info.impersonate = Some(AGENT_SA.to_string());
            let as_agent = kube::Client::try_from(cfg).context("could not build the client")?;
            let api: kube::Api<crd::Volume> = kube::Api::all(as_agent);
            let params = kube::api::PatchParams { dry_run: true, ..Default::default() };
            for (field, patch) in [
                ("spec.restoreTo", serde_json::json!({ "spec": { "restoreTo": "" } })),
                ("spec.quotaGb", serde_json::json!({ "spec": { "quotaGb": 1 } })),
            ] {
                match api.patch(&volume, &params, &kube::api::Patch::Merge(&patch)).await {
                    Ok(_) => {}
                    Err(kube::Error::Api(e)) => return denied_or_noise(field, e.code, &e.message),
                    // NOT a refusal: a connection reset, a TLS error or a timeout is the probe's
                    // own path to the API server, and calling that "the agent was refused" would
                    // burn this budget on infrastructure noise.
                    Err(e) => return Err(anyhow!("the API server could not be reached for {field}: {e}")),
                }
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// An API error on an ALLOWED write: only an actual denial is this id's failure.
///
/// A 403 or a policy 422 is the grant having been taken away, which is the thing worth knowing. A
/// 404 (the Volume went between the list and the patch, which this run's own teardown does), a 429
/// or a 5xx is the cluster being busy, and neither is the policy refusing anything.
fn denied_or_noise(field: &str, code: u16, message: &str) -> Result<()> {
    match code {
        403 => Err(anyhow!("the agent was REFUSED {field}, which its reconcilers need: {message}")),
        422 if message.contains("ValidatingAdmissionPolicy") => {
            Err(anyhow!("the admission policy refused {field}, which its reconcilers need: {message}"))
        }
        _ => Err(anyhow!("the {field} write answered {code}, which is not a policy decision: {message}")),
    }
}

/// Any `Volume` to try the allowed writes on — the policy is cluster-wide, so any object proves
/// the same thing, exactly as `a_workspace` does for the refusal.
async fn a_volume(client: &kube::Client) -> Option<String> {
    use kube::api::ResourceExt;
    let api: kube::Api<crd::Volume> = kube::Api::all(client.clone());
    api.list(&kube::api::ListParams::default()).await.ok()?.items.first().map(|v| v.name_any())
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
                // The grant is scoped by resourceName (slo-rbac.yaml); an unnamed review reads as
                // "may impersonate ANY serviceaccount", which is exactly what we are not allowed.
                name: Some("kloudlite-agent".into()),
                namespace: Some("kube-system".into()),
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

/// `id.token.revoked`: a personal token stops working the moment it is revoked.
///
/// A THROWAWAY token, minted here and revoked here — never the one stage 1 minted, which every
/// later git push (`repo.protection` in stage 14 among them) still signs with; the git tier takes
/// Basic `x:<token>` and nothing else, so revoking that one turns the rest of the run into 401s.
/// Sent the way git sends one — Basic `x:<token>` — because that is the shape a leaked token would
/// actually be replayed in.
async fn token_revoked(c: &mut Ctx) {
    let Some(repo) = c.state.repo.clone() else {
        return c.skip("id.token.revoked", "no repo to try it against");
    };
    let name = format!("{}-revoked", c.prefix());
    let probe = c.probe_user.clone();
    c.step("id.token.revoked", REFUSAL_CEILING, move |c| {
        let tokens = api(c, "/v1/tokens");
        let refs = refs_url(c, &repo);
        let jwt = c.probe_jwt.clone();
        async move {
            let body = serde_json::json!({ "owner": probe, "name": name });
            let out = super::post(c, &tokens, &jwt, body).await.context("could not mint a token to revoke")?;
            let id = out.pointer("/_id").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("the answer carried no id"))?;
            let secret = out.get("token").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("the answer carried no token"))?;
            let basic = basic(secret);
            let url = format!("{tokens}/{id}");
            super::call(c, reqwest::Method::DELETE, &url, &jwt, None)
                .await
                .context("could not revoke the token")?;
            let (status, _) =
                raw(c, reqwest::Method::GET, &refs, "", None, &[("authorization", basic)]).await?;
            refused_with("a revoked token", status, &[400, 401, 403])
        }
        .boxed()
    })
    .await;
}

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

    /// The split the review asked for: a 100 % id asserts only refusals, so each of the two now
    /// has a 99.9 % sibling carrying its positive — and BOTH must still emit exactly once on every
    /// path, or a run stops being exactly-once complete.
    #[tokio::test]
    async fn each_hundred_percent_id_has_its_positive_split_off_and_both_emit_once() {
        let app = axum::Router::new().fallback(any(|| async { StatusCode::FORBIDDEN }));
        let mut c = crate::testkit::ctx_against(app).await;
        c.kube = None;
        c.cfg.git_url = c.cfg.api_url.clone();
        c.state.repo = Some("run-fast-1-repo".into());

        visibility(&mut c).await;
        agent_spec(&mut c).await;

        for id in ["repo.visibility", "repo.visibility.public", "sec.agent.spec", "agent.spec.allowed"] {
            assert_eq!(c.steps.iter().filter(|s| s.slo_id == id).count(), 1, "{id}");
        }
        // This fleet refuses the PATCH too, so the flip never happens — and the positive sibling
        // SKIPS rather than failing, which is the property the split is for: a repo that was never
        // published cannot say anything about whether a published one reads.
        let public = c.steps.iter().find(|s| s.slo_id == "repo.visibility.public").expect("id");
        assert!(public.skipped && public.detail == "the repo was never made public", "{public:?}");
        // No kubeconfig is a deployment gap for both agent ids, never a breach.
        for id in ["sec.agent.spec", "agent.spec.allowed"] {
            assert!(c.steps.iter().find(|s| s.slo_id == id).expect(id).skipped, "{id}");
        }
    }

    /// Only a real DENIAL fails `agent.spec.allowed`. A 404 (this run's own teardown took the
    /// Volume), a 429 or a 5xx is the cluster being busy, and burning a budget on that is what
    /// splitting this off a 100 % id was meant to avoid.
    #[test]
    fn only_a_denial_fails_the_allowed_writes() {
        assert!(denied_or_noise("spec.quotaGb", 403, "denied").unwrap_err().to_string().contains("REFUSED"));
        let policy = denied_or_noise("spec.restoreTo", 422, "ValidatingAdmissionPolicy denied");
        assert!(policy.unwrap_err().to_string().contains("admission policy refused"));
        for code in [404, 429, 500, 503] {
            let e = denied_or_noise("spec.quotaGb", code, "busy").unwrap_err().to_string();
            assert!(e.contains("not a policy decision"), "{code}: {e}");
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
