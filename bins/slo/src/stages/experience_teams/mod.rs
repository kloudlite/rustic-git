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
use super::{api, drain_team, get, poll_json, post, raw, TEAM_DRAIN};
use crate::drill::{undoing, UNDO_SLACK};
use crate::ctx::Ctx;
use kloudlite_workspaces::crd;
use super::{clip};

mod membership;
pub(crate) use membership::*;
mod repos;
pub(crate) use repos::*;


// Per-step ceilings. Each is at least its catalogue target, for the reason stage 5 states: a slow
// answer must be a breach with a number, never a step the probe cut off.

pub(super) const QUICK: Duration = Duration::from_secs(20);
///
/// `team.repo.shared` and `repo.protection` are a BODY cap plus `UNDO_SLACK`: both compensate — a
/// team credential to revoke, a protected `main` to unprotect — and `Ctx::step`'s own timeout
/// drops the whole future, undo included, when it fires. The body runs inside `drill::undoing`
/// under the first, so the step's can never fire first. `team.delete` carries the drain of the
/// team's workspaces, which is `TEAM_DRAIN` on its own.
pub(super) const TEAM_REPO_BODY: Duration = Duration::from_secs(90);

pub(super) const TEAM_REPO_CEILING: Duration = Duration::from_secs(TEAM_REPO_BODY.as_secs() + UNDO_SLACK);

pub(super) const TEAM_WS_CEILING: Duration = Duration::from_secs(120);

pub(super) const DELETE_CEILING: Duration = Duration::from_secs(TEAM_DRAIN.as_secs() + 30);

pub(super) const PROTECTION_BODY: Duration = Duration::from_secs(150);

pub(super) const PROTECTION_CEILING: Duration = Duration::from_secs(PROTECTION_BODY.as_secs() + UNDO_SLACK);
pub(super) const PULL_CEILING: Duration = Duration::from_secs(30);


/// `repo.commit.patch` is bounded at 5 s and `pr.merge.p95` at 60 s; both waits are given their
/// target as the poll cap, so a wait that runs out is the SLO being missed rather than the probe
/// being impatient.
pub(super) const LOG_CAP: Duration = Duration::from_secs(5);
pub(super) const MERGE_CAP: Duration = Duration::from_secs(60);


/// The team workspace asks for the same disk stage 5's does, well inside the compiled-in
/// `default-team` quota a team with no `Quota` object of its own inherits.
pub(super) const QUOTA_GB: u64 = 1;

// ── names ───────────────────────────────────────────────────────────────────
//
// All of them under `run-{run_id}`, which is the whole of teardown's contract — and all of them
// pure functions of the run, which is what lets one id read back what the previous one wrote.


/// `run-{id}-team`. Lowercase, dashes and digits only, and 26 characters for a typical run id:
/// `check_handle` caps a handle at 39.
/// `pub(super)` because the siblings read back what this file wrote — `experience_gaps`'
/// `team.invite.revoke` and `team.environment` both act on THIS team, and a second `format!`
/// there would be a second place the name has to be remembered.
pub(super) fn team_slug(c: &Ctx) -> String {
    format!("{}-team", c.prefix())
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
        namespace_reaped(&mut c).await;

        let made = sample(&c, "team.create");
        assert!(!made.ok && !made.skipped, "the create carries the failure");
        for id in ["team.invite.accept", "team.role.set", "team.repo.shared", "team.workspace", "team.member.remove", "team.delete", "team.namespace.reaped"] {
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
            "members": [{ "email": crate::ctx::email_of(crate::ctx::OTHER_USER), "role": "admin" }],
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
        namespace_reaped(&mut c).await;

        for id in ["team.create", "team.invite.accept", "team.role.set", "team.repo.shared", "team.workspace", "team.member.remove", "team.delete", "team.namespace.reaped"] {
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
        for name in [team_slug(&c), shared_repo(&c), prot_branch(&c), patch_branch(&c), closed_branch(&c)] {
            assert!(name.starts_with(&p), "{name} is not swept by the {p} prefix");
        }
        // A team slug is a handle: `check_handle` caps it at 39 characters and permits only
        // lowercase letters, digits and dashes.
        let slug = team_slug(&c);
        assert!(slug.len() <= 39, "{slug} is too long to be a handle");
        assert!(slug.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'), "{slug}");
        assert!(!slug.starts_with('-') && !slug.ends_with('-'), "{slug}");
    }
}
