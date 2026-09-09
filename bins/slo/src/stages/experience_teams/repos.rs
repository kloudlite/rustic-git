//! A team's shared repository: push and clone as a member, branch protection, patches, compare,
//! comments, close, the merge that is refused and then allowed, commit verification.

use super::*;


/// The repository the team owns and its member clones.
pub(crate) fn shared_repo(c: &Ctx) -> String {
    format!("{}-shared", c.prefix())
}


/// The branch `repo.protection` opens its change from, and the two `commit_patch` writes.
pub(crate) fn prot_branch(c: &Ctx) -> String {
    format!("{}-prot", c.prefix())
}


pub(crate) fn patch_branch(c: &Ctx) -> String {
    format!("{}-patch", c.prefix())
}


pub(crate) fn closed_branch(c: &Ctx) -> String {
    format!("{}-closed", c.prefix())
}

// ── teams ───────────────────────────────────────────────────────────────────


/// `team.repo.shared`: a repository the TEAM owns, pushed to and cloned by a member, and refused
/// to a third identity that has no credential at all.
///
/// The credential is minted UNDER THE TEAM by the member — `create_token` runs `may_act_under`, so
/// a non-member cannot mint one at all, and that mint is itself the membership check. It is revoked
/// inside the step: a team-owned token is not swept by teardown (which lists `/v1/tokens` for the
/// two USERS) and outlives the team's slug, which is the one credential this probe must never leak.
pub(crate) async fn repo_shared(c: &mut Ctx) {
    if !is_member(c).await {
        return c.skip("team.repo.shared", "the second user never joined the team");
    }
    let slug = team_slug(c);
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

            // Revoked outside the cancellable region, so a clone that runs out of time never
            // leaves a live credential for a team that teardown is about to delete.
            let revoke = || async {
                let Some(id) = id else { return Ok(()) };
                super::super::call(c, reqwest::Method::DELETE, &api(c, &format!("/v1/tokens/{id}")), &other, None)
                    .await
                    .map(|_| ())
                    .context("the team credential was left LIVE")
            };
            undoing(TEAM_REPO_BODY, push_and_clone(c, &work, &dest, &url, &token, &refs), revoke).await
        }
        .boxed()
    })
    .await;
}


