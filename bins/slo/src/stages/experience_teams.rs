//! Stage 14 · Experience, the teams and repo/pull-request half: `team.*`, `repo.protection`,
//! `repo.commit.patch`, `repo.compare`, `pr.comment`, `pr.close`, `commit.verify`.
//!
//! A sibling of `experience.rs` rather than more of it: four implementers fill that scaffold at
//! once, and a stage file everyone edits is a stage file nobody can merge. `experience.rs` keeps
//! one line per id, which is the whole of the shared surface.
//!
//! **Every step derives its own names from `Ctx::prefix`** instead of threading state through
//! `State` — the team slug, the shared repo, the branches. That is what lets each id be one
//! self-contained call from the scaffold in the catalogue's order, and it is also the more honest
//! shape: what the previous step left behind is read back from the platform, so a step that
//! "passed" while writing nothing fails here rather than being believed.
//!
//! Two places where the brief and the code disagree, both deliberate and both real findings:
//!
//! 1. **A protected branch does not refuse an ordinary push.** `protection_verdict`
//!    (`crates/gitbase/src/refs.rs`) refuses a DELETE and a non-fast-forward, and nothing else —
//!    there is no "pushes must go through a pull request" flag in `Protection`. So the refusal this
//!    step measures is the one the platform actually makes: a rewrite of `main`. A step asserting
//!    that a fast-forward push is refused would fail every single run.
//! 2. **A git credential is owned by an OWNER, not by a person** (`auth::authorize`: the token's
//!    owner must equal the repo's owner). A member clones a team repo with a token minted UNDER THE
//!    TEAM — which only a member may mint — and removing that member does not revoke it. So
//!    `team.repo.shared` revokes its own token inside the step, and `team.member.remove` measures
//!    the surfaces where membership IS the check on every request: the browse read and minting a
//!    new team credential.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;
use serde_json::Value;

use super::git::{git, BASE_BRANCH};
use super::{api, get, poll_json, post, raw};
use crate::ctx::{Ctx, OTHER_EMAIL, PROBE_USER};

// Per-step ceilings. Each is at least its catalogue target, for the reason stage 5 states: a slow
// answer must be a breach with a number, never a step the probe cut off.
const QUICK: Duration = Duration::from_secs(20);
const TEAM_REPO_CEILING: Duration = Duration::from_secs(90);
const TEAM_WS_CEILING: Duration = Duration::from_secs(120);
const DELETE_CEILING: Duration = Duration::from_secs(30);
const PROTECTION_CEILING: Duration = Duration::from_secs(150);
const PULL_CEILING: Duration = Duration::from_secs(30);

/// `repo.commit.patch` is bounded at 5 s and `pr.merge.p95` at 60 s; both waits are given their
/// target as the poll cap, so a wait that runs out is the SLO being missed rather than the probe
/// being impatient.
const LOG_CAP: Duration = Duration::from_secs(5);
const MERGE_CAP: Duration = Duration::from_secs(60);

/// The team workspace asks for the same disk stage 5's does, well inside the compiled-in
/// `default-team` quota a team with no `Quota` object of its own inherits.
const QUOTA_GB: u64 = 1;

// ── names ───────────────────────────────────────────────────────────────────
//
// All of them under `run-{run_id}`, which is the whole of teardown's contract — and all of them
// pure functions of the run, which is what lets one id read back what the previous one wrote.

/// `run-{id}-team`. Lowercase, dashes and digits only, and 26 characters for a typical run id:
/// `check_handle` caps a handle at 39.
fn slug(c: &Ctx) -> String {
    format!("{}-team", c.prefix())
}

/// The repository the team owns and its member clones.
fn shared_repo(c: &Ctx) -> String {
    format!("{}-shared", c.prefix())
}

/// The branch `repo.protection` opens its change from, and the two `commit_patch` writes.
fn prot_branch(c: &Ctx) -> String {
    format!("{}-prot", c.prefix())
}

fn patch_branch(c: &Ctx) -> String {
    format!("{}-patch", c.prefix())
}

fn closed_branch(c: &Ctx) -> String {
    format!("{}-closed", c.prefix())
}

// ── teams ───────────────────────────────────────────────────────────────────

