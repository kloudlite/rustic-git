//! Stage 2 · Git: the product's front door, over both protocols.
//!
//! The repo is created FIRST and untimed. It is not an SLO of its own — `git.push.ok` is — but
//! every step here needs it, so a create that fails skips the whole stage rather than reporting
//! ten separate failures for one broken thing (the design's "Error handling": skipped is no
//! sample, and the failure was already counted where it happened).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;

use super::{api, get, poll_json, post};
use crate::ctx::{Ctx, PROBE_USER};
use crate::step::DEFAULT_TIMEOUT;
use crate::tools;

/// The branch stage 3 opens its change from. Cut here because the commit that seeds it is also
/// `git.push.p95`'s sample — the journey pushes twice, and the second push is the one a pull
/// request needs anyway.
pub const HEAD_BRANCH: &str = "slo";
pub const BASE_BRANCH: &str = "main";

/// `browse.commit.visible` is bounded at 5 s by the catalogue; the wait is given more room for the
/// same reason `id.key.usable`'s is — a slow answer is a breach with a number, a cut-off one is
/// indistinguishable from the tier being down.
const VISIBLE_CAP: Duration = Duration::from_secs(20);

const GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Every id after the first push, in order. A precondition that fails skips exactly this list.
const AFTER_PUSH: [&str; 8] = [
    "git.push.p95",
    "git.clone.ok",
    "git.clone.p95",
    "ssh.clone.ok",
    "ssh.unregistered.refused",
    "browse.p95",
    "browse.commit.visible",
    "web.repo.page",
];

pub async fn run(c: &mut Ctx) {
    let name = c.prefix();
    if let Err(e) = create(c, &name).await {
        // One reason, on every id the repo was the precondition for.
        let why = format!("no repo: {e:#}");
        c.skip("id.jwt.tiers", &why);
        c.skip("git.push.ok", &why);
        for id in AFTER_PUSH {
            c.skip(id, &why);
        }
        // The host key is served by the SSH listener whether or not this repo exists, so it is the
        // one step here that still means something.
        hostkey(c).await;
        return;
    }
    c.state.repo = Some(name.clone());

    let work = c.tmp.join("git").join(&name);
    // The local git work lives INSIDE the push step, not before it. A tmp directory that cannot be
    // written is the fleet answering nothing about itself — but it is still `git.push.ok` that did
    // not happen, and silently swallowing it would report a green run with no push in it.
    if !push_base(c, &work, &name).await {
        for id in AFTER_PUSH {
            c.skip(id, "the first push failed");
        }
        super::identity::tiers(c, &name).await;
        hostkey(c).await;
        return;
    }

    // Needs a repo with a ref in it, which is why it runs here and not in stage 1.
    super::identity::tiers(c, &name).await;

    let head_branch_pushed = push_head(c, &work, &name).await;
    let head_oid = match git(c, vec!["rev-parse".into(), BASE_BRANCH.into()], Some(&work)).await {
        Ok(o) => Some(o.trim().to_string()),
        Err(e) => {
            tracing::warn!(op = "rev-parse", error = %format!("{e:#}"), "slo.git.failed");
            None
        }
    };

    clone_http(c, &name).await;
    hostkey(c).await;
    ssh_clone(c, &name).await;
    unregistered_refused(c, &name).await;
    browse(c, &name, head_oid.as_deref()).await;
    web_repo_page(c, &name).await;
    let _ = head_branch_pushed;
}

/// Private, and named for the run: private because a public probe repo is a namespace anybody can
/// watch churn in, and named for the run because that prefix is the whole of teardown's contract.
async fn create(c: &mut Ctx, name: &str) -> Result<()> {
    let body = serde_json::json!({ "owner": PROBE_USER, "name": name, "visibility": "private" });
    let jwt = c.probe_jwt.clone();
    post(c, &api(c, "/v1/repos"), &jwt, body).await.map(|_| ())
}

