//! Stage 1 · Identity: the credentials the rest of the journey is carried out with.
//!
//! Everything here is also a PRECONDITION for a later stage — the token, the registered SSH key,
//! the proof a JWT is honoured on all three tiers — which is why this stage is first and why its
//! failures are recorded rather than aborted: a broken key registration must show up as
//! `id.key.usable` failing and `ssh.clone.ok` skipping, not as one anonymous dead run.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;

use super::{api, get, post, raw};
use crate::ctx::{Ctx, PROBE_USER};
use crate::step::DEFAULT_TIMEOUT;
use crate::tools;

/// The catalogue bounds `id.key.usable` and `id.cli.flow` at 30 s and 15 s. The step timeout is
/// deliberately looser than the target: a call that took 40 s is a BREACH with a number, while one
/// cut off at the target would be indistinguishable from the tier being down.
const KEY_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn run(c: &mut Ctx) {
    // The session JWT is minted in-process from the Secret, so there is no password path to walk:
    // the web signs in through Auth.js (OAuth or an emailed link) and neither is reachable from a
    // pod. What is left — and what actually breaks — is whether that token IDENTIFIES anyone:
    // `/v1/cli/tokens` defaults its owner to the caller's own handle, so a 200 is the directory
    // resolving this JWT to `slo-probe` and nothing else.
    c.step("id.signin", DEFAULT_TIMEOUT, |c| {
        let jwt = c.probe_jwt.clone();
        async move {
            get(c, &api(c, "/v1/cli/tokens"), &jwt).await?;
            Ok(())
        }
        .boxed()
    })
    .await;

    let name = c.prefix();
    if c.step("id.token.mint", DEFAULT_TIMEOUT, |c| {
        let (name, jwt) = (name.clone(), c.probe_jwt.clone());
        async move {
            let body = serde_json::json!({ "owner": PROBE_USER, "name": name });
            let out = post(c, &api(c, "/v1/tokens"), &jwt, body).await?;
            // Recorded so teardown revokes it by id even if the name sweep somehow misses it.
            c.state.token = out
                .pointer("/meta/_id")
                .or_else(|| out.pointer("/meta/id"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            // The one time it is readable, and it is never logged: a token in a step detail would
            // outlive the run in ClickHouse.
            out.get("token")
                .and_then(|v| v.as_str())
                .filter(|t| !t.is_empty())
                .map(|_| ())
                .ok_or_else(|| anyhow!("the answer carried no token"))
        }
        .boxed()
    })
    .await
    {
        tracing::info!("slo.identity.token.ready");
    }

    key(c, &name).await;
    cli_flow(c, &name).await;
    tiers(c).await;
}

/// Register the public half of the mounted key and confirm the directory lists it.
///
/// The listing matters as much as the write: the fleet authenticates an SSH connection from the
/// object store's `authorized_keys` view, which is built from these rows, so a key that was
/// accepted but never listed is one `ssh.clone.ok` would fail on with no clue why.
async fn key(c: &mut Ctx, name: &str) {
    let ok = c
        .step("id.key.usable", KEY_TIMEOUT, |c| {
            let (name, jwt, key_path) = (name.to_string(), c.probe_jwt.clone(), c.cfg.ssh_key_path.clone());
            async move {
                // Derived from the private half rather than mounted beside it: two files that must
                // agree are two files that can disagree, and the Secret holds only the one.
                let public = tools::plain("ssh-keygen", &["-y", "-f", &key_path], Duration::from_secs(10))
                    .await
                    .context("could not read the probe's public key")?;
                let public = public.trim().to_string();
                let body = serde_json::json!({ "owner": PROBE_USER, "name": name, "key": public });
                post(c, &api(c, "/v1/keys"), &jwt, body).await.context("could not register the key")?;
                let listed = get(c, &api(c, &format!("/v1/keys?owner={PROBE_USER}")), &jwt).await?;
                let found = listed
                    .as_array()
                    .is_some_and(|rows| rows.iter().any(|r| r.get("name").and_then(|v| v.as_str()) == Some(&name)));
                if !found {
                    return Err(anyhow!("the key was accepted but is not listed"));
                }
                c.state.key = Some(name);
                Ok(())
            }
            .boxed()
        })
        .await;
    if !ok {
        tracing::warn!("slo.identity.key.unusable");
    }
}

/// The whole device-code handshake a person walks when they run `kl login`: ask for a code with no
/// credentials, approve it as the signed-in person, then collect the token exactly once.
async fn cli_flow(c: &mut Ctx, name: &str) {
    c.step("id.cli.flow", Duration::from_secs(45), |c| {
        let (name, jwt) = (name.to_string(), c.probe_jwt.clone());
        async move {
            // The device name carries the run prefix, which is the only handle teardown has on the
            // token this mints.
            let started = post(c, &api(c, "/v1/cli/code"), "", serde_json::json!({ "device": name })).await?;
            let code = started.get("code").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let poll = started.get("poll").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            if code.is_empty() || poll.is_empty() {
                return Err(anyhow!("the login handshake answered no code"));
            }
            post(c, &api(c, "/v1/cli/approve"), &jwt, serde_json::json!({ "code": code })).await?;
            // One poll, not a loop: approve is synchronous, so a 202 here means the row the
            // approval wrote is not visible to the replica that answered — which is the failure.
            let out = get(c, &api(c, &format!("/v1/cli/token?poll={poll}")), "").await?;
            out.get("token")
                .and_then(|v| v.as_str())
                .filter(|t| !t.is_empty())
                .map(|_| ())
                .ok_or_else(|| anyhow!("the approved login handed back no token"))
        }
        .boxed()
    })
    .await;
}

/// One JWT, three tiers: `/v1`, the git fleet's browse API through the api's forwarder, and the
/// web app. A token honoured by two of the three is the failure this exists to name — each tier
/// verifies it against its own copy of the secret, and a rotation that reached two of them is
/// invisible to any single-tier check.
async fn tiers(c: &mut Ctx) {
    c.step("id.jwt.tiers", DEFAULT_TIMEOUT, |c| {
        let jwt = c.probe_jwt.clone();
        async move {
            get(c, &api(c, &format!("/v1/repos?owner={PROBE_USER}")), &jwt).await.context("/v1")?;
            // Owner-scoped, so it needs no repo and can run before stage 2 creates one — and it is
            // still a real hop onto the git tier through the api's browse forwarder.
            get(c, &api(c, &format!("/api/{PROBE_USER}/images")), &jwt).await.context("browse")?;
            web_page(c, &format!("/{PROBE_USER}")).await.context("web")
        }
        .boxed()
    })
    .await;
}

/// A signed-in `GET` of a web page. The web app authenticates with an Auth.js session cookie
/// rather than a bearer, so the probe presents the JWT as that cookie: what is measured is the
/// PAGE rendering, and a page that renders signed-out is a different measurement.
pub(crate) async fn web_page(c: &Ctx, path: &str) -> Result<()> {
    let url = format!("{}{path}", c.cfg.web_url.trim_end_matches('/'));
    // The web picks its cookie name from whether AUTH_URL is https (`web/apps/web/src/auth.ts`),
    // and the probe reaches the same web over the same scheme, so the scheme decides it here too.
    let name = match c.cfg.web_url.starts_with("https") {
        true => "__Secure-authjs.session-token",
        false => "authjs.session-token",
    };
    let cookie = format!("{name}={}", c.probe_jwt);
    let (status, body) = raw(c, reqwest::Method::GET, &url, "", None, &[("cookie", cookie)]).await?;
    if !status.is_success() {
        return Err(anyhow!("{path} answered {status}: {}", body.chars().take(200).collect::<String>()));
    }
    Ok(())
}