/// The git half of `team.repo.shared`: seed and push as the team, clone it back, and check that a
/// caller with no credential is refused.
pub(crate) async fn push_and_clone(
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
pub(crate) fn with_token(token: &str, rest: &[&str]) -> Vec<String> {
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


/// `repo.protection`: a protected `main` refuses a rewrite, and still takes a pull request.
///
/// The refusal is a FORCE push, not an ordinary one: `protection_verdict` refuses a delete and a
/// non-fast-forward and nothing else, so "a direct push is refused" is measurable only in the shape
/// the platform actually refuses. An orphan commit is used rather than a rewound one so the step
/// does not depend on `main` having a second commit on it.
pub(crate) async fn protection(c: &mut Ctx) {
    let Some(name) = c.state.repo.clone() else {
        return c.skip("repo.protection", "no repo");
    };
    let work = c.tmp.join("git").join(&name);
    if !work.is_dir() {
        return c.skip("repo.protection", "the git stage left no working tree");
    }
    let branch = prot_branch(c);
    c.step("repo.protection", PROTECTION_CEILING, move |c| {
        let probe = c.probe_user.clone();
        let jwt = c.probe_jwt.clone();
        let url = format!("{}/{probe}/{name}.git", c.cfg.git_url.trim_end_matches('/'));
        let rule = api(c, &format!("/v1/repos/{probe}/{name}/protection"));
        let refs = api(c, &format!("/api/{probe}/{name}/refs"));
        let pulls = api(c, &format!("/v1/repos/{probe}/{name}/pulls"));
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
            // Outside the cancellable region: a run that leaves `main` protected breaks the NEXT
            // run's stage 2 push, and the step's own timeout would drop this with the body.
            let unprotect = || async {
                post(c, &rule, &jwt, serde_json::json!({ "pattern": BASE_BRANCH, "remove": true }))
                    .await
                    .map(|_| ())
                    .context("`main` was left PROTECTED")
            };
            let walked = refuse_then_merge(c, &work, &url, &branch, &refs, &pulls, &jwt, &run_id);
            undoing(PROTECTION_BODY, walked, unprotect).await
        }
        .boxed()
    })
    .await;
}


/// The two halves the protection rule is about: a rewrite of `main` is refused, and a change that
/// goes through a pull request still lands.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn refuse_then_merge(
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
    let force = super::super::git::authed(c, &["push", "-q", "--force", url, &format!("rewrite:{BASE_BRANCH}")]);
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
    // From the branch's CURRENT tip, fetched now: this clone's `main` is stage 2's, and the PR
    // stage has merged past it since, so a branch off the local copy is not a fast-forward.
    let fetch = super::super::git::authed(c, &["fetch", "-q", url, BASE_BRANCH]);
    git(c, fetch, Some(work)).await.context("could not fetch the base branch")?;
    git(c, vec!["checkout".into(), "-q".into(), "-B".into(), branch.into(), "FETCH_HEAD".into()], Some(work)).await?;
    std::fs::write(work.join("protected.txt"), format!("{run_id}\n")).context("could not write protected.txt")?;
    git(c, vec!["add".into(), "-A".into()], Some(work)).await?;
    git(c, vec!["commit".into(), "-q".into(), "-m".into(), "through a pull request".into()], Some(work)).await?;
    let push = super::super::git::authed(c, &["push", "-q", url, branch]);
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
    let landed = poll_json(c, refs, jwt, MERGE_CAP, |r| {
        super::super::git::oid_of(r, BASE_BRANCH).as_deref() == Some(target.as_str())
    })
    .await;
    if landed.is_err() {
        // The worker's own verdict, so a refusal reads as the fleet's sentence rather than as a
        // silence: `merge.state` and `merge.detail` are what the person waiting would see.
        let pr = super::super::get(c, &format!("{pulls}/{number}"), jwt).await.ok();
        let job = pr.as_ref().and_then(|p| p.get("merge")).cloned().unwrap_or(Value::Null);
        let (state, detail) = (
            job.get("state").and_then(Value::as_str).unwrap_or("no job").to_string(),
            job.get("detail").and_then(Value::as_str).unwrap_or("").to_string(),
        );
        return landed.map(|_| ()).with_context(|| format!("the merge into a protected branch never landed: merge {state} {detail}"));
    }
    Ok(())
}