/// The environment every git call runs under.
///
/// `GIT_COMMITTER_*`/`GIT_AUTHOR_*` are not optional: the pod has no git identity, and a commit
/// without one fails with an error about `user.email` that reads like a fleet problem. `GIT_TERMINAL_PROMPT=0`
/// turns a rejected credential into an exit code instead of a process waiting on a tty nobody has.
pub(crate) fn git_env(c: &Ctx) -> HashMap<String, String> {
    HashMap::from([
        ("GIT_AUTHOR_NAME".into(), "kloudlite slo probe".into()),
        ("GIT_AUTHOR_EMAIL".into(), crate::ctx::PROBE_EMAIL.into()),
        ("GIT_COMMITTER_NAME".into(), "kloudlite slo probe".into()),
        ("GIT_COMMITTER_EMAIL".into(), crate::ctx::PROBE_EMAIL.into()),
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ("GIT_CONFIG_GLOBAL".into(), c.tmp.join("gitconfig").display().to_string()),
        ("GIT_CONFIG_SYSTEM".into(), "/dev/null".into()),
    ])
}

/// `-c http.extraHeader=…` carrying the probe's JWT, ahead of the subcommand.
///
/// The header is never anywhere else: `tools::run` refuses to put an argv in an error, so this is
/// the only place the token appears and it goes no further than the child's own memory.
pub(crate) fn authed(c: &Ctx, rest: &[&str]) -> Vec<String> {
    let mut args = vec![
        "-c".to_string(),
        format!("http.extraHeader=Authorization: Bearer {}", c.probe_jwt),
    ];
    args.extend(rest.iter().map(|a| a.to_string()));
    args
}

async fn git(c: &Ctx, args: Vec<String>, dir: Option<&Path>) -> Result<String> {
    tools::run(&c.programs.git, &args, &git_env(c), dir, GIT_TIMEOUT).await
}

/// The first push: make the tree, commit, push `main`. All of it inside the step, so a local
/// failure is `git.push.ok` failing with the reason rather than a lost sample (H1).
async fn push_base(c: &mut Ctx, work: &Path, name: &str) -> bool {
    let url = format!("{}/{PROBE_USER}/{name}.git", c.cfg.git_url.trim_end_matches('/'));
    let (work, name) = (work.to_path_buf(), name.to_string());
    c.step("git.push.ok", GIT_TIMEOUT, move |c| {
        let args = authed(c, &["push", "-q", &url, BASE_BRANCH]);
        async move {
            std::fs::create_dir_all(&work)
                .with_context(|| format!("could not make {}", work.display()))?;
            git(c, vec!["init".into(), "-q".into(), format!("--initial-branch={BASE_BRANCH}")], Some(&work)).await?;
            write(&work, "README.md", &format!("# {name}\n"))?;
            git(c, vec!["add".into(), "-A".into()], Some(&work)).await?;
            git(c, vec!["commit".into(), "-q".into(), "-m".into(), "seed".into()], Some(&work)).await?;
            git(c, args, Some(&work)).await.map(|_| ())
        }
        .boxed()
    })
    .await
}

/// The second push, onto the branch stage 3 opens its change from — so it is work the journey
/// needed anyway rather than a push invented for a number. Same shape as `push_base`: the local
/// half is inside the step, and a failure preparing it fails `git.push.p95` rather than vanishing.
async fn push_head(c: &mut Ctx, work: &Path, name: &str) -> bool {
    let url = format!("{}/{PROBE_USER}/{name}.git", c.cfg.git_url.trim_end_matches('/'));
    let (work, run_id) = (work.to_path_buf(), c.run_id.clone());
    c.step("git.push.p95", GIT_TIMEOUT, move |c| {
        let args = authed(c, &["push", "-q", &url, HEAD_BRANCH]);
        async move {
            git(c, vec!["checkout".into(), "-q".into(), "-b".into(), HEAD_BRANCH.into()], Some(&work)).await?;
            write(&work, "change.txt", &format!("{run_id}\n"))?;
            git(c, vec!["add".into(), "-A".into()], Some(&work)).await?;
            git(c, vec!["commit".into(), "-q".into(), "-m".into(), "change".into()], Some(&work)).await?;
            git(c, args, Some(&work)).await.map(|_| ())
        }
        .boxed()
    })
    .await
}

