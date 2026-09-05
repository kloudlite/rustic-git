//! Stage 14's remaining verbs: the twenty ids the 2026-09-05 coverage review found unprobed.
//!
//! A sibling of `experience.rs` like `experience_ws`, `experience_teams`, `experience_env` and
//! `experience_admin` — one file per group of ids, because the stage is one catalogue and several
//! hands. This one is the batch that batch review named, kept together rather than scattered
//! across the four existing files so a reader can hold the review and the code side by side.
//!
//! Three of the review's proposals are implemented differently from its wording, and each is a
//! finding about the code rather than a shortcut:
//!
//! 1. **`id.username` cannot claim a free handle.** `claim_username` RENAMES the caller
//!    (`crates/api/src/teams.rs`), and the probe's tenant already has one — a step that claimed a
//!    fresh handle would rename `slo-probe` and break every later run. So the id measures the two
//!    refusals the route owes: a taken handle is a 409 and a malformed one a 400.
//! 2. **`id.profile.upsert` cannot upsert.** `POST /v1/users` is peer-only BECAUSE it mints a
//!    session, and the probe holds no peer secret. The invariant worth an SLO is exactly that: a
//!    session token must not be able to renew itself for as long as its holder likes.
//! 3. **`req.decide.kinds` brings its own team.** An access approval grants the ASKER membership
//!    of `access.team`, and `team.delete` has already taken the stage's own team by the time this
//!    runs — so this makes and takes back a team of its own rather than depending on the order.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;
use serde_json::{json, Value};

use super::git::BASE_BRANCH;
use super::{admin, api, call, get, poll_json, post, raw};
use crate::ctx::Ctx;
use crate::drill::{undoing, UNDO_SLACK};
use crate::tools;

/// Per-step ceilings, each at or above its catalogue target for the reason every other stage
/// states: a slow answer must be a breach with a number, never a step the probe cut off.
const QUICK: Duration = Duration::from_secs(20);
const READ_CEILING: Duration = Duration::from_secs(15);
const KEY_BODY: Duration = Duration::from_secs(60);
const KEY_CEILING: Duration = Duration::from_secs(KEY_BODY.as_secs() + UNDO_SLACK);
const MERGE_CEILING: Duration = Duration::from_secs(300);
const MERGEABILITY_CEILING: Duration = Duration::from_secs(60);
const TEAM_ENV_CEILING: Duration = Duration::from_secs(240);
const ATTACH_PAIR_CEILING: Duration = Duration::from_secs(120);
const ADMIN_ENV_CEILING: Duration = Duration::from_secs(180);
const ADMIN_DELETE_CEILING: Duration = Duration::from_secs(180);
const DECIDE_CEILING: Duration = Duration::from_secs(90);
const SSHCONFIG_CEILING: Duration = Duration::from_secs(45);

/// Every admin write carries a note onto its audit row; an empty one is a 422.
const NOTE: &str = "slo probe";

const QUOTA_GB: u64 = 1;

// ── identity ────────────────────────────────────────────────────────────────

/// `id.username`: the one irreversible identity write, measured by its refusals.
///
/// See the module header: claiming a FREE handle would rename this tenant, so the two things the
/// route owes are what is asked of it — a handle somebody holds is refused, and one that breaks
/// `check_handle` is rejected before it reaches the directory at all. Both, because either alone
/// would pass against a route that refused everything.
pub(super) async fn username(c: &mut Ctx) {
    let probe = c.probe_user.clone();
    c.step("id.username", QUICK, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, "/v1/users/username");
        async move {
            // Taken: this tenant's own handle, which it is certainly holding.
            let (status, body) =
                raw(c, reqwest::Method::POST, &url, &jwt, Some(json!({ "username": probe })), &[]).await?;
            if status != reqwest::StatusCode::CONFLICT {
                return Err(anyhow!("claiming a taken handle answered {status}: {}", clip(&body)));
            }
            // Malformed: an uppercase leading dash is refused by `check_handle`, and a 400 rather
            // than a 409 is what says the shape check ran before the directory was asked.
            let (status, body) =
                raw(c, reqwest::Method::POST, &url, &jwt, Some(json!({ "username": "-Not A Handle-" })), &[]).await?;
            match status.as_u16() {
                400 => Ok(()),
                other => Err(anyhow!("claiming a malformed handle answered {other}: {}", clip(&body))),
            }
        }
        .boxed()
    })
    .await;
}

/// `id.profile.upsert`: the sign-in write, and the reason a session cannot make one.
///
/// `POST /v1/users` mints a session token, so a session presenting itself there would be a token
/// that renews itself forever — `peer_only` is what stops that, and this is the id that says so.
pub(super) async fn profile_upsert(c: &mut Ctx) {
    let probe_email = c.probe_email.clone();
    c.step("id.profile.upsert", READ_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, "/v1/users");
        async move {
            let body = json!({ "email": probe_email, "name": "kloudlite slo probe" });
            let (status, text) = raw(c, reqwest::Method::POST, &url, &jwt, Some(body), &[]).await?;
            match status.as_u16() {
                401 | 403 => Ok(()),
                // A 2xx here is the failure the route's own comment names: a leaked session token
                // renewing itself for as long as its holder likes.
                other => Err(anyhow!("the session-minting upsert answered {other} to a session token: {}", clip(&text))),
            }
        }
        .boxed()
    })
    .await;
}