/// `repo.commit.patch`: an edit made the way the web's editor makes one — a new file, on a NEW
/// branch off `main` — and then read back out of the log.
///
/// The log read is the SLI, not the 200: the api tier forwards the patch to the owning node, and a
/// commit that is written but not visible is the failure a person sees as "my edit vanished".
pub(crate) async fn commit_patch(c: &mut Ctx) {
    let Some(name) = c.state.repo.clone() else {
        return c.skip("repo.commit.patch", "no repo");
    };
    let branch = patch_branch(c);
    c.step("repo.commit.patch", PULL_CEILING, move |c| {
        let probe = c.probe_user.clone();
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/repos/{probe}/{name}/commits"));
        let log = api(c, &format!("/api/{probe}/{name}/log"));
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
pub(crate) async fn patch(
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
pub(crate) async fn compare(c: &mut Ctx) {
    let Some(name) = c.state.repo.clone() else {
        return c.skip("repo.compare", "no repo");
    };
    let branch = patch_branch(c);
    let Some(oid) = branch_oid(c, &name, &branch).await else {
        return c.skip("repo.compare", "the edit never made a branch to compare");
    };
    c.step("repo.compare", QUICK, move |c| {
        let probe = c.probe_user.clone();
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/repos/{probe}/{name}/compare?base={BASE_BRANCH}&head={branch}"));
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
pub(crate) async fn branch_oid(c: &Ctx, name: &str, branch: &str) -> Option<String> {
    let probe = c.probe_user.clone();
    let refs = get(c, &api(c, &format!("/api/{probe}/{name}/refs")), &c.probe_jwt).await.ok()?;
    super::super::git::oid_of(&refs, branch)
}


/// `pr.comment`: a comment on a change is readable back off the change itself.
pub(crate) async fn comment(c: &mut Ctx) {
    let Some(name) = c.state.repo.clone() else {
        return c.skip("pr.comment", "no repo");
    };
    let branch = patch_branch(c);
    if branch_oid(c, &name, &branch).await.is_none() {
        return c.skip("pr.comment", "the edit never made a branch to open a change from");
    }
    c.step("pr.comment", PULL_CEILING, move |c| {
        let probe = c.probe_user.clone();
        let jwt = c.probe_jwt.clone();
        let pulls = api(c, &format!("/v1/repos/{probe}/{name}/pulls"));
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
pub(crate) async fn close(c: &mut Ctx) {
    let Some(name) = c.state.repo.clone() else {
        return c.skip("pr.close", "no repo");
    };
    let branch = closed_branch(c);
    let probe = c.probe_user.clone();
    let commits = api(c, &format!("/v1/repos/{probe}/{name}/commits"));
    let jwt = c.probe_jwt.clone();
    let run = c.run_id.clone();
    // Untimed precondition, like `pr.rs`'s `open`: the catalogue has no id for making a branch.
    if let Err(e) = patch(c, &commits, &jwt, BASE_BRANCH, &branch, &format!("slo close {run}"), "closed.txt").await {
        return c.skip("pr.close", &format!("no change to close: {e:#}"));
    }
    c.step("pr.close", PULL_CEILING, move |c| {
        let probe = c.probe_user.clone();
        let jwt = c.probe_jwt.clone();
        let pulls = api(c, &format!("/v1/repos/{probe}/{name}/pulls"));
        let run = c.run_id.clone();
        async move {
            let number = open_pull(c, &pulls, &jwt, &branch, &format!("slo close {run}")).await?;
            post(c, &format!("{pulls}/{number}/close"), &jwt, Value::Null)
                .await
                .context("could not close the change")?;
            // 409 from `merge_pull` — "this change is not open" — and nothing else. Not in
            // `refused`'s set: this is a state conflict, not an authorization refusal, and the two
            // must not share a helper that would let a 403 here read as a pass. A 400 was in this
            // set too, which is the api's answer to a body it could not read at all: a merge
            // request the tier never understood would have passed as a refusal it never made.
            let url = format!("{pulls}/{number}/merge?strategy=fast-forward");
            let (status, body) = raw(c, reqwest::Method::POST, &url, &jwt, None, &[]).await?;
            match status.as_u16() {
                409 => Ok(()),
                _ => Err(anyhow!("merging a closed change answered {status}: {}", clip(&body))),
            }
        }
        .boxed()
    })
    .await;
}


pub(crate) async fn open_pull(c: &Ctx, pulls: &str, jwt: &str, branch: &str, title: &str) -> Result<i64> {
    let body = serde_json::json!({ "title": title, "body": "", "base": BASE_BRANCH, "head": branch });
    let out = post(c, pulls, jwt, body).await.context("could not open the change")?;
    out.get("number").and_then(Value::as_i64).ok_or_else(|| anyhow!("the answer carried no number"))
}


/// `commit.verify`: the signature endpoint answers for a real commit.
///
/// Any verdict passes — the probe's commits are unsigned, and `unsigned` is the honest answer for
/// them. What is measured is that the endpoint ANSWERS, and inside a second: it reads the commit
/// out of the odb on the owning node, so a hung or fenced handle shows up here first.
pub(crate) async fn verify(c: &mut Ctx) {
    let Some(name) = c.state.repo.clone() else {
        return c.skip("commit.verify", "no repo");
    };
    let Some(oid) = branch_oid(c, &name, BASE_BRANCH).await else {
        return c.skip("commit.verify", "nothing was pushed");
    };
    c.step("commit.verify", QUICK, move |c| {
        let probe = c.probe_user.clone();
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/api/{probe}/{name}/signature/{oid}"));
        async move { get(c, &url, &jwt).await.map(|_| ()) }.boxed()
    })
    .await;
}

// ── shared ──────────────────────────────────────────────────────────────────


/// A refusal, and only a refusal: 401, 403 or 404. A 5xx, a timeout or a success are all the thing
/// the check exists to catch, so none of them may pass — the rule every refusal step in this probe
/// is written to (`deploy/slo.md`'s security section, and `sec.*` in stage 9).
pub(crate) async fn refused(c: &Ctx, method: reqwest::Method, url: &str, token: &str, what: &str) -> Result<()> {
    let (status, body) = raw(c, method, url, token, None, &[]).await?;
    if matches!(status.as_u16(), 401 | 403 | 404) {
        return Ok(());
    }
    Err(anyhow!("{what} answered {status}: {}", clip(&body)))
}