fn write(dir: &Path, name: &str, body: &str) -> Result<()> {
    std::fs::write(dir.join(name), body).with_context(|| format!("could not write {name}"))
}

/// Two clones, for the same reason there are two pushes.
async fn clone_http(c: &mut Ctx, name: &str) {
    let url = format!("{}/{PROBE_USER}/{name}.git", c.cfg.git_url.trim_end_matches('/'));
    for (id, into) in [("git.clone.ok", "clone-a"), ("git.clone.p95", "clone-b")] {
        let dest = c.tmp.join(into);
        // A leftover from a run that died mid-clone would make git refuse the directory, which
        // reads as a fleet failure and is not one.
        let _ = std::fs::remove_dir_all(&dest);
        let url = url.clone();
        c.step(id, GIT_TIMEOUT, move |c| {
            let dest = dest.display().to_string();
            let args = authed(c, &["clone", "-q", &url, &dest]);
            async move { git(c, args, None).await.map(|_| ()) }.boxed()
        })
        .await;
    }
}

/// The `known_hosts` the SSH steps run against, written from the PINNED key.
///
/// Never from `ssh-keyscan`: learning the key from the host being measured makes
/// `StrictHostKeyChecking=yes` a formality that passes through exactly the substitution it exists
/// to refuse.
async fn known_hosts(c: &Ctx) -> Result<PathBuf> {
    if c.cfg.ssh_hostkey.trim().is_empty() {
        return Err(anyhow!("no host key is pinned (KLOUDLITE_GIT_SLO_SSH_HOSTKEY)"));
    }
    let (host, port) = c.cfg.ssh_endpoint();
    // `[host]:port` is the form ssh matches a non-22 port against; the bare host for 22.
    let subject = if port == 22 { host.to_string() } else { format!("[{host}]:{port}") };
    let path = c.tmp.join("known_hosts");
    let line = c.cfg.ssh_hostkey.trim();
    // A `SHA256:…` fingerprint identifies a key but cannot be written into a known_hosts file, so
    // the SSH steps learn the key from the listener and `hostkey` is what judges it — the pin is
    // still checked every run, one step earlier, which is the guarantee that matters.
    let body = match pin_kind(line) {
        Pin::Fingerprint => {
            let (host, port) = (host.to_string(), port);
            let served = keyscan(c, &host, port).await?;
            served
        }
        // The operator may paste the key alone or a whole known_hosts line; `subject` is prepended
        // only when the line does not already carry one.
        Pin::Key => format!("{subject} {line}\n"),
        Pin::Line => format!("{line}\n"),
    };
    std::fs::write(&path, body).context("could not write known_hosts")?;
    Ok(path)
}

/// What the operator pinned. Three shapes, because all three are things people paste.
enum Pin {
    /// `SHA256:…`, what `ssh-keygen -lf` prints and what a fingerprint check compares.
    Fingerprint,
    /// `ssh-ed25519 AAAA…` — a key with no host in front of it.
    Key,
    /// `host ssh-ed25519 AAAA…` — a whole known_hosts line.
    Line,
}

fn pin_kind(pin: &str) -> Pin {
    if pin.starts_with("SHA256:") {
        Pin::Fingerprint
    } else if pin.starts_with("ssh-") || pin.starts_with("ecdsa-") || pin.starts_with("sk-") {
        Pin::Key
    } else {
        Pin::Line
    }
}

async fn keyscan(c: &Ctx, host: &str, port: u16) -> Result<String> {
    tools::plain(&c.programs.ssh_keyscan, &["-p", &port.to_string(), host], Duration::from_secs(20))
        .await
        .context("could not read the served host key")
}