/// `team.create`: a person makes a team, and it is listed back.
///
/// The read-back is half the step: `create` reserves the handle and inserts the document in two
/// writes, so a 201 whose team nobody can then open is exactly the failure worth catching.
pub(super) async fn create(c: &mut Ctx) {
    c.step("team.create", QUICK, |c| {
        let (slug, jwt) = (slug(c), c.probe_jwt.clone());
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
pub(super) async fn invite_accept(c: &mut Ctx) {
    if !team_exists(c).await {
        return c.skip("team.invite.accept", "the team was never created");
    }
    c.step("team.invite.accept", QUICK, |c| {
        let (slug, jwt, other) = (slug(c), c.probe_jwt.clone(), c.other_jwt.clone());
        async move {
            let body = serde_json::json!({ "email": OTHER_EMAIL, "role": "member" });
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
pub(super) async fn role_set(c: &mut Ctx) {
    if !is_member(c).await {
        return c.skip("team.role.set", "the second user never joined the team");
    }
    c.step("team.role.set", QUICK, |c| {
        let (slug, jwt) = (slug(c), c.probe_jwt.clone());
        let url = api(c, &format!("/v1/teams/{slug}/members/{OTHER_EMAIL}"));
        async move {
            let body = serde_json::json!({ "role": "admin" });
            super::call(c, reqwest::Method::PATCH, &url, &jwt, Some(body))
                .await
                .context("could not change the role")?;
            let team = get(c, &api(c, &format!("/v1/teams/{slug}")), &jwt).await?;
            if role_of(&team, OTHER_EMAIL).as_deref() != Some("admin") {
                return Err(anyhow!("the profile still does not show them as an admin"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// The role one member holds, out of the `TeamDoc` `/v1/teams/{slug}` answers.
fn role_of(team: &Value, email: &str) -> Option<String> {
    team.get("members")?
        .as_array()?
        .iter()
        .find(|m| m.get("email").and_then(Value::as_str).is_some_and(|e| e.eq_ignore_ascii_case(email)))
        .and_then(|m| m.get("role"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// `team.repo.shared`: a repository the TEAM owns, pushed to and cloned by a member, and refused
/// to a third identity that has no credential at all.
///
/// The credential is minted UNDER THE TEAM by the member — `create_token` runs `may_act_under`, so
/// a non-member cannot mint one at all, and that mint is itself the membership check. It is revoked
/// inside the step: a team-owned token is not swept by teardown (which lists `/v1/tokens` for the
/// two USERS) and outlives the team's slug, which is the one credential this probe must never leak.
pub(super) async fn repo_shared(c: &mut Ctx) {
    if !is_member(c).await {
        return c.skip("team.repo.shared", "the second user never joined the team");
    }
    let slug = slug(c);
    let name = shared_repo(c);
    // Untimed: the repository is a precondition, not an SLO of its own — the catalogue has no id
    // for it, and a failure here is reported as the step it blocked.
    let body = serde_json::json!({ "owner": slug, "name": name, "visibility": "private" });
    if let Err(e) = post(c, &api(c, "/v1/repos"), &c.probe_jwt.clone(), body).await {
        return c.skip("team.repo.shared", &format!("no team repo: {e:#}"));
    }

    let work = c.tmp.join("team").join(&name);
    let _ = std::fs::remove_dir_all(&work);
    let dest = c.tmp.join("team-clone");
    let _ = std::fs::remove_dir_all(&dest);
    c.step("team.repo.shared", TEAM_REPO_CEILING, move |c| {
        let (slug, name, other) = (slug.clone(), name.clone(), c.other_jwt.clone());
        let url = format!("{}/{slug}/{name}.git", c.cfg.git_url.trim_end_matches('/'));
        let refs = format!("{url}/info/refs?service=git-upload-pack");
        let tokens = api(c, "/v1/tokens");
        let run_id = c.run_id.clone();
        async move {
            let minted = post(
                c,
                &tokens,
                &other,
                serde_json::json!({ "owner": slug, "name": format!("{}-team", run_id) }),
            )
            .await
            .context("a member could not mint a credential under the team")?;
            let token = minted
                .get("token")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .ok_or_else(|| anyhow!("the answer carried no token"))?
                .to_string();
            let id = minted.get("_id").and_then(Value::as_str).map(str::to_string);

            let walked = push_and_clone(c, &work, &dest, &url, &token, &refs).await;
            // Revoked whatever happened above, and BEFORE the verdict, so a failing clone never
            // leaves a live credential for a team that teardown is about to delete.
            if let Some(id) = id {
                if let Err(e) = super::call(c, reqwest::Method::DELETE, &api(c, &format!("/v1/tokens/{id}")), &other, None).await {
                    tracing::error!(kind = "token", op = "revoke", error = %format!("{e:#}"), "slo.experience.failed");
                }
            }
            walked
        }
        .boxed()
    })
    .await;
}

/// The git half of `team.repo.shared`: seed and push as the team, clone it back, and check that a
/// caller with no credential is refused.
async fn push_and_clone(
    c: &Ctx,
    work: &std::path::Path,
    dest: &std::path::Path,
    url: &str,
    token: &str,
    refs: &str,
) -> Result<()> {
    std::fs::create_dir_all(work).with_context(|| format!("could not make {}", work.display()))?;
    git(c, vec!["init".into(), "-q".into(), format!("--initial-branch={BASE_BRANCH}")], Some(work)).await?;
    std::fs::write(work.join("README.md"), "# shared\n").context("could not write README.md")?;
    git(c, vec!["add".into(), "-A".into()], Some(work)).await?;
    git(c, vec!["commit".into(), "-q".into(), "-m".into(), "seed".into()], Some(work)).await?;
    git(c, with_token(token, &["push", "-q", url, BASE_BRANCH]), Some(work))
        .await
        .context("a member could not push to the team repo")?;
    git(c, with_token(token, &["clone", "-q", url, &dest.display().to_string()]), None)
        .await
        .context("a member could not clone the team repo")?;
    // The third identity: no credential at all. Asked over HTTP rather than through `git`, because
    // only a status can tell a refusal from a DNS failure — and a step that counted "the command
    // failed" as a refusal would stay green through the outage it exists to catch.
    let (status, body) = raw(c, reqwest::Method::GET, refs, "", None, &[]).await?;
    if matches!(status.as_u16(), 401 | 403 | 404) {
        return Ok(());
    }
    Err(anyhow!("an anonymous read of a private team repo answered {status}: {}", clip(&body)))
}

/// `-c http.extraHeader=…` carrying one specific token as git's `x:<token>` Basic pair.
///
/// A twin of `git::authed` on purpose: that one reads the PROBE's token out of `State`, and every
/// call here is made under a different owner — the team. `tools::run` refuses to put an argv in an
/// error, so this is the only place the token appears.
fn with_token(token: &str, rest: &[&str]) -> Vec<String> {
    use base64::Engine as _;
    let mut args = vec![
        "-c".to_string(),
        format!(
            "http.extraHeader=Authorization: Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("x:{token}"))
        ),
    ];
    args.extend(rest.iter().map(|a| a.to_string()));
    args
}

/// `team.workspace`: a workspace created with `team` set lands in the TEAM's namespace and starts.
///
/// The namespace is the whole point — `ws_namespace` sends a team's workspace to `wt-{owner}-…`
/// rather than the owner's own `ws-{owner}` — so the step reads the pod THERE. Without a
/// kubeconfig there is nothing that could tell the two namespaces apart, and a workspace created
/// to measure nothing is a workspace left behind, so the id skips before creating one.
pub(super) async fn workspace(c: &mut Ctx) {
    if c.kube.is_none() {
        return c.skip("team.workspace", "no kubeconfig");
    }
    if !team_exists(c).await {
        return c.skip("team.workspace", "the team was never created");
    }
    c.step("team.workspace", TEAM_WS_CEILING, |c| {
        let (slug, jwt) = (slug(c), c.probe_jwt.clone());
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

            let ns = kloudlite_workspaces::crd::ws_namespace(PROBE_USER, &slug);
            if !ns.starts_with("wt-") {
                return Err(anyhow!("a team workspace's namespace is {ns}, not a team one"));
            }
            let k = c.kube.as_ref().ok_or_else(|| anyhow!("no kubeconfig"))?;
            let pods: kube::Api<k8s_openapi::api::core::v1::Pod> = kube::Api::namespaced(k.clone(), &ns);
            match pods.get_opt(&id).await {
                Ok(Some(_)) => Ok(()),
                Ok(None) => Err(anyhow!("the workspace is ready but has no pod in {ns}")),
                Err(e) => Err(anyhow!("could not read {ns}: {e}")),
            }
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
pub(super) async fn member_remove(c: &mut Ctx) {
    if !is_member(c).await {
        return c.skip("team.member.remove", "the second user never joined the team");
    }
    c.step("team.member.remove", QUICK, |c| {
        let (slug, name) = (slug(c), shared_repo(c));
        let (jwt, other) = (c.probe_jwt.clone(), c.other_jwt.clone());
        let remove = api(c, &format!("/v1/teams/{slug}/members/{OTHER_EMAIL}"));
        let refs = api(c, &format!("/api/{slug}/{name}/refs"));
        let tokens = api(c, "/v1/tokens");
        async move {
            // Readable while they are still in, so the refusal below is the REMOVAL and not a repo
            // that was never there. A team with no shared repo is not a reason to fail this id.
            let had = get(c, &refs, &other).await.is_ok();
            super::call(c, reqwest::Method::DELETE, &remove, &jwt, None)
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
                let _ = super::call(c, reqwest::Method::DELETE, &api(c, &format!("/v1/tokens/{id}")), &other, None).await;
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
pub(super) async fn delete(c: &mut Ctx) {
    if !team_exists(c).await {
        return c.skip("team.delete", "the team was never created");
    }
    let slug = slug(c);
    let jwt = c.probe_jwt.clone();
    for id in team_workspaces(c, &slug).await {
        let url = api(c, &format!("/v1/workspaces/{id}"));
        match super::call(c, reqwest::Method::DELETE, &url, &jwt, None).await {
            Ok(_) => tracing::info!(kind = "workspace", name = %id, "slo.teardown.deleted"),
            Err(e) => tracing::warn!(kind = "workspace", op = "delete", name = %id, error = %format!("{e:#}"), "slo.teardown.failed"),
        }
    }
    let repo = api(c, &format!("/v1/repos/{slug}/{}", shared_repo(c)));
    if let Err(e) = super::call(c, reqwest::Method::DELETE, &repo, &jwt, None).await {
        tracing::warn!(kind = "repo", op = "delete", error = %format!("{e:#}"), "slo.teardown.failed");
    }

    c.step("team.delete", DELETE_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let (del, read) = (api(c, &format!("/v1/teams/{slug}")), api(c, &format!("/v1/teams/{slug}")));
        async move {
            super::call(c, reqwest::Method::DELETE, &del, &jwt, None)
                .await
                .context("could not delete the team")?;
            refused(c, reqwest::Method::GET, &read, &jwt, "a deleted team").await
        }
        .boxed()
    })
    .await;
}

/// Every workspace the team holds. `/v1/workspaces?team=` is the only listing that shows them —
/// the default one is personal, which is exactly why teardown's sweep cannot reach these.
async fn team_workspaces(c: &Ctx, slug: &str) -> Vec<String> {
    let url = api(c, &format!("/v1/workspaces?team={slug}"));
    let Ok(rows) = get(c, &url, &c.probe_jwt).await else { return vec![] };
    rows.as_array()
        .map(|rows| rows.iter().filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_string)).collect())
        .unwrap_or_default()
}

/// Whether the team this run creates is there. A read, not a remembered flag: the step that made
/// it reports its own outcome, and every later id wants to know what the platform holds now.
async fn team_exists(c: &Ctx) -> bool {
    get(c, &api(c, &format!("/v1/teams/{}", slug(c))), &c.probe_jwt).await.is_ok()
}

/// Whether the second user is in it — asked as THEM, because `/v1/teams/{slug}` answers 404 to a
/// non-member and that is precisely the question.
async fn is_member(c: &Ctx) -> bool {
    get(c, &api(c, &format!("/v1/teams/{}", slug(c))), &c.other_jwt).await.is_ok()
}

// ── the repo and pull-request verbs ─────────────────────────────────────────

/// `repo.protection`: a protected `main` refuses a rewrite, and still takes a pull request.
///
/// The refusal is a FORCE push, not an ordinary one: `protection_verdict` refuses a delete and a
/// non-fast-forward and nothing else, so "a direct push is refused" is measurable only in the shape
/// the platform actually refuses. An orphan commit is used rather than a rewound one so the step
/// does not depend on `main` having a second commit on it.
pub(super) async fn protection(c: &mut Ctx) {
    let Some(name) = c.state.repo.clone() else {
        return c.skip("repo.protection", "no repo");
    };
    let work = c.tmp.join("git").join(&name);
    if !work.is_dir() {
        return c.skip("repo.protection", "the git stage left no working tree");
    }
    let branch = prot_branch(c);
    c.step("repo.protection", PROTECTION_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = format!("{}/{PROBE_USER}/{name}.git", c.cfg.git_url.trim_end_matches('/'));
        let rule = api(c, &format!("/v1/repos/{PROBE_USER}/{name}/protection"));
        let refs = api(c, &format!("/api/{PROBE_USER}/{name}/refs"));
        let pulls = api(c, &format!("/v1/repos/{PROBE_USER}/{name}/pulls"));
        let run_id = c.run_id.clone();
        async move {
            post(c, &rule, &jwt, serde_json::json!({ "pattern": BASE_BRANCH }))
                .await
                .context("could not protect the branch")?;
            let listed = get(c, &rule, &jwt).await.context("could not read the rules back")?;
            let protected = listed.as_array().is_some_and(|rows| {
                rows.iter().any(|r| r.get("pattern").and_then(Value::as_str) == Some(BASE_BRANCH))
            });
            if !protected {
                return Err(anyhow!("the rule was accepted but is not listed"));
            }
            // Everything after this point must run even when a step fails, or the run leaves
            // `main` protected for the next one — which would break stage 2's push.
            let walked = refuse_then_merge(c, &work, &url, &branch, &refs, &pulls, &jwt, &run_id).await;
            let unprotected = post(c, &rule, &jwt, serde_json::json!({ "pattern": BASE_BRANCH, "remove": true }))
                .await
                .map(|_| ());
            if let Err(e) = &unprotected {
                tracing::error!(op = "unprotect", error = %format!("{e:#}"), "slo.experience.failed");
            }
            walked?;
            unprotected.context("`main` was left PROTECTED")
        }
        .boxed()
    })
    .await;
}

/// The two halves the protection rule is about: a rewrite of `main` is refused, and a change that
/// goes through a pull request still lands.
#[allow(clippy::too_many_arguments)]
async fn refuse_then_merge(
    c: &Ctx,
    work: &std::path::Path,
    url: &str,
    branch: &str,
    refs: &str,
    pulls: &str,
    jwt: &str,
    run_id: &str,
) -> Result<()> {
    // An orphan: no shared history with `main` at all, so the push can only be a rewrite.
    git(c, vec!["checkout".into(), "-q".into(), "--orphan".into(), "rewrite".into()], Some(work)).await?;
    git(c, vec!["commit".into(), "-q".into(), "--allow-empty".into(), "-m".into(), "rewrite".into()], Some(work)).await?;
    let force = super::git::authed(c, &["push", "-q", "--force", url, &format!("rewrite:{BASE_BRANCH}")]);
    match git(c, force, Some(work)).await {
        Ok(_) => return Err(anyhow!("a protected branch accepted a rewrite")),
        Err(e) => {
            // Only the rule's own words pass. A 500, a timeout or a wrong URL also make git exit
            // non-zero, and reading those as "the branch is protected" would keep this SLO green
            // through the outage it exists to catch.
            let detail = format!("{e:#}");
            if !detail.contains("is protected") {
                return Err(anyhow!("the push failed for some other reason than the rule: {detail}"));
            }
        }
    }

    // The way through: a branch, a change on it, and a pull request. `main` never moves by hand.
    git(c, vec!["checkout".into(), "-q".into(), BASE_BRANCH.into()], Some(work)).await?;
    git(c, vec!["checkout".into(), "-q".into(), "-b".into(), branch.into()], Some(work)).await?;
    std::fs::write(work.join("protected.txt"), format!("{run_id}\n")).context("could not write protected.txt")?;
    git(c, vec!["add".into(), "-A".into()], Some(work)).await?;
    git(c, vec!["commit".into(), "-q".into(), "-m".into(), "through a pull request".into()], Some(work)).await?;
    let push = super::git::authed(c, &["push", "-q", url, branch]);
    git(c, push, Some(work)).await.context("could not push the change branch")?;
    let target = git(c, vec!["rev-parse".into(), branch.into()], Some(work)).await?.trim().to_string();

    let body = serde_json::json!({ "title": format!("slo protection {run_id}"), "base": BASE_BRANCH, "head": branch });
    let opened = post(c, pulls, jwt, body).await.context("could not open the change")?;
    let number = opened
        .get("number")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("the answer carried no number"))?;
    let merge = format!("{pulls}/{number}/merge?strategy=fast-forward");
    post(c, &merge, jwt, Value::Null).await.context("could not ask for the merge")?;
    poll_json(c, refs, jwt, MERGE_CAP, |r| {
        super::git::oid_of(r, BASE_BRANCH).as_deref() == Some(target.as_str())
    })
    .await
    .context("the merge into a protected branch never landed")
}

/// `repo.commit.patch`: an edit made the way the web's editor makes one — a new file, on a NEW
/// branch off `main` — and then read back out of the log.
///
/// The log read is the SLI, not the 200: the api tier forwards the patch to the owning node, and a
/// commit that is written but not visible is the failure a person sees as "my edit vanished".
pub(super) async fn commit_patch(c: &mut Ctx) {
    let Some(name) = c.state.repo.clone() else {
        return c.skip("repo.commit.patch", "no repo");
    };
    let branch = patch_branch(c);
    c.step("repo.commit.patch", PULL_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/repos/{PROBE_USER}/{name}/commits"));
        let log = api(c, &format!("/api/{PROBE_USER}/{name}/log"));
        let run_id = c.run_id.clone();
        async move {
            let oid = patch(c, &url, &jwt, BASE_BRANCH, &branch, &format!("slo edit {run_id}"), "experience.txt").await?;
            poll_json(c, &format!("{log}/{oid}"), &jwt, LOG_CAP, |rows| {
                rows.as_array().is_some_and(|rows| {
                    rows.first().and_then(|r| r.get("oid")).and_then(Value::as_str) == Some(oid.as_str())
                })
            })
            .await
            .context("the commit never reached the log")
        }
        .boxed()
    })
    .await;
}

/// One `commit_patch` call, answering the oid it wrote. The content carries the run id so two runs
/// never write the same tree — an identical patch is a commit the node can legitimately decline to
/// make twice.
async fn patch(
    c: &Ctx,
    url: &str,
    jwt: &str,
    base: &str,
    new_branch: &str,
    message: &str,
    path: &str,
) -> Result<String> {
    use base64::Engine as _;
    let content = base64::engine::general_purpose::STANDARD.encode(format!("{message}\n"));
    let body = serde_json::json!({
        "branch": base,
        "newBranch": new_branch,
        "message": message,
        "changes": [{ "path": path, "contentBase64": content }],
    });
    let out = post(c, url, jwt, body).await.context("could not commit the patch")?;
    out.get("commit")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("the commit answered no oid"))
}

/// `repo.compare`: `main…branch` lists exactly the one commit the edit made.
///
/// "Exactly" is the measurement: a compare that answers the whole history, or an empty list, both
/// render as a diff nobody can review, and both answer 200.
pub(super) async fn compare(c: &mut Ctx) {
    let Some(name) = c.state.repo.clone() else {
        return c.skip("repo.compare", "no repo");
    };
    let branch = patch_branch(c);
    let Some(oid) = branch_oid(c, &name, &branch).await else {
        return c.skip("repo.compare", "the edit never made a branch to compare");
    };
    c.step("repo.compare", QUICK, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/repos/{PROBE_USER}/{name}/compare?base={BASE_BRANCH}&head={branch}"));
        async move {
            let seen = get(c, &url, &jwt).await.context("could not compare")?;
            let commits = seen.get("commits").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
            match commits {
                [one] if one.get("oid").and_then(Value::as_str) == Some(oid.as_str()) => Ok(()),
                other => Err(anyhow!("the compare lists {} commits, not the one edit", other.len())),
            }
        }
        .boxed()
    })
    .await;
}

/// The tip of one branch, from the browse refs. `None` when it is not there — which is a SKIP for
/// the ids that need it, not a second failure for the step that should have pushed it.
async fn branch_oid(c: &Ctx, name: &str, branch: &str) -> Option<String> {
    let refs = get(c, &api(c, &format!("/api/{PROBE_USER}/{name}/refs")), &c.probe_jwt).await.ok()?;
    super::git::oid_of(&refs, branch)
}

/// `pr.comment`: a comment on a change is readable back off the change itself.
pub(super) async fn comment(c: &mut Ctx) {
    let Some(name) = c.state.repo.clone() else {
        return c.skip("pr.comment", "no repo");
    };
    let branch = patch_branch(c);
    if branch_oid(c, &name, &branch).await.is_none() {
        return c.skip("pr.comment", "the edit never made a branch to open a change from");
    }
    c.step("pr.comment", PULL_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let pulls = api(c, &format!("/v1/repos/{PROBE_USER}/{name}/pulls"));
        let said = format!("slo probe {}", c.run_id);
        async move {
            let number = open_pull(c, &pulls, &jwt, &branch, &said).await?;
            post(c, &format!("{pulls}/{number}/comments"), &jwt, serde_json::json!({ "body": said }))
                .await
                .context("could not comment")?;
            let pull = get(c, &format!("{pulls}/{number}"), &jwt).await.context("could not read the change")?;
            let there = pull
                .get("comments")
                .and_then(Value::as_array)
                .is_some_and(|cs| cs.iter().any(|x| x.get("body").and_then(Value::as_str) == Some(said.as_str())));
            if there {
                Ok(())
            } else {
                Err(anyhow!("the comment was accepted but the change does not carry it"))
            }
        }
        .boxed()
    })
    .await;
}

/// `pr.close`: a closed change is refused a merge.
///
/// On a SECOND change, of its own: `pr.comment`'s is the one a person would go on to merge, and
/// closing it would make the two ids interfere. The branch is made with `commit_patch` rather than
/// git — no working tree needed, and the api tier's own write path is exercised twice.
pub(super) async fn close(c: &mut Ctx) {
    let Some(name) = c.state.repo.clone() else {
        return c.skip("pr.close", "no repo");
    };
    let branch = closed_branch(c);
    let commits = api(c, &format!("/v1/repos/{PROBE_USER}/{name}/commits"));
    let jwt = c.probe_jwt.clone();
    let run = c.run_id.clone();
    // Untimed precondition, like `pr.rs`'s `open`: the catalogue has no id for making a branch.
    if let Err(e) = patch(c, &commits, &jwt, BASE_BRANCH, &branch, &format!("slo close {run}"), "closed.txt").await {
        return c.skip("pr.close", &format!("no change to close: {e:#}"));
    }
    c.step("pr.close", PULL_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let pulls = api(c, &format!("/v1/repos/{PROBE_USER}/{name}/pulls"));
        let run = c.run_id.clone();
        async move {
            let number = open_pull(c, &pulls, &jwt, &branch, &format!("slo close {run}")).await?;
            post(c, &format!("{pulls}/{number}/close"), &jwt, Value::Null)
                .await
                .context("could not close the change")?;
            // 409 from `merge_pull` — "this change is not open". Not in `refused`'s set: this is a
            // state conflict, not an authorization refusal, and the two must not share a helper
            // that would let a 403 here read as a pass.
            let url = format!("{pulls}/{number}/merge?strategy=fast-forward");
            let (status, body) = raw(c, reqwest::Method::POST, &url, &jwt, None, &[]).await?;
            match status.as_u16() {
                409 | 400 => Ok(()),
                _ => Err(anyhow!("merging a closed change answered {status}: {}", clip(&body))),
            }
        }
        .boxed()
    })
    .await;
}

async fn open_pull(c: &Ctx, pulls: &str, jwt: &str, branch: &str, title: &str) -> Result<i64> {
    let body = serde_json::json!({ "title": title, "body": "", "base": BASE_BRANCH, "head": branch });
    let out = post(c, pulls, jwt, body).await.context("could not open the change")?;
    out.get("number").and_then(Value::as_i64).ok_or_else(|| anyhow!("the answer carried no number"))
}

/// `commit.verify`: the signature endpoint answers for a real commit.
///
/// Any verdict passes — the probe's commits are unsigned, and `unsigned` is the honest answer for
/// them. What is measured is that the endpoint ANSWERS, and inside a second: it reads the commit
/// out of the odb on the owning node, so a hung or fenced handle shows up here first.
pub(super) async fn verify(c: &mut Ctx) {
    let Some(name) = c.state.repo.clone() else {
        return c.skip("commit.verify", "no repo");
    };
    let Some(oid) = branch_oid(c, &name, BASE_BRANCH).await else {
        return c.skip("commit.verify", "nothing was pushed");
    };
    c.step("commit.verify", QUICK, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/api/{PROBE_USER}/{name}/signature/{oid}"));
        async move { get(c, &url, &jwt).await.map(|_| ()) }.boxed()
    })
    .await;
}

// ── shared ──────────────────────────────────────────────────────────────────

/// A refusal, and only a refusal: 401, 403 or 404. A 5xx, a timeout or a success are all the thing
/// the check exists to catch, so none of them may pass — the rule every refusal step in this probe
/// is written to (`deploy/slo.md`'s security section, and `sec.*` in stage 9).
async fn refused(c: &Ctx, method: reqwest::Method, url: &str, token: &str, what: &str) -> Result<()> {
    let (status, body) = raw(c, method, url, token, None, &[]).await?;
    if matches!(status.as_u16(), 401 | 403 | 404) {
        return Ok(());
    }
    Err(anyhow!("{what} answered {status}: {}", clip(&body)))
}

fn clip(body: &str) -> String {
    body.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;
    use axum::http::StatusCode;
    use axum::routing::{get as axget, post as axpost};

    fn sample<'a>(c: &'a Ctx, id: &str) -> &'a kloudlite_workspaces::history::slo::StepReport {
        c.steps.iter().find(|s| s.slo_id == id).unwrap_or_else(|| panic!("no {id}"))
    }

    fn once(c: &Ctx, id: &str) {
        assert_eq!(c.steps.iter().filter(|s| s.slo_id == id).count(), 1, "{id} was not reported exactly once");
    }

    /// The team ids each report exactly once when the platform is answering — and `team.create`
    /// carries the failure when the create is refused, while every id that needed the team SKIPS
    /// rather than counting the same broken thing seven times.
    #[tokio::test]
    async fn a_team_that_cannot_be_created_fails_once_and_skips_the_rest() {
        let app = axum::Router::new()
            .route("/v1/teams", axpost(|| async { StatusCode::BAD_GATEWAY }))
            .fallback(axget(|| async { StatusCode::NOT_FOUND }));
        let mut c = testkit::ctx_against(app).await;
        c.kube = None;

        create(&mut c).await;
        invite_accept(&mut c).await;
        role_set(&mut c).await;
        repo_shared(&mut c).await;
        workspace(&mut c).await;
        member_remove(&mut c).await;
        delete(&mut c).await;

        let made = sample(&c, "team.create");
        assert!(!made.ok && !made.skipped, "the create carries the failure");
        for id in ["team.invite.accept", "team.role.set", "team.repo.shared", "team.workspace", "team.member.remove", "team.delete"] {
            assert!(sample(&c, id).skipped, "{id} should be skipped, not sampled");
            once(&c, id);
        }
        assert_eq!(c.failed(), 1, "one broken thing is one failure");
    }

    /// Every id in the group reports exactly once on the success path too — the create is answered,
    /// the team reads back, and each later id runs its own step. Their outcomes are not asserted:
    /// nothing here serves the git listener or a cluster, so what is being held is the CONTRACT
    /// that the run stays exactly-once complete whatever the fleet answers.
    #[tokio::test]
    async fn every_team_id_is_reported_exactly_once_when_the_team_exists() {
        let team = serde_json::json!({
            "slug": "run-fast-1-team",
            "members": [{ "email": OTHER_EMAIL, "role": "admin" }],
        });
        let app = axum::Router::new()
            .route("/v1/teams", axpost(|| async { (StatusCode::CREATED, axum::Json(serde_json::json!({}))) }))
            .route("/v1/teams/{slug}", axget(move || {
                let team = team.clone();
                async move { axum::Json(team) }
            }))
            .fallback(axget(|| async { StatusCode::NOT_FOUND }).post(|| async { StatusCode::NOT_FOUND }));
        let mut c = testkit::ctx_against(app).await;
        c.run_id = "fast-1".into();
        c.kube = None;

        create(&mut c).await;
        invite_accept(&mut c).await;
        role_set(&mut c).await;
        repo_shared(&mut c).await;
        workspace(&mut c).await;
        member_remove(&mut c).await;
        delete(&mut c).await;

        for id in ["team.create", "team.invite.accept", "team.role.set", "team.repo.shared", "team.workspace", "team.member.remove", "team.delete"] {
            once(&c, id);
        }
        // The one id that must SKIP rather than create anything: without a kubeconfig there is no
        // way to tell the team namespace from the personal one, and a workspace made to measure
        // nothing is a workspace left behind.
        assert!(sample(&c, "team.workspace").skipped, "no kubeconfig must skip, not create");
    }

    /// The repo and pull-request ids: with no repo every one of them skips, exactly once, and
    /// nothing is counted as a failure — the repo's own failure was counted in stage 2.
    #[tokio::test]
    async fn the_repo_ids_skip_without_a_repo() {
        let app = axum::Router::new().fallback(axget(|| async { StatusCode::NOT_FOUND }));
        let mut c = testkit::ctx_against(app).await;

        protection(&mut c).await;
        commit_patch(&mut c).await;
        compare(&mut c).await;
        comment(&mut c).await;
        close(&mut c).await;
        verify(&mut c).await;

        for id in ["repo.protection", "repo.commit.patch", "repo.compare", "pr.comment", "pr.close", "commit.verify"] {
            assert!(sample(&c, id).skipped && sample(&c, id).detail == "no repo", "{id}");
            once(&c, id);
        }
        assert_eq!(c.failed(), 0);
    }

    /// The repo ids report exactly once when the repo exists but the fleet answers nothing useful:
    /// each one either fails with a reason or skips, and none is reported twice or dropped.
    #[tokio::test]
    async fn every_repo_id_is_reported_exactly_once_with_a_repo() {
        let app = axum::Router::new().fallback(
            axget(|| async { StatusCode::INTERNAL_SERVER_ERROR }).post(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
        );
        let mut c = testkit::ctx_against(app).await;
        c.state.repo = Some("run-fast-1".into());

        protection(&mut c).await;
        commit_patch(&mut c).await;
        compare(&mut c).await;
        comment(&mut c).await;
        close(&mut c).await;
        verify(&mut c).await;

        for id in ["repo.protection", "repo.commit.patch", "repo.compare", "pr.comment", "pr.close", "commit.verify"] {
            once(&c, id);
        }
    }

    /// Only a refusal passes. The bug this guards against is the natural one — treating "not a
    /// success" as a refusal, which would let a 500 from a broken tier read as access denied.
    #[tokio::test]
    async fn only_401_403_404_count_as_a_refusal() {
        for (code, pass) in [(401, true), (403, true), (404, true), (200, false), (409, false), (500, false)] {
            let app = axum::Router::new().fallback(axget(move || async move {
                StatusCode::from_u16(code).expect("status")
            }));
            let c = testkit::ctx_against(app).await;
            let url = api(&c, "/v1/anything");
            let got = refused(&c, reqwest::Method::GET, &url, "", "a read").await;
            assert_eq!(got.is_ok(), pass, "{code} should {} be a refusal", if pass { "" } else { "not" });
        }
    }

    /// The token rides in an `http.extraHeader` as git's `x:<token>` Basic pair, which is the one
    /// shape the git listener accepts — and it is built here rather than read out of `State`,
    /// because every call in this file is made under the TEAM's credential, not the probe's.
    #[test]
    fn a_team_credential_is_carried_as_basic_x_token() {
        use base64::Engine as _;
        let args = with_token("SECRET", &["clone", "url"]);
        let want = base64::engine::general_purpose::STANDARD.encode("x:SECRET");
        assert_eq!(args[0], "-c");
        assert_eq!(args[1], format!("http.extraHeader=Authorization: Basic {want}"));
        assert_eq!(&args[2..], ["clone", "url"]);
    }

    /// Every name this stage writes carries the run prefix, which is the whole of teardown's
    /// contract: an object named anything else is one the sweep can never find.
    #[tokio::test]
    async fn every_name_carries_the_run_prefix() {
        let mut c = testkit::ctx().await;
        c.run_id = "hourly-1757000000".into();
        let p = c.prefix();
        for name in [slug(&c), shared_repo(&c), prot_branch(&c), patch_branch(&c), closed_branch(&c)] {
            assert!(name.starts_with(&p), "{name} is not swept by the {p} prefix");
        }
        // A team slug is a handle: `check_handle` caps it at 39 characters and permits only
        // lowercase letters, digits and dashes.
        let slug = slug(&c);
        assert!(slug.len() <= 39, "{slug} is too long to be a handle");
        assert!(slug.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'), "{slug}");
        assert!(!slug.starts_with('-') && !slug.ends_with('-'), "{slug}");
    }
}