/// `id.cli.tokens`: the CLI's own credential list, and a revoked one being refused.
///
/// A token of its OWN, walked through the whole device-code handshake: `id.cli.flow` discards the
/// value it collects (a live credential must not sit in `State`), and revoking the run's other CLI
/// token would leave `id.cli.sshconfig` below with nothing to log in as.
pub(super) async fn cli_tokens(c: &mut Ctx) {
    let device = format!("{}-rev", c.prefix());
    c.step("id.cli.tokens", QUICK, move |c| {
        let jwt = c.probe_jwt.clone();
        async move {
            let (token, id) = cli_login(c, &jwt, &device).await?;
            // It works BEFORE the revoke, or the refusal below says nothing: a token that never
            // authenticated anything is refused whether or not revocation works.
            get(c, &api(c, "/v1/repos"), &token).await.context("a fresh CLI token was not honoured")?;
            let listed = get(c, &api(c, "/v1/cli/tokens"), &jwt).await.context("could not list the CLI tokens")?;
            let there = listed.as_array().is_some_and(|rows| {
                rows.iter().any(|r| r.get("id").and_then(Value::as_str) == Some(id.as_str()))
            });
            if !there {
                return Err(anyhow!("the CLI token was minted but is not listed"));
            }
            call(c, reqwest::Method::DELETE, &api(c, &format!("/v1/cli/tokens/{id}")), &jwt, None)
                .await
                .context("could not revoke the CLI token")?;
            let (status, _) = raw(c, reqwest::Method::GET, &api(c, "/v1/repos"), &token, None, &[]).await?;
            match status.as_u16() {
                401 | 403 => Ok(()),
                other => Err(anyhow!("a revoked CLI token answered {other}")),
            }
        }
        .boxed()
    })
    .await;
}

/// The whole `kl login` handshake, answering `(token, id)`. The device name carries the run
/// prefix, which is teardown's only handle on what this mints.
async fn cli_login(c: &Ctx, jwt: &str, device: &str) -> Result<(String, String)> {
    let started = post(c, &api(c, "/v1/cli/code"), "", json!({ "device": device }))
        .await
        .context("the login handshake was refused")?;
    let code = started.get("code").and_then(Value::as_str).unwrap_or_default().to_string();
    let poll = started.get("poll").and_then(Value::as_str).unwrap_or_default().to_string();
    if code.is_empty() || poll.is_empty() {
        return Err(anyhow!("the login handshake answered no code"));
    }
    post(c, &api(c, "/v1/cli/approve"), jwt, json!({ "code": code })).await.context("the approval was refused")?;
    let out = get(c, &api(c, &format!("/v1/cli/token?poll={poll}")), "").await.context("the token collection failed")?;
    let token = out
        .get("token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow!("the approved login handed back no token"))?
        .to_string();
    let rows = get(c, &api(c, "/v1/cli/tokens"), jwt).await.context("could not list the CLI tokens")?;
    let id = rows
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r.get("name").and_then(Value::as_str) == Some(device))
        .and_then(|r| r.get("id").and_then(Value::as_str))
        .ok_or_else(|| anyhow!("the minted CLI token is not listed by name"))?
        .to_string();
    Ok((token, id))
}

/// `id.cli.sshconfig`: `kl ws sshconfig` writes a block a person's `ssh` can use.
///
/// The real binary, against a real login, with `HOME` and `KL_CONFIG_DIR` pointed at the run's tmp
/// tree — the pod's root filesystem is read-only and the command writes `~/.ssh/config`. The
/// assertion is the rendered block, not the exit code: `render` skips a workspace whose name it
/// will not put in a config, so a run where every workspace was skipped exits zero having written
/// a file with nothing in it.
pub(super) async fn sshconfig(c: &mut Ctx) {
    let Some(ws) = c.state.ux_workspace.clone().or_else(|| c.state.workspace.clone()) else {
        return c.skip("id.cli.sshconfig", "no workspace to write a host block for");
    };
    let device = format!("{}-cli", c.prefix());
    let home = c.tmp.join("klhome");
    c.step("id.cli.sshconfig", SSHCONFIG_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let (kl, api_url) = (c.programs.kl.clone(), c.cfg.api_url.clone());
        let probe = c.probe_user.clone();
        async move {
            let (token, id) = cli_login(c, &jwt, &device).await?;
            // Revoked whatever the rest said, and outside the cancellable region: a CLI token is a
            // 30-day credential, and one per hour that nobody takes back is the leak `KINDS`'
            // `cli-token` entry exists to avoid.
            let revoke = || async {
                call(c, reqwest::Method::DELETE, &api(c, &format!("/v1/cli/tokens/{id}")), &jwt, None)
                    .await
                    .map(|_| ())
                    .context("the CLI token was left LIVE")
            };
            let body = async {
                let dir = home.join(".config/kl");
                std::fs::create_dir_all(&dir).with_context(|| format!("could not make {}", dir.display()))?;
                // Exactly what `kl login` stores, so the command has nothing to do but read it.
                let cfg = json!({
                    "api": api_url,
                    "token": token,
                    "expires_at": "2099-01-01T00:00:00Z",
                    "username": probe,
                });
                std::fs::write(dir.join("config.json"), cfg.to_string()).context("could not stage the CLI login")?;
                let env = std::collections::HashMap::from([
                    ("HOME".to_string(), home.display().to_string()),
                    ("KL_CONFIG_DIR".to_string(), dir.display().to_string()),
                ]);
                tools::run(&kl, &["ws".to_string(), "sshconfig".into()], &env, None, SSHCONFIG_CEILING)
                    .await
                    .context("`kl ws sshconfig` failed")?;
                let block = std::fs::read_to_string(home.join(".ssh/kloudlite_config"))
                    .context("no ~/.ssh/kloudlite_config was written")?;
                has_host_block(&block, &ws)
            };
            undoing(SSHCONFIG_CEILING - Duration::from_secs(10), body, revoke).await
        }
        .boxed()
    })
    .await;
}

/// The rendered block names this workspace and gives ssh the proxy that reaches it.
///
/// A pure function so the one judgement the id turns on is testable without a CLI: a file with a
/// header and no host block is what a run whose workspaces were all skipped writes, and it is
/// exactly what an exit-code check would call a pass.
fn has_host_block(block: &str, id: &str) -> Result<()> {
    let hostname = format!("HostName {id}");
    let proxy = format!("ProxyCommand kl ws proxy {id}");
    if !block.contains(&hostname) {
        return Err(anyhow!("the ssh config carries no block for {id}"));
    }
    if !block.contains(&proxy) {
        return Err(anyhow!("{id}'s block has no ProxyCommand through the gateway"));
    }
    Ok(())
}

