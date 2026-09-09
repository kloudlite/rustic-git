//! Experience probes on a person's identity: handle, profile, CLI tokens and device login, the
//! ssh-config block `kl-connect` writes, and a key's whole life from add to revoke.

use super::*;


/// `id.username`: the one irreversible identity write, measured by its refusals.
///
/// See the module header: claiming a FREE handle would rename this tenant, so the two things the
/// route owes are what is asked of it — a handle somebody holds is refused, and one that breaks
/// `check_handle` is rejected before it reaches the directory at all. Both, because either alone
/// would pass against a route that refused everything.
pub(crate) async fn username(c: &mut Ctx) {
    let probe = c.probe_user.clone();
    c.step("id.username", QUICK, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, "/v1/users/username");
        async move {
            // NOT a 409. `claim_username` (`crates/pulls/src/directory/mod.rs:751-761`) answers a
            // caller who already holds a handle before it ever looks the wanted one up: the same
            // handle is a retry and answers 200, a different one is "username already set". The
            // 409 path exists only for an account with NO handle, and this tenant has had one
            // since bootstrap — so the invariant this id can actually assert is the one that
            // matters anyway: the claim is IRREVERSIBLE.
            let (status, body) =
                raw(c, reqwest::Method::POST, &url, &jwt, Some(json!({ "username": format!("{probe}2") })), &[]).await?;
            if status.as_u16() != 400 || !body.contains("already set") {
                return Err(anyhow!("re-claiming a handle answered {status}: {}", clip(&body)));
            }
            // And the shape check runs before the directory is asked at all, so a malformed handle
            // is refused whoever is asking.
            let (status, body) =
                raw(c, reqwest::Method::POST, &url, &jwt, Some(json!({ "username": "-Not A Handle-" })), &[]).await?;
            match status.as_u16() {
                400 if !body.contains("already set") => Ok(()),
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
pub(crate) async fn profile_upsert(c: &mut Ctx) {
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
pub(crate) async fn cli_tokens(c: &mut Ctx) {
    let device = format!("{}-rev", c.prefix());
    c.step("id.cli.tokens", QUICK, move |c| {
        let jwt = c.probe_jwt.clone();
        async move {
            let (token, id) = cli_login(c, &jwt, &device).await?;
            // Asked of the CLI's OWN collection, not `/v1/repos`: a `cli` token is honoured by the
            // routes that go through `user_identity` (which checks the jti against the revocation
            // row) and refused by the ones that go through `caller`, which takes a browser session
            // only. `/v1/repos` is the second kind, so the 401 it answers says nothing about the
            // token — it says the probe asked the wrong door.
            let mine = api(c, "/v1/cli/tokens");
            get(c, &mine, &token).await.context("a fresh CLI token was not honoured")?;
            let listed = get(c, &mine, &jwt).await.context("could not list the CLI tokens")?;
            let there = listed.as_array().is_some_and(|rows| {
                rows.iter().any(|r| r.get("id").and_then(Value::as_str) == Some(id.as_str()))
            });
            if !there {
                return Err(anyhow!("the CLI token was minted but is not listed"));
            }
            call(c, reqwest::Method::DELETE, &api(c, &format!("/v1/cli/tokens/{id}")), &jwt, None)
                .await
                .context("could not revoke the CLI token")?;
            let (status, _) = raw(c, reqwest::Method::GET, &mine, &token, None, &[]).await?;
            match status.as_u16() {
                401 | 403 => Ok(()),
                other => Err(anyhow!("a revoked CLI token answered {other}")),
            }
        }
        .boxed()
    })
    .await;
}


/// The whole `kl-connect login` handshake, answering `(token, id)`. The device name carries the run
/// prefix, which is teardown's only handle on what this mints.
pub(crate) async fn cli_login(c: &Ctx, jwt: &str, device: &str) -> Result<(String, String)> {
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


/// `id.cli.sshconfig`: `kl-connect ws sshconfig` writes a block a person's `ssh` can use.
///
/// The real binary, against a real login, with `HOME` and `KL_CONFIG_DIR` pointed at the run's tmp
/// tree — the pod's root filesystem is read-only and the command writes `~/.ssh/config`. The
/// assertion is the rendered block, not the exit code: `render` skips a workspace whose name it
/// will not put in a config, so a run where every workspace was skipped exits zero having written
/// a file with nothing in it.
pub(crate) async fn sshconfig(c: &mut Ctx) {
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
                let dir = home.join(".config/kl-connect");
                std::fs::create_dir_all(&dir).with_context(|| format!("could not make {}", dir.display()))?;
                // Exactly what `kl-connect login` stores, so the command has nothing to do but read it.
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
                tools::run(&kl, &["ws".to_string(), KL_SSH_CONFIG.into()], &env, None, SSHCONFIG_CEILING)
                    .await
                    .with_context(|| format!("`kl-connect ws {KL_SSH_CONFIG}` failed"))?;
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
pub(crate) fn has_host_block(block: &str, id: &str) -> Result<()> {
    let hostname = format!("HostName {id}");
    let proxy = format!("ProxyCommand kl-connect ws proxy {id}");
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
pub(crate) async fn key_lifecycle(c: &mut Ctx) {
    let Some(repo) = c.state.repo.clone() else {
        return c.skip("key.ssh.lifecycle", "no repo to clone");
    };
    let hosts = match super::super::git::known_hosts(c).await {
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
    let name = format!("{}-lifecycle", c.prefix());
    c.step("key.ssh.lifecycle", KEY_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = ssh_remote(c, &repo);
        let cmd = super::super::git::ssh_command(c, &key.display().to_string(), &hosts);
        let mut env = super::super::git::git_env(c);
        env.insert("GIT_SSH_COMMAND".into(), cmd);
        let git_bin = c.programs.git.clone();
        let keys = api(c, "/v1/keys");
        async move {
            let added = post(c, &keys, &jwt, json!({ "name": name, "key": public }))
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
                call(c, reqwest::Method::DELETE, &api(c, &format!("/v1/keys/{}", path_seg(&id))), &jwt, None)
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
            // STRICT again: credential HITS are not cached at all (`crates/storage/src/auth.rs`'s
            // comment on `CACHE_TTL` — only misses are, because the cache is per process and the
            // revoking process is not the authenticating one). A removed key must be refused on
            // the very next request; the small bound is the api round trip, nothing else.
            let refused = refused_within(&git_bin, &argv, &env, REVOCATION_WINDOW).await;
            match refused {
                true => Ok(()),
                false => Err(anyhow!(
                    "a removed key could still read the repo {} s after it was deleted",
                    REVOCATION_WINDOW.as_secs()
                )),
            }
        }
        .boxed()
    })
    .await;
}


/// Whether the key is refused before `cap` runs out. A refusal is `Permission denied`; anything
/// else ssh says is neither an acceptance nor a refusal and keeps the loop going until the window
/// closes, so a transient network error cannot read as a revocation.
pub(crate) async fn refused_within(
    git: &str,
    argv: &[String],
    env: &std::collections::HashMap<String, String>,
    cap: Duration,
) -> bool {
    let start = std::time::Instant::now();
    loop {
        match tools::run(git, argv, env, None, Duration::from_secs(30)).await {
            Ok(_) => {}
            Err(e) if format!("{e:#}").contains("Permission denied") => return true,
            Err(e) => tracing::info!(error = %format!("{e:#}"), "slo.key.revocation.waiting"),
        }
        if start.elapsed() >= cap {
            return false;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}


/// The SSH clone URL for one of the probe's own repos.
pub(crate) fn ssh_remote(c: &Ctx, repo: &str) -> String {
    super::super::git::ssh_url(c, repo)
}


/// Retry `ls-remote` until the newly added key is honoured.
pub(crate) async fn ssh_works(
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