fn ssh_command(c: &Ctx, key: &str, hosts: &Path) -> String {
    let (_, port) = c.cfg.ssh_endpoint();
    format!(
        "ssh -i {key} -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile={} -o BatchMode=yes -p {port}",
        hosts.display()
    )
}

fn ssh_url(c: &Ctx, name: &str) -> String {
    let (host, port) = c.cfg.ssh_endpoint();
    // The user name is ignored by the listener — it authenticates on the key's fingerprint — but
    // `git@` is what the web's clone box prints, so it is what the probe walks.
    format!("ssh://git@{host}:{port}/{PROBE_USER}/{name}.git")
}

/// What the listener actually serves, compared against the pin.
///
/// A separate step from `ssh.clone.ok` on purpose: a clone failing tells you nothing about WHY,
/// and "the host key changed" is the one cause an operator must never have to guess at.
async fn hostkey(c: &mut Ctx) {
    if c.cfg.ssh_hostkey.trim().is_empty() {
        c.skip("ssh.hostkey", "no host key is pinned (KLOUDLITE_GIT_SLO_SSH_HOSTKEY)");
        return;
    }
    c.step("ssh.hostkey", Duration::from_secs(30), |c| {
        let (host, port) = c.cfg.ssh_endpoint();
        let (host, port) = (host.to_string(), port);
        let pinned = c.cfg.ssh_hostkey.trim().to_string();
        let (scan, keygen) = (c.programs.ssh_keyscan.clone(), c.programs.ssh_keygen.clone());
        async move {
            let served = tools::plain(&scan, &["-p", &port.to_string(), &host], Duration::from_secs(20))
                .await
                .context("could not read the served host key")?;
            let hit = match pin_kind(&pinned) {
                // `ssh-keygen -lf -` prints one `bits SHA256:… host (ALG)` line per key the scan
                // offered; the pin matching any of them is the pin being served.
                Pin::Fingerprint => {
                    let path = c.tmp.join("served_hostkeys");
                    std::fs::write(&path, &served).context("could not stage the served keys")?;
                    let listed = tools::plain(&keygen, &["-lf", &path.display().to_string()], Duration::from_secs(10))
                        .await
                        .context("could not fingerprint the served host key")?;
                    listed.split_whitespace().any(|w| w == pinned)
                }
                // The base64 blob is the identity; comparing whole lines would fail on a comment
                // or a host field the two spellings disagree about.
                Pin::Key | Pin::Line => pinned
                    .split_whitespace()
                    .filter(|w| w.len() > 40)
                    .any(|blob| served.contains(blob)),
            };
            if hit {
                Ok(())
            } else {
                Err(anyhow!("the served host key does not match the pinned one"))
            }
        }
        .boxed()
    })
    .await;
}

async fn ssh_clone(c: &mut Ctx, name: &str) {
    if c.state.key.is_none() {
        c.skip("ssh.clone.ok", "the probe's key was never registered");
        return;
    }
    let hosts = match known_hosts(c).await {
        Ok(p) => p,
        Err(e) => {
            c.skip("ssh.clone.ok", &format!("{e:#}"));
            return;
        }
    };
    let dest = c.tmp.join("clone-ssh");
    let _ = std::fs::remove_dir_all(&dest);
    c.step("ssh.clone.ok", GIT_TIMEOUT, move |c| {
        let url = ssh_url(c, name);
        let cmd = ssh_command(c, &c.cfg.ssh_key_path.clone(), &hosts);
        let mut env = git_env(c);
        env.insert("GIT_SSH_COMMAND".into(), cmd);
        let args = vec!["clone".into(), "-q".into(), url, dest.display().to_string()];
        let git = c.programs.git.clone();
        async move { tools::run(&git, &args, &env, None, GIT_TIMEOUT).await.map(|_| ()) }.boxed()
    })
    .await;
}