/// `key.ssh.lifecycle`: a key a person adds works, and stops working when they take it away.
///
/// A THROWAWAY key, never the probe's mounted one: removing that would take `ssh.clone.ok` and
/// `git.push.ssh` down with it for the rest of the run. `ls-remote` is the smallest thing that
/// needs authentication and leaves nothing on disk, and only `Permission denied` counts as the
/// refusal — a DNS failure or a host-key mismatch makes `ssh` fail too, and reading those as "the
/// key was withdrawn" would keep this green through the outage it exists to catch.
pub(super) async fn key_lifecycle(c: &mut Ctx) {
    let Some(repo) = c.state.repo.clone() else {
        return c.skip("key.ssh.lifecycle", "no repo to clone");
    };
    let hosts = match super::git::known_hosts(c).await {
        Ok(p) => p,
        Err(e) => return c.skip("key.ssh.lifecycle", &format!("{e:#}")),
    };
    let key = c.tmp.join("lifecycle-key");
    let _ = std::fs::remove_file(&key);
    let _ = std::fs::remove_file(key.with_extension("pub"));
    let made = tools::plain(
        &c.programs.ssh_keygen,
        &["-q", "-t", "ed25519", "-N", "", "-C", "slo lifecycle", "-f", &key.display().to_string()],
        Duration::from_secs(20),
    )
    .await;
    if let Err(e) = made {
        return c.skip("key.ssh.lifecycle", &format!("no throwaway key: {e:#}"));
    }
    let public = match tools::plain(&c.programs.ssh_keygen, &["-y", "-f", &key.display().to_string()], Duration::from_secs(10)).await {
        Ok(p) => p.trim().to_string(),
        Err(e) => return c.skip("key.ssh.lifecycle", &format!("could not read the throwaway key: {e:#}")),
    };
    let probe = c.probe_user.clone();
    let name = format!("{}-lifecycle", c.prefix());
    c.step("key.ssh.lifecycle", KEY_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = ssh_remote(c, &repo);
        let cmd = super::git::ssh_command(c, &key.display().to_string(), &hosts);
        let mut env = super::git::git_env(c);
        env.insert("GIT_SSH_COMMAND".into(), cmd);
        let git_bin = c.programs.git.clone();
        let keys = api(c, "/v1/keys");
        async move {
            let added = post(c, &keys, &jwt, json!({ "owner": probe, "name": name, "key": public }))
                .await
                .context("could not add the key")?;
            let id = added
                .pointer("/_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("the answer carried no key id"))?
                .to_string();
            // The REMOVE is both the compensation and the second half of the SLI, so it runs
            // outside the cancellable region and the refusal is checked after it: a key the probe
            // left behind is a standing credential for this account.
            let forget = || async {
                call(c, reqwest::Method::DELETE, &api(c, &format!("/v1/keys/{id}")), &jwt, None)
                    .await
                    .map(|_| ())
                    .context("the throwaway key was left REGISTERED")
            };
            let argv = vec!["ls-remote".to_string(), url];
            let clones = async {
                // The `authorized_keys` view is rebuilt from the directory rows, so a fresh key is
                // usable within a beat rather than instantly.
                ssh_works(&git_bin, &argv, &env, KEY_BODY - Duration::from_secs(20)).await
            };
            undoing(KEY_BODY, clones, forget).await?;
            match tools::run(&git_bin, &argv, &env, None, Duration::from_secs(30)).await {
                Ok(_) => Err(anyhow!("a removed key could still read the repo")),
                Err(e) => {
                    let detail = format!("{e:#}");
                    if detail.contains("Permission denied") {
                        Ok(())
                    } else {
                        Err(anyhow!("ssh failed for some other reason than a refusal: {detail}"))
                    }
                }
            }
        }
        .boxed()
    })
    .await;
}

/// The SSH clone URL for one of the probe's own repos.
fn ssh_remote(c: &Ctx, repo: &str) -> String {
    super::git::ssh_url(c, repo)
}

/// Retry `ls-remote` until the newly added key is honoured.
async fn ssh_works(
    git: &str,
    argv: &[String],
    env: &std::collections::HashMap<String, String>,
    cap: Duration,
) -> Result<()> {
    let start = std::time::Instant::now();
    let mut why;
    loop {
        match tools::run(git, argv, env, None, Duration::from_secs(30)).await {
            Ok(_) => return Ok(()),
            Err(e) => why = format!("{e:#}"),
        }
        if start.elapsed() >= cap {
            return Err(anyhow!("a newly added key never cloned after {} ms: {why}", cap.as_millis()));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

// ── git and pull requests ───────────────────────────────────────────────────

/// `repo.description`: saved, and read back off the repo.
///
/// The read-back is the SLI: `update_repo` forwards the description to the OWNING node and answers
/// 204 either way, so a 204 whose text nobody can then read is the failure a person sees as "my
/// change did not stick".
pub(super) async fn description(c: &mut Ctx) {
    let Some(name) = c.state.repo.clone() else {
        return c.skip("repo.description", "no repo");
    };
    let probe = c.probe_user.clone();
    let want = format!("slo probe {}", c.run_id);
    c.step("repo.description", READ_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/repos/{probe}/{name}"));
        async move {
            call(c, reqwest::Method::PATCH, &url, &jwt, Some(json!({ "description": want })))
                .await
                .context("could not save the description")?;
            let seen = get(c, &url, &jwt).await.context("could not read the repo back")?;
            match seen.get("description").and_then(Value::as_str) {
                Some(d) if d == want => Ok(()),
                other => Err(anyhow!("the repo reads back {other:?}, not the description just saved")),
            }
        }
        .boxed()
    })
    .await;
}

/// `pr.merge.strategies`: all four, each on its own change, each judged on the TREE it left.
///
/// Only one strategy was ever probed and `merge_worker.rs` implements four; squash's retry guard
/// (merged-tree == base-tree, not ancestry) and rebase's throwaway worktree are the two paths that
/// have actually broken. Every strategy gets a branch of its own with a file only it writes, so
/// "it landed" is that file being on `main` afterwards — a merge that reported success and moved
/// nothing, or moved the wrong thing, fails here rather than passing on a status.
///
/// One step, first failure wins: "the merge button works" is false if any of the four is broken,
/// and four ids would let one dead strategy sit at 75 % attainment looking mostly healthy.
pub(super) async fn merge_strategies(c: &mut Ctx) {
    let Some(name) = c.state.repo.clone() else {
        return c.skip("pr.merge.strategies", "no repo");
    };
    let probe = c.probe_user.clone();
    c.step("pr.merge.strategies", MERGE_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let commits = api(c, &format!("/v1/repos/{probe}/{name}/commits"));
        let pulls = api(c, &format!("/v1/repos/{probe}/{name}/pulls"));
        let files = api(c, &format!("/api/{probe}/{name}/refs"));
        let blobs = api(c, &format!("/api/{probe}/{name}/blob"));
        let run = c.run_id.clone();
        async move {
            for strategy in STRATEGIES {
                one_strategy(c, &commits, &pulls, &files, &blobs, &jwt, &run, strategy)
                    .await
                    .with_context(|| format!("the {strategy} strategy"))?;
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// The four `merge_worker.rs` implements, in the order the web offers them.
const STRATEGIES: [&str; 4] = ["merge", "squash", "rebase", "fast-forward"];

/// One strategy: a branch with a file of its own, a change, the merge, and the file on `main`.
///
/// The branch is cut by the web's own commit endpoint rather than by git, so this needs no working
/// tree — and a fast-forward is only offered when the branch has not diverged, which is why each
/// runs to completion before the next one starts.
#[allow(clippy::too_many_arguments)]
async fn one_strategy(
    c: &Ctx,
    commits: &str,
    pulls: &str,
    refs: &str,
    blobs: &str,
    jwt: &str,
    run: &str,
    strategy: &str,
) -> Result<()> {
    use base64::Engine as _;
    let branch = format!("run-{run}-{strategy}");
    let path = format!("{strategy}.txt");
    let content = base64::engine::general_purpose::STANDARD.encode(format!("{run} {strategy}\n"));
    let body = json!({
        "branch": BASE_BRANCH,
        "newBranch": branch,
        "message": format!("slo {strategy} {run}"),
        "changes": [{ "path": path, "contentBase64": content }],
    });
    post(c, commits, jwt, body).await.context("could not make the branch")?;
    let opened = post(
        c,
        pulls,
        jwt,
        json!({ "title": format!("slo {strategy} {run}"), "base": BASE_BRANCH, "head": branch }),
    )
    .await
    .context("could not open the change")?;
    let number = opened
        .get("number")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("the answer carried no number"))?;
    post(c, &format!("{pulls}/{number}/merge?strategy={strategy}"), jwt, Value::Null)
        .await
        .context("the merge was refused")?;
    poll_json(c, &format!("{pulls}/{number}"), jwt, Duration::from_secs(60), |p| {
        p.get("state").and_then(Value::as_str) == Some("merged")
    })
    .await
    .context("the change never reached `merged`")?;
    // The TREE, not the status. `merged` is the record the worker wrote; what the SLI promises is
    // that the strategy LANDED the expected tree, and the four strategies build that tree in four
    // different ways (a merge commit, a squashed one, a replayed one, a moved ref). So the file
    // this branch alone wrote is read back off `main` and its CONTENT compared: a merge that
    // answered 202 and moved nothing, or replayed the wrong branch, gets no further than here.
    let want = format!("{run} {strategy}\n");
    landed_on_main(c, refs, blobs, jwt, &path, &want).await
}

/// The file `path` as `main` now has it, compared to what the branch wrote.
///
/// Polled rather than read once: the merge is recorded by the owner and the refs move with it, so
/// a read that raced the ref update would report the base branch's old tree and blame the strategy.
async fn landed_on_main(
    c: &Ctx,
    refs: &str,
    blobs: &str,
    jwt: &str,
    path: &str,
    want: &str,
) -> Result<()> {
    use base64::Engine as _;
    let start = std::time::Instant::now();
    let mut why;
    loop {
        let seen = async {
            let r = get(c, refs, jwt).await.context("could not read the refs")?;
            let head = super::git::oid_of(&r, BASE_BRANCH)
                .ok_or_else(|| anyhow!("`{BASE_BRANCH}` has no tip"))?;
            let blob = get(c, &format!("{blobs}/{head}/{path}"), jwt)
                .await
                .with_context(|| format!("`{path}` is not on `{BASE_BRANCH}` at {head}"))?;
            let b64 = blob
                .get("bytes_base64")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("the blob answer carried no bytes"))?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .context("the blob is not base64")?;
            let got = String::from_utf8_lossy(&bytes).to_string();
            if got == want {
                return Ok(());
            }
            Err(anyhow!("`{path}` on `{BASE_BRANCH}` holds {got:?}, not {want:?}"))
        }
        .await;
        match seen {
            Ok(()) => return Ok(()),
            Err(e) => why = format!("{e:#}"),
        }
        if start.elapsed() >= MERGE_LAND {
            return Err(anyhow!("the merge did not land the expected tree: {why}"));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// How long one strategy's tree has to appear on `main` after its change reports `merged`.
const MERGE_LAND: Duration = Duration::from_secs(30);

/// `pr.mergeability`: the answer the web's merge button is drawn from.
///
/// Both verdicts, because either alone is worthless: a fleet that answered `clean` to everything
/// would offer a merge that fails at the click, and one that answered `dirty` to everything would
/// hide the button on every change. The conflicting branch is built by writing the SAME path from
/// the same base twice, which is the one shape `merge-tree` cannot combine on its own.
pub(super) async fn mergeability(c: &mut Ctx) {
    let Some(name) = c.state.repo.clone() else {
        return c.skip("pr.mergeability", "no repo");
    };
    let probe = c.probe_user.clone();
    c.step("pr.mergeability", MERGEABILITY_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let commits = api(c, &format!("/v1/repos/{probe}/{name}/commits"));
        let pulls = api(c, &format!("/v1/repos/{probe}/{name}/pulls"));
        let run = c.run_id.clone();
        async move {
            use base64::Engine as _;
            let enc = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
            // A file only this run touches, so nothing else in the journey can make the two agree.
            let path = format!("conflict-{run}.txt");
            // The first branch lands on `main`, which is what makes the second one a conflict
            // rather than two branches nobody has combined.
            let a = format!("run-{run}-clean");
            branch_with(c, &commits, &jwt, &a, &path, &enc("one\n"), &format!("slo clean {run}")).await?;
            let clean = open(c, &pulls, &jwt, &a, &format!("slo clean {run}")).await?;
            verdict(c, &pulls, &jwt, clean, "clean").await?;
            post(c, &format!("{pulls}/{clean}/merge?strategy=fast-forward"), &jwt, Value::Null)
                .await
                .context("could not land the clean change")?;
            poll_json(c, &format!("{pulls}/{clean}"), &jwt, Duration::from_secs(60), |p| {
                p.get("state").and_then(Value::as_str) == Some("merged")
            })
            .await
            .context("the clean change never merged, so nothing can conflict with it")?;
            // The second branch was cut from the base BEFORE that merge and writes the same path,
            // which is the one shape a trial merge cannot combine.
            let b = format!("run-{run}-dirty");
            branch_with(c, &commits, &jwt, &b, &path, &enc("two\n"), &format!("slo dirty {run}")).await?;
            let dirty = open(c, &pulls, &jwt, &b, &format!("slo dirty {run}")).await?;
            verdict(c, &pulls, &jwt, dirty, "dirty").await
        }
        .boxed()
    })
    .await;
}

/// A new branch off `main` carrying one file, through the web's own commit endpoint.
async fn branch_with(
    c: &Ctx,
    commits: &str,
    jwt: &str,
    branch: &str,
    path: &str,
    content: &str,
    message: &str,
) -> Result<()> {
    let body = json!({
        "branch": BASE_BRANCH,
        "newBranch": branch,
        "message": message,
        "changes": [{ "path": path, "contentBase64": content }],
    });
    post(c, commits, jwt, body).await.map(|_| ()).with_context(|| format!("could not make {branch}"))
}

async fn open(c: &Ctx, pulls: &str, jwt: &str, branch: &str, title: &str) -> Result<i64> {
    let body = json!({ "title": title, "base": BASE_BRANCH, "head": branch });
    let out = post(c, pulls, jwt, body).await.context("could not open the change")?;
    out.get("number").and_then(Value::as_i64).ok_or_else(|| anyhow!("the answer carried no number"))
}

/// Wait for the owner's own mergeability verdict to be the one asked for.
///
/// `unknown` is "not worked out yet", so the wait is for a REAL verdict and the comparison is what
/// judges it — reading `unknown` as either answer would make this pass on a fleet whose checker
/// never ran at all.
async fn verdict(c: &Ctx, pulls: &str, jwt: &str, number: i64, want: &str) -> Result<()> {
    let url = format!("{pulls}/{number}");
    let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let last = seen.clone();
    poll_json(c, &url, jwt, Duration::from_secs(25), move |p| {
        let state = p.pointer("/mergeability/state").and_then(Value::as_str).unwrap_or("unknown");
        *last.lock().expect("lock") = state.to_string();
        state != "unknown"
    })
    .await
    .with_context(|| format!("change {number}'s mergeability was never worked out"))?;
    let got = seen.lock().expect("lock").clone();
    if got == want {
        return Ok(());
    }
    Err(anyhow!("change {number} reports `{got}`, not `{want}`"))
}

// ── teams and environments ──────────────────────────────────────────────────

/// `team.invite.revoke`: a revoked invitation cannot be redeemed.
///
/// Its own invitation, never `team.invite.accept`'s: that one is SPENT by the time this runs, and
/// a spent token is refused whether or not revocation works at all. The token is never formatted
/// into a detail — `raw` carries the status and the body, never the URL.
pub(super) async fn invite_revoke(c: &mut Ctx) {
    let slug = super::experience_teams::team_slug(c);
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
            call(c, reqwest::Method::DELETE, &api(c, &format!("{invites}/{id}")), &jwt, None)
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
pub(super) async fn team_environment(c: &mut Ctx) {
    if c.kube.is_none() {
        return c.skip("team.environment", "no kubeconfig");
    }
    let slug = super::experience_teams::team_slug(c);
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
async fn team_env_ready(c: &Ctx, id: &str, one: &str, jwt: &str) -> Result<()> {
    poll_json(c, one, jwt, TEAM_ENV_CEILING - Duration::from_secs(60), |v| {
        v.get("state").and_then(Value::as_str) == Some("running")
    })
    .await
    .context("the team environment never reported running")?;
    let k = c.kube.as_ref().ok_or_else(|| anyhow!("no kubeconfig"))?;
    let ns = kloudlite_workspaces::crd::env_namespace(id);
    let (code, out, err) = crate::kube::exec(
        k,
        &ns,
        "redis-0",
        None,
        &["sh", "-c", "getent hosts redis >/dev/null && redis-cli -h redis ping"],
        Duration::from_secs(20),
    )
    .await?;
    if code != 0 || !out.trim().eq_ignore_ascii_case("pong") {
        return Err(anyhow!("the service in {ns} does not resolve and answer: {}", err.trim()));
    }
    Ok(())
}

/// `env.attach.pair`: deleting an attached workspace takes the ENVIRONMENT-side policy with it.
///
/// `env.attach`/`env.detach` only ever exercise the workspace side. The environment-side
/// `attach-{ws}` NetworkPolicy lives in the environment's namespace and cannot carry an
/// ownerReference across it, so `delete_ws` removes it BY HAND while the spec is still readable —
/// which is precisely the kind of hand-written cleanup that stops happening unnoticed. The policy
/// is read from Kubernetes rather than inferred: there is no API that reports one.
pub(super) async fn attach_pair(c: &mut Ctx) {
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
async fn netpol_is(k: &kube::Client, ns: &str, name: &str, want: bool, cap: Duration) -> Result<()> {
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

/// `vol.list`: the listing names the volumes this run is holding.
///
/// `vol.history` reads one volume's chain; nothing read the LIST, which is what the console's own
/// pages are drawn from and the one place a volume with no working copy is visible at all. Matched
/// on `display_name`, because a volume's `name` is the ws/env id and carries no run prefix —
/// exactly the trap that made teardown's sweep miss every probe volume.
pub(super) async fn vol_list(c: &mut Ctx) {
    let prefix = c.prefix();
    c.step("vol.list", READ_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, "/v1/volumes");
        async move {
            let rows = get(c, &url, &jwt).await.context("could not list the volumes")?;
            let rows = rows.as_array().ok_or_else(|| anyhow!("the volume list is not a list"))?;
            let mine = rows
                .iter()
                .filter(|r| {
                    r.get("display_name").and_then(Value::as_str).is_some_and(|n| n.starts_with(&prefix))
                })
                .count();
            if mine == 0 {
                return Err(anyhow!("the listing names none of the {} volumes this run holds", rows.len()));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

// ── admin ───────────────────────────────────────────────────────────────────

/// `admin.stop.environment`: `admin.stop.workspace`'s twin.
///
/// The OWNER's own read is what says it happened, exactly as the workspace one does: an admin
/// route that only satisfies itself proves nothing about what the person whose environment it was
/// can see.
pub(super) async fn admin_stop_environment(c: &mut Ctx) {
    let Some(env) = c.state.env_multi.clone() else {
        return c.skip("admin.stop.environment", "no environment to stop");
    };
    c.step("admin.stop.environment", ADMIN_ENV_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let admin_jwt = c.admin_jwt.clone();
        let one = api(c, &format!("/v1/environments/{env}"));
        let stop = admin(c, &format!("/admin/environments/{env}/stop"));
        async move {
            // Running first: stopping something already stopped answers 2xx and measures nothing.
            poll_json(c, &one, &jwt, Duration::from_secs(120), |v| {
                v.get("state").and_then(Value::as_str) == Some("running")
            })
            .await
            .context("the environment was not running to begin with")?;
            post(c, &stop, &admin_jwt, json!({ "note": NOTE })).await.context("the admin stop was refused")?;
            poll_json(c, &one, &jwt, Duration::from_secs(45), |v| {
                v.get("state").and_then(Value::as_str) == Some("stopped")
            })
            .await
            .context("the owner's own read never showed it stopped")
        }
        .boxed()
    })
    .await;
}

/// `admin.delete.workload`: the console's own deletes, on objects of the probe's own making.
///
/// Both kinds in one step: the two admin handlers are separate code paths over the same finalizer,
/// and either one silently doing nothing is the same failure — an operator who thinks they have
/// taken something away and has not.
pub(super) async fn admin_delete(c: &mut Ctx) {
    let name = format!("{}-ad", c.prefix());
    c.step("admin.delete.workload", ADMIN_DELETE_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let admin_jwt = c.admin_jwt.clone();
        let region = c.cfg.region.clone();
        let workspaces = api(c, "/v1/workspaces");
        let environments = api(c, "/v1/environments");
        async move {
            let ws = json!({ "name": format!("{name}-w"), "region": region, "quota_gb": QUOTA_GB, "packages": [] });
            let ws = id_of(post(c, &workspaces, &jwt, ws).await.context("could not create a workspace to delete")?)?;
            let env = json!({
                "name": format!("{name}-e"),
                "region": region,
                "quota_gb": QUOTA_GB,
                "services": [{ "name": "redis", "image": "redis:7-alpine", "command": [], "env": {}, "mounts": [], "ports": [6379] }],
            });
            let env = id_of(post(c, &environments, &jwt, env).await.context("could not create an environment to delete")?)?;
            // No wait for `ready`: the delete is the SLI and a create that is still converging is
            // deleted the same way. Both are named `run-…`, so teardown finds either one anyway.
            for (kind, id, path) in [
                ("workspace", ws, "workspaces"),
                ("environment", env, "environments"),
            ] {
                let del = admin(c, &format!("/admin/{path}/{id}"));
                call(c, reqwest::Method::DELETE, &del, &admin_jwt, Some(json!({ "note": NOTE })))
                    .await
                    .with_context(|| format!("the admin delete of the {kind} was refused"))?;
                // The OWNER's read, like the stop above: the admin route agreeing with itself is
                // not the same as the object being gone.
                let one = api(c, &format!("/v1/{path}/{id}"));
                let start = std::time::Instant::now();
                loop {
                    let (status, _) = raw(c, reqwest::Method::GET, &one, &jwt, None, &[]).await?;
                    if status == reqwest::StatusCode::NOT_FOUND {
                        break;
                    }
                    if start.elapsed() >= Duration::from_secs(60) {
                        return Err(anyhow!("the {kind} still answers {status} to its owner"));
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `admin.screens`: the three console reads nothing else covers.
///
/// One step over three routes, because they are one claim — "the console renders" — and each is a
/// different module's own reader (`admin/owners.rs`, `admin/clusters.rs`, `admin/overview.rs`).
/// The overview is composed from every other module's reader, so it is the one that goes red first
/// when any of them stops answering.
pub(super) async fn screens(c: &mut Ctx) {
    let probe = c.probe_user.clone();
    c.step("admin.screens", READ_CEILING, move |c| {
        let jwt = c.admin_jwt.clone();
        let owners = admin(c, "/admin/owners");
        let owner = admin(c, &format!("/admin/owners/{probe}"));
        let clusters = admin(c, "/admin/clusters");
        let overview = admin(c, "/admin/overview");
        async move {
            // The probe's own owner row, not merely a non-empty list: a listing that answered `[]`
            // is a console screen with nothing on it, and a 200 all the same.
            let rows = get(c, &owners, &jwt).await.context("the owners screen")?;
            let listed = rows.as_array().is_some_and(|rows| {
                rows.iter().any(|r| {
                    [r.get("slug"), r.get("owner"), r.get("name")]
                        .into_iter()
                        .flatten()
                        .any(|v| v.as_str() == Some(probe.as_str()))
                })
            });
            if !listed {
                return Err(anyhow!("the owners screen does not list {probe}"));
            }
            get(c, &owner, &jwt).await.context("the owner detail screen")?;
            get(c, &clusters, &jwt).await.context("the clusters screen")?;
            get(c, &overview, &jwt).await.context("the overview screen")?;
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `admin.workloads.read`: the roll-target list the infrastructure tab and every `Mark::Boot` save
/// are drawn from. A save that would roll a reader it cannot see is a save that never lands.
pub(super) async fn workloads(c: &mut Ctx) {
    c.step("admin.workloads.read", READ_CEILING, |c| {
        let jwt = c.admin_jwt.clone();
        let url = admin(c, "/admin/workloads");
        async move {
            let rows = get(c, &url, &jwt).await.context("could not read the workloads")?;
            let rows = rows.get("workloads").and_then(Value::as_array).or_else(|| rows.as_array()).cloned();
            match rows {
                Some(rows) if !rows.is_empty() => Ok(()),
                // Empty is the failure, not a quiet fleet: `KNOWN` is compiled in, so a list with
                // nothing on it means the reader could not see the cluster at all.
                Some(_) => Err(anyhow!("the workloads list names no roll target at all")),
                None => Err(anyhow!("the answer carried no workloads")),
            }
        }
        .boxed()
    })
    .await;
}

/// `audit.export`: the CSV a person downloads off the Audit screen.
///
/// A header AND a row: the export is the object-store log rendered, and an export that answers a
/// header with nothing under it is what a broken reader produces — the fast suite's `audit.row`
/// has already filed at least one row by the time this runs.
pub(super) async fn audit_export(c: &mut Ctx) {
    c.step("audit.export", READ_CEILING, |c| {
        let jwt = c.admin_jwt.clone();
        let url = admin(c, "/admin/audit.csv?limit=50");
        async move {
            let (status, body) = raw(c, reqwest::Method::GET, &url, &jwt, None, &[]).await?;
            if !status.is_success() {
                return Err(anyhow!("{status}: {}", clip(&body)));
            }
            let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
            match lines.as_slice() {
                [] => Err(anyhow!("the export is empty — not even a header")),
                [_] => Err(anyhow!("the export carries a header and no rows")),
                [head, ..] if head.contains(',') => Ok(()),
                _ => Err(anyhow!("the export's first line is not a CSV header")),
            }
        }
        .boxed()
    })
    .await;
}

/// `req.decide.kinds`: an ACCESS approval grants what it says, and a deny closes with its reason.
///
/// Only the quota kind was probed, and approve is kind-specific: access grants team membership
/// through the directory the admin process already holds, and each kind writes its effect BEFORE
/// marking the request. The membership read is what says the effect landed — a request marked
/// `approved` with no grant behind it is the failure that kind exists to avoid.
///
/// Its own team, made and taken back inside the step: see the module header.
pub(super) async fn decide_kinds(c: &mut Ctx) {
    let slug = format!("{}-rq", c.prefix());
    c.step("req.decide.kinds", DECIDE_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let other = c.other_jwt.clone();
        let admin_jwt = c.admin_jwt.clone();
        let team = api(c, &format!("/v1/teams/{slug}"));
        async move {
            post(c, &api(c, "/v1/teams"), &jwt, json!({ "slug": slug, "name": "slo probe requests" }))
                .await
                .context("could not create the team the access request is for")?;
            let drop_team = || async {
                call(c, reqwest::Method::DELETE, &team, &jwt, None)
                    .await
                    .map(|_| ())
                    .context("the request team was left standing")
            };
            let both = async {
                // Approve: the SECOND tenant asks to join, and its own read of the team is what
                // says the grant landed — `/v1/teams/{slug}` answers 404 to a non-member, which is
                // exactly the question.
                let ask = json!({
                    "kind": "access",
                    "reason": format!("{} slo probe access", c.prefix()),
                    "access": { "team": slug, "role": "member" },
                });
                let made = post(c, &api(c, "/v1/requests"), &other, ask).await.context("could not open the access request")?;
                let id = id_of(made)?;
                post(c, &admin(c, &format!("/admin/requests/{id}/approve")), &admin_jwt, json!({ "note": NOTE }))
                    .await
                    .context("the access approval was refused")?;
                poll_json(c, &team, &other, Duration::from_secs(20), |v| {
                    v.get("slug").and_then(Value::as_str) == Some(slug.as_str())
                })
                .await
                .context("the approved access request granted no membership")?;
                // Deny: a second request, of a different kind so the one-pending-per-kind rule
                // does not refuse it, closed with the reason the asker reads.
                let ask = json!({
                    "kind": "other",
                    "reason": format!("{} slo probe deny", c.prefix()),
                    "other": { "title": "slo probe", "body": "deny me" },
                });
                let made = post(c, &api(c, "/v1/requests"), &other, ask).await.context("could not open the request to deny")?;
                let id = id_of(made)?;
                let note = format!("slo probe denied {}", c.run_id);
                post(c, &admin(c, &format!("/admin/requests/{id}/deny")), &admin_jwt, json!({ "note": note }))
                    .await
                    .context("the deny was refused")?;
                let seen = get(c, &api(c, &format!("/v1/requests/{id}")), &other).await.context("the asker cannot read it back")?;
                denied_with(&seen, &note)
            };
            undoing(DECIDE_CEILING - Duration::from_secs(20), both, drop_team).await
        }
        .boxed()
    })
    .await;
}

/// A denied request reads back as denied AND carries the reason. A state with no note is a
/// decision the asker cannot act on, which is the half a status check would miss.
fn denied_with(request: &Value, note: &str) -> Result<()> {
    let state = request.get("state").and_then(Value::as_str).unwrap_or_default();
    if state != "denied" {
        return Err(anyhow!("the request reads back as `{state}`, not denied"));
    }
    let carried = request.to_string();
    if !carried.contains(note) {
        return Err(anyhow!("the denied request carries no reason the asker can read"));
    }
    Ok(())
}

/// `req.legacy.union`: the retired `QuotaRequest` queue is still readable and still migrates.
///
/// `QuotaRequest` is deliberately kept alive — unioned into `GET /admin/requests`, copied over once
/// by `POST /admin/requests/migrate` — until it is deleted as a CRD in a later release. Both are
/// asked, because the union going quiet and the migration failing are the same outcome for whoever
/// filed one: a request nobody will ever see.
pub(super) async fn legacy_union(c: &mut Ctx) {
    c.step("req.legacy.union", READ_CEILING, |c| {
        let jwt = c.admin_jwt.clone();
        let legacy = admin(c, "/admin/quota-requests");
        let migrate = admin(c, "/admin/requests/migrate");
        let queue = admin(c, "/admin/requests");
        async move {
            // Reading the retired queue must ANSWER; an empty list is the ordinary state and is
            // not a failure — the CRD may legitimately hold nothing left to migrate.
            let rows = get(c, &legacy, &jwt).await.context("the retired quota-request queue")?;
            if !rows.is_array() {
                return Err(anyhow!("the retired queue did not answer a list"));
            }
            post(c, &migrate, &jwt, json!({ "note": NOTE })).await.context("the migration was refused")?;
            let unioned = get(c, &queue, &jwt).await.context("the admin queue")?;
            unioned
                .as_array()
                .map(|_| ())
                .ok_or_else(|| anyhow!("the admin queue did not answer a list after the migration"))
        }
        .boxed()
    })
    .await;
}

/// `region.status`: the region list a create offers, and this run's own cluster's status.
///
/// The CREATE is deliberately not here: a `Region` has no delete on any tier — a second POST only
/// retires or renames one — so a probe region would be shared state nobody could ever take back.
/// What every run does need is that the region it is running in is listed and answers, which is
/// the read every workspace create is validated against.
pub(super) async fn region_status(c: &mut Ctx) {
    let region = c.cfg.region.clone();
    c.step("region.status", READ_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let admin_jwt = c.admin_jwt.clone();
        let list = api(c, "/v1/regions");
        let detail = admin(c, &format!("/admin/clusters/{region}"));
        async move {
            let rows = get(c, &list, &jwt).await.context("could not list the regions")?;
            let there = rows.as_array().is_some_and(|rows| {
                rows.iter().any(|r| {
                    [r.get("id"), r.get("name")]
                        .into_iter()
                        .flatten()
                        .any(|v| v.as_str() == Some(region.as_str()))
                })
            });
            if !there {
                return Err(anyhow!("`{region}` — the region this run is in — is not listed"));
            }
            get(c, &detail, &admin_jwt).await.context("the cluster's own status").map(|_| ())
        }
        .boxed()
    })
    .await;
}

// ── shared ──────────────────────────────────────────────────────────────────

fn id_of(doc: Value) -> Result<String> {
    doc.get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("the answer carried no id"))
}

fn clip(body: &str) -> String {
    body.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;

    /// The one judgement `id.cli.sshconfig` turns on. `render` SKIPS a workspace whose name it
    /// will not put in a config and still exits zero, so a file with a header and no host block is
    /// exactly what an exit-code check would call a pass.
    #[test]
    fn the_ssh_config_check_wants_a_real_host_block() {
        let good = "# Managed by kl.\n\nHost run-fast-1\n  HostName ws-abc\n  User kl\n  \
                    ProxyCommand kl ws proxy ws-abc\n  HostKeyAlias ws-abc\n";
        assert!(has_host_block(good, "ws-abc").is_ok());
        // A file the command wrote having skipped every workspace.
        assert!(has_host_block("# Managed by kl.\n", "ws-abc").is_err());
        // Another workspace's block is not this one's.
        assert!(has_host_block(good, "ws-other").is_err());
        // A block with no way to reach the pod is a block ssh cannot use.
        let no_proxy = "Host x\n  HostName ws-abc\n  User kl\n";
        assert!(has_host_block(no_proxy, "ws-abc").is_err());
    }

    /// A denied request that carries no reason is a decision the asker cannot act on — the half a
    /// check on `state` alone would miss.
    #[test]
    fn a_deny_has_to_carry_its_reason() {
        let note = "slo probe denied fast-1";
        assert!(denied_with(&json!({ "state": "denied", "note": note }), note).is_ok());
        assert!(denied_with(&json!({ "state": "denied", "decision": { "note": note } }), note).is_ok());
        assert!(denied_with(&json!({ "state": "denied" }), note).is_err());
        assert!(denied_with(&json!({ "state": "approved", "note": note }), note).is_err());
        assert!(denied_with(&json!({ "state": "pending", "note": note }), note).is_err());
    }

    /// Every id this file owns reports exactly once against a fleet that answers nothing — as a
    /// failure with a reason, or as a skip when its precondition is genuinely absent. A run is
    /// exactly-once complete on every path, which is what lets the console tell a grey stage from
    /// a broken one.
    #[tokio::test]
    async fn every_id_reports_once_with_nothing_reachable() {
        let mut c = testkit::ctx().await;
        c.kube = None;
        username(&mut c).await;
        profile_upsert(&mut c).await;
        cli_tokens(&mut c).await;
        sshconfig(&mut c).await;
        key_lifecycle(&mut c).await;
        description(&mut c).await;
        merge_strategies(&mut c).await;
        mergeability(&mut c).await;
        invite_revoke(&mut c).await;
        team_environment(&mut c).await;
        attach_pair(&mut c).await;
        vol_list(&mut c).await;
        admin_stop_environment(&mut c).await;
        admin_delete(&mut c).await;
        screens(&mut c).await;
        workloads(&mut c).await;
        audit_export(&mut c).await;
        decide_kinds(&mut c).await;
        legacy_union(&mut c).await;
        region_status(&mut c).await;
        for id in IDS {
            assert_eq!(c.steps.iter().filter(|s| s.slo_id == id).count(), 1, "{id}");
        }
        assert_eq!(c.steps.len(), IDS.len(), "an id nobody asked for was reported");
        // Nothing anywhere carries a credential: these steps mint CLI tokens and read invitations.
        for s in &c.steps {
            assert!(!s.detail.contains(&c.probe_jwt), "a jwt reached a detail: {s:?}");
        }
    }

    /// Every id this file owns, which is also the set `experience.rs` dispatches to it.
    const IDS: [&str; 20] = [
        "id.username",
        "id.profile.upsert",
        "id.cli.tokens",
        "id.cli.sshconfig",
        "key.ssh.lifecycle",
        "repo.description",
        "pr.merge.strategies",
        "pr.mergeability",
        "team.invite.revoke",
        "team.environment",
        "env.attach.pair",
        "vol.list",
        "admin.stop.environment",
        "admin.delete.workload",
        "admin.screens",
        "admin.workloads.read",
        "audit.export",
        "req.decide.kinds",
        "req.legacy.union",
        "region.status",
    ];
}