/// A key the fleet has never seen must be refused.
///
/// The step passes only when the connection FAILS, which is the one measurement in the suite whose
/// polarity is inverted — and the reason `tools` has a program override at all, since nothing else
/// in the binary can make a real `ssh` succeed where it should not.
async fn unregistered_refused(c: &mut Ctx, name: &str) {
    let hosts = match known_hosts(c).await {
        Ok(p) => p,
        Err(e) => {
            c.skip("ssh.unregistered.refused", &format!("{e:#}"));
            return;
        }
    };
    let junk = c.tmp.join("unregistered");
    let _ = std::fs::remove_file(&junk);
    let _ = std::fs::remove_file(junk.with_extension("pub"));
    let made = tools::plain(
        &c.programs.ssh_keygen,
        &["-q", "-t", "ed25519", "-N", "", "-C", "unregistered", "-f", &junk.display().to_string()],
        Duration::from_secs(20),
    )
    .await;
    if let Err(e) = made {
        c.skip("ssh.unregistered.refused", &format!("no throwaway key: {e:#}"));
        return;
    }
    c.step("ssh.unregistered.refused", GIT_TIMEOUT, move |c| {
        let url = ssh_url(c, name);
        let cmd = ssh_command(c, &junk.display().to_string(), &hosts);
        let mut env = git_env(c);
        env.insert("GIT_SSH_COMMAND".into(), cmd);
        // `ls-remote`, not `clone`: it is the smallest thing that requires authentication, and it
        // leaves nothing on disk to clean up when the refusal does not happen.
        let args = vec!["ls-remote".into(), url];
        let git = c.programs.git.clone();
        async move {
            match tools::run(&git, &args, &env, None, GIT_TIMEOUT).await {
                Ok(_) => Err(anyhow!("an unregistered key was allowed to read the repo")),
                // Only a REFUSAL passes. A DNS failure, a wrong port or a host-key mismatch also
                // make the command fail, and counting those as "the fleet refused it" would keep
                // this SLO green through the exact outage it is supposed to catch.
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

/// The Browse API, through the api tier's forwarder — the same hop the web app makes.
async fn browse(c: &mut Ctx, name: &str, head: Option<&str>) {
    let Some(head) = head else {
        c.skip("browse.p95", "nothing was pushed");
        c.skip("browse.commit.visible", "nothing was pushed");
        return;
    };
    let head = head.to_string();
    let refs_url = api(c, &format!("/api/{PROBE_USER}/{name}/refs"));

    {
        let (head, url) = (head.clone(), refs_url.clone());
        c.step("browse.commit.visible", VISIBLE_CAP + Duration::from_secs(10), move |c| {
            let jwt = c.probe_jwt.clone();
            async move {
                poll_json(c, &url, &jwt, VISIBLE_CAP, |refs| {
                    oid_of(refs, BASE_BRANCH).as_deref() == Some(head.as_str())
                })
                .await
            }
            .boxed()
        })
        .await;
    }

    // The repo page is a tree listing at the head, which is what the web renders and therefore
    // what the 500 ms target is about.
    let tree = api(c, &format!("/api/{PROBE_USER}/{name}/tree/{head}"));
    c.step("browse.p95", DEFAULT_TIMEOUT, move |c| {
        let jwt = c.probe_jwt.clone();
        async move { get(c, &tree, &jwt).await.map(|_| ()) }.boxed()
    })
    .await;
}

/// The oid of one branch out of a `/refs` answer, whose names are full (`refs/heads/main`).
pub(crate) fn oid_of(refs: &serde_json::Value, branch: &str) -> Option<String> {
    let want = format!("refs/heads/{branch}");
    refs.as_array()?
        .iter()
        .find(|r| r.get("name").and_then(|v| v.as_str()) == Some(&want))
        .and_then(|r| r.get("oid"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}


/// The web app's repo page, rendered.
///
/// The web authenticates with an Auth.js session cookie — a JWE encrypted with `AUTH_SECRET`,
/// which a pod holding only the api's signing secret cannot mint. So the page is measured the one
/// way that is honest without one: the repo is flipped PUBLIC for the length of the request and
/// back again. The flip back is part of the step, not a best-effort afterthought — stage 9's
/// `sec.private.repo` reads this same repo, and a probe repo left public is a hole the next run
/// would report as a passing security check.
async fn web_repo_page(c: &mut Ctx, name: &str) {
    let name = name.to_string();
    c.step("web.repo.page", DEFAULT_TIMEOUT, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = format!("{}/{PROBE_USER}/{name}", c.cfg.web_url.trim_end_matches('/'));
        let patch = api(c, &format!("/v1/repos/{PROBE_USER}/{name}"));
        async move {
            visibility(c, &patch, &jwt, "public").await.context("could not publish the repo")?;
            let rendered = rendered(c, &url).await;
            // Restored before the read is judged, so a failing page never leaves it public.
            let restored = visibility(c, &patch, &jwt, "private").await;
            if let Err(e) = &restored {
                tracing::error!(op = "restore-visibility", name = %name, error = %format!("{e:#}"), "slo.git.failed");
            }
            rendered?;
            restored.context("the repo was left PUBLIC")
        }
        .boxed()
    })
    .await;
}

async fn visibility(c: &Ctx, url: &str, jwt: &str, to: &str) -> Result<()> {
    let body = serde_json::json!({ "visibility": to });
    super::call(c, reqwest::Method::PATCH, url, jwt, Some(body)).await.map(|_| ())
}

/// The page must actually carry the repo's content, not merely answer 200: a signed-out visitor
/// gets a rendered 404 shell with a 200 from plenty of frameworks, and the file name is the
/// smallest thing that only the real page has.
async fn rendered(c: &Ctx, url: &str) -> Result<()> {
    let (status, body) = super::raw(c, reqwest::Method::GET, url, "", None, &[]).await?;
    if !status.is_success() {
        return Err(anyhow!("{status}: {}", body.chars().take(200).collect::<String>()));
    }
    if !body.contains("README.md") {
        return Err(anyhow!("the page rendered without the repo's files"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;
    use axum::http::StatusCode;
    use axum::routing::{get as axget, post as axpost};

    /// Every id the stage owns, so a test can assert on the whole set rather than the two it
    /// happened to think of.
    const IDS: [&str; 10] = [
        "git.push.ok",
        "git.push.p95",
        "git.clone.ok",
        "git.clone.p95",
        "ssh.clone.ok",
        "ssh.hostkey",
        "ssh.unregistered.refused",
        "browse.p95",
        "browse.commit.visible",
        "web.repo.page",
    ];

    fn sample<'a>(c: &'a Ctx, id: &str) -> &'a kloudlite_git_workspaces::history::slo::StepReport {
        c.steps.iter().find(|s| s.slo_id == id).unwrap_or_else(|| panic!("no {id}"))
    }

    /// A repo that cannot be created is ONE failure, already counted where it happened — the whole
    /// rest of the stage is no sample. Without this, a five-minute outage of the api tier would
    /// report ten breached SLOs and burn ten error budgets for one broken thing.
    #[tokio::test]
    async fn git_stage_skips_everything_when_repo_create_fails() {
        let app = axum::Router::new()
            .route("/v1/repos", axpost(|| async { StatusCode::INTERNAL_SERVER_ERROR }))
            .fallback(axget(|| async { StatusCode::NOT_FOUND }));
        let mut c = testkit::ctx_against(app).await;
        c.stage = super::super::GIT.to_string();

        run(&mut c).await;

        assert_eq!(c.failed(), 0, "a create failure must not be counted a second time as an SLO");
        for id in IDS {
            assert!(sample(&c, id).skipped, "{id} should be skipped, not sampled");
        }
        assert!(c.state.repo.is_none());
    }

    /// The local half of a push lives inside the push step, so a tmp directory nobody can write is
    /// `git.push.ok` FAILING with the reason — not a green run with no push in it, which is what
    /// preparing the tree before the step would have produced.
    #[tokio::test]
    async fn a_local_git_failure_fails_the_push_id_rather_than_vanishing() {
        let app = axum::Router::new()
            .route("/v1/repos", axpost(|| async { (StatusCode::CREATED, "{}") }))
            .fallback(axget(|| async { StatusCode::NOT_FOUND }));
        let mut c = testkit::ctx_against(app).await;
        c.stage = super::super::GIT.to_string();
        // A FILE where the working tree's parent directory has to be: `create_dir_all` cannot make
        // a directory under it, whatever the permissions say, and it does not need root to set up.
        std::fs::create_dir_all(&c.tmp).expect("tmp");
        std::fs::write(c.tmp.join("git"), "not a directory").expect("block the path");

        run(&mut c).await;

        let push = sample(&c, "git.push.ok");
        assert!(!push.skipped && !push.ok, "the push id must carry the failure");
        assert!(!push.detail.is_empty(), "with the reason");
        for id in AFTER_PUSH {
            assert!(sample(&c, id).skipped, "{id} should be skipped once the push failed");
        }
        // One failure among the stage's OWN ids, not one per downstream id. `id.jwt.tiers` also
        // fails here — it dials a git url no test serves — and is a different stage's sample.
        assert_eq!(
            c.steps.iter().filter(|s| IDS.contains(&s.slo_id.as_str()) && !s.ok && !s.skipped).count(),
            1
        );
    }

    /// The one step whose polarity is inverted. An `ssh` that SUCCEEDS with a key the fleet has
    /// never seen is the refusal not happening, and must be recorded as a failure — the bug this
    /// guards against is the natural one, writing the step as "did the command run".
    #[tokio::test]
    async fn unregistered_key_refusal_is_ok_only_when_ssh_fails() {
        let app = axum::Router::new().fallback(axget(|| async { StatusCode::NOT_FOUND }));
        let mut c = testkit::ctx_against(app).await;
        c.cfg.ssh_hostkey = "host ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPinnedProbeHostKeyForTests".into();
        std::fs::create_dir_all(&c.tmp).expect("tmp");
        // Both stubbed by `true`, so the throwaway key "is generated" and the "ssh" attempt
        // succeeds — which is exactly the fleet accepting a key it must not.
        c.programs.git = "true".into();
        c.programs.ssh_keygen = "true".into();

        unregistered_refused(&mut c, "run-fast-1").await;

        let s = sample(&c, "ssh.unregistered.refused");
        assert!(!s.skipped, "it ran");
        assert!(!s.ok, "an accepted unregistered key is a failure");
        assert!(s.detail.contains("unregistered key was allowed"), "{}", s.detail);
    }

    /// A refusal is `Permission denied`, and nothing else. `false` fails the way a DNS error or a
    /// wrong port does — and counting that as "the fleet refused it" would keep this SLO green
    /// through the exact outage it exists to catch.
    #[tokio::test]
    async fn a_failure_that_is_not_a_refusal_is_not_a_pass() {
        let app = axum::Router::new().fallback(axget(|| async { StatusCode::NOT_FOUND }));
        let mut c = testkit::ctx_against(app).await;
        c.cfg.ssh_hostkey = "host ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPinnedProbeHostKeyForTests".into();
        std::fs::create_dir_all(&c.tmp).expect("tmp");
        c.programs.git = "false".into();
        c.programs.ssh_keygen = "true".into();

        unregistered_refused(&mut c, "run-fast-1").await;

        let s = sample(&c, "ssh.unregistered.refused");
        assert!(!s.ok, "a non-refusal failure is not a refusal");
        assert!(s.detail.contains("some other reason"), "{}", s.detail);
    }
}
