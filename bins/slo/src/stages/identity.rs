//! Stage 1 · Identity: the credentials the rest of the journey is carried out with.
//!
//! Everything here is also a PRECONDITION for a later stage — the token, the registered SSH key,
//! the proof a JWT is honoured on all three tiers — which is why this stage is first and why its
//! failures are recorded rather than aborted: a broken key registration must show up as
//! `id.key.usable` failing and `ssh.clone.ok` skipping, not as one anonymous dead run.

use std::time::Duration;

use anyhow::{anyhow, Context};
use futures::FutureExt;

use super::{api, get, post};
use crate::ctx::Ctx;
use crate::step::DEFAULT_TIMEOUT;
use crate::tools;

/// The catalogue bounds `id.key.usable` and `id.cli.flow` at 30 s and 15 s. The step timeout is
/// deliberately looser than the target: a call that took 40 s is a BREACH with a number, while one
/// cut off at the target would be indistinguishable from the tier being down.
const KEY_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn run(c: &mut Ctx) {
    let probe = c.probe_user.clone();
    // The owner's PLATFORM key is generated on the first read of `/v1/platform-key`, and the api
    // writes a workspace's `authorized_keys` Secret only for an owner who has one (see
    // `write_user_key`). A person reaches that page in the web; the probe never does, so it reads
    // it here — otherwise every gateway login answers "Permission denied (publickey)".
    if let Err(e) = get(c, &api(c, &format!("/v1/platform-key?owner={probe}")), &c.probe_jwt.clone()).await {
        tracing::warn!(reason = "platform-key", error = %e, "slo.identity.degraded");
    }
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
    c.step("id.token.mint", DEFAULT_TIMEOUT, |c| {
        let (name, jwt) = (name.clone(), c.probe_jwt.clone());
        async move {
            let body = serde_json::json!({ "owner": probe, "name": name });
            let out = post(c, &api(c, "/v1/tokens"), &jwt, body).await?;
            // Recorded so teardown revokes it by id even if the name sweep somehow misses it.
            // `_id` at the root: the answer is `IssuedToken`, whose `meta` is flattened into it.
            c.state.token = out.pointer("/_id").and_then(|v| v.as_str()).map(str::to_string);
            // The one time it is readable, and it is never logged: a token in a step detail would
            // outlive the run in ClickHouse.
            // Kept, not printed: `reg.push.ok` logs in to the registry with this exact value.
            c.state.token_value =
                out.get("token").and_then(|v| v.as_str()).filter(|t| !t.is_empty()).map(str::to_string);
            c.state.token_value
                .as_ref()
                .map(|_| ())
                .ok_or_else(|| anyhow!("the answer carried no token"))
        }
        .boxed()
    })
    .await;

    key(c, &name).await;
    cli_flow(c, &name).await;
    passkey(c, &name).await;
}

/// `id.signin.passkey`: the half of a passkey sign-in this tier actually owns.
///
/// NOT the WebAuthn ceremony: the relying-party identity and the challenge live in the web app
/// (`crates/api/src/passkeys.rs`'s own header says so), and the api holds no verifier — there is
/// nothing a pod could sign an assertion against. What a sign-in DOES ask this tier is "whose
/// credential is this", off a row somebody registered, so that is what is measured: the row goes
/// in, is listed back, the lookup a sign-in makes stays peer-only, and the row comes out again.
///
/// The credential is synthetic and is deleted inside the step. A passkey the probe left behind is
/// a standing sign-in credential for the probe's account that nobody holds the private half of —
/// harmless, and exactly the kind of thing that should not accumulate one per five minutes.
async fn passkey(c: &mut Ctx, name: &str) {
    let name = name.to_string();
    c.step("id.signin.passkey", Duration::from_secs(15), move |c| {
        let jwt = c.probe_jwt.clone();
        let base = api(c, "/v1/passkeys");
        let lookup = api(c, "/v1/passkeys/lookup");
        // The credential id a browser would hand over is opaque base64url; the run id makes this
        // one unique per run, so two overlapping runs never collide on the 409 path.
        let id = format!("slo-{}", c.run_id);
        async move {
            let body = serde_json::json!({
                "id": id,
                "publicKey": "slo-probe-synthetic-public-key",
                "name": name,
            });
            let out = post(c, &base, &jwt, body).await.context("could not register the passkey");
            // Deleted whatever the rest of the step said, and before the step can return: a
            // registration nobody takes back is a sign-in credential left on the account.
            let forget = || async {
                super::call(c, reqwest::Method::DELETE, &format!("{base}/{id}"), &jwt, None)
                    .await
                    .map(|_| ())
                    .context("the synthetic passkey was left REGISTERED")
            };
            let checks = async {
                out?;
                let listed = get(c, &base, &jwt).await.context("could not list the passkeys")?;
                // `_id`, not `id`: `directory::Passkey` renames its credential id for Mongo, so
                // that is the field both the create's answer and the listing carry.
                let there = listed.as_array().is_some_and(|rows| {
                    rows.iter().any(|r| r.get("_id").and_then(|v| v.as_str()) == Some(id.as_str()))
                });
                if !there {
                    return Err(anyhow!("the passkey was accepted but is not listed"));
                }
                // The one route a sign-in calls before it knows who is signing in. A session must
                // never be enough — a credential id maps to somebody's email and public key — so
                // this JWT being refused IS the SLI's second half.
                let body = serde_json::json!({ "id": id });
                let (status, _) =
                    super::raw(c, reqwest::Method::POST, &lookup, &jwt, Some(body), &[]).await?;
                match status.as_u16() {
                    401 | 403 => Ok(()),
                    other => Err(anyhow!("the passkey lookup answered {other} to a session token")),
                }
            };
            crate::drill::undoing(Duration::from_secs(10), checks, forget).await
        }
        .boxed()
    })
    .await;
}

/// Register the public half of the mounted key and confirm the directory lists it.
///
/// The listing matters as much as the write: the fleet authenticates an SSH connection from the
/// object store's `authorized_keys` view, which is built from these rows, so a key that was
/// accepted but never listed is one `ssh.clone.ok` would fail on with no clue why.
async fn key(c: &mut Ctx, name: &str) {
    let probe = c.probe_user.clone();
    c.step("id.key.usable", KEY_TIMEOUT, |c| {
            let (name, jwt) = (name.to_string(), c.probe_jwt.clone());
            let (key_path, keygen) = (c.cfg.ssh_key_path.clone(), c.programs.ssh_keygen.clone());
            async move {
                // Derived from the private half rather than mounted beside it: two files that must
                // agree are two files that can disagree, and the Secret holds only the one.
                let public = tools::plain(&keygen, &["-y", "-f", &key_path], Duration::from_secs(10))
                    .await
                    .context("could not read the probe's public key")?;
                let public = public.trim().to_string();
                let body = serde_json::json!({ "owner": probe, "name": name, "key": public });
                post(c, &api(c, "/v1/keys"), &jwt, body).await.context("could not register the key")?;
                let listed = get(c, &api(c, &format!("/v1/keys?owner={probe}")), &jwt).await?;
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
            if out.get("token").and_then(|v| v.as_str()).is_none_or(str::is_empty) {
                return Err(anyhow!("the approved login handed back no token"));
            }
            // The id, not the token: the id is what revokes it, and the token itself must never be
            // held anywhere a report could reach.
            c.state.cli_token = list_cli_token_id(c, &name).await;
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// The id of the CLI login named `name`, for teardown. Best-effort: the name sweep gets it too,
/// and a failure here must not turn a working login flow into a red SLO.
async fn list_cli_token_id(c: &Ctx, name: &str) -> Option<String> {
    let rows = get(c, &api(c, "/v1/cli/tokens"), &c.probe_jwt).await.ok()?;
    rows.as_array()?
        .iter()
        .find(|r| r.get("name").and_then(|v| v.as_str()) == Some(name))
        .and_then(|r| r.get("id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// One JWT, three tiers: `/v1`, the git fleet's browse API through the api's forwarder, and the
/// git protocol itself over HTTP.
///
/// Each tier verifies the token against its own copy of the secret, so a rotation that reached two
/// of the three is invisible to any single-tier check — and that is the failure this names. It
/// runs from the git stage rather than stage 1 because two of the three legs need a repo to point
/// at; the step is stamped `1 · Identity` regardless, which is where the journey puts it.
pub(crate) async fn tiers(c: &mut Ctx, repo: &str) {
    let probe = c.probe_user.clone();
    let was = std::mem::replace(&mut c.stage, super::IDENTITY.to_string());
    c.step("id.jwt.tiers", DEFAULT_TIMEOUT, |c| {
        let jwt = c.probe_jwt.clone();
        let refs = api(c, &format!("/api/{probe}/{repo}/refs"));
        let url = format!("{}/{probe}/{repo}.git", c.cfg.git_url.trim_end_matches('/'));
        let args = super::git::authed(c, &["ls-remote", &url]);
        let (git, env) = (c.programs.git.clone(), super::git::git_env(c));
        async move {
            get(c, &api(c, &format!("/v1/repos?owner={probe}")), &jwt).await.context("/v1")?;
            get(c, &refs, &jwt).await.context("browse")?;
            tools::run(&git, &args, &env, None, Duration::from_secs(30)).await.context("git over HTTP")?;
            Ok(())
        }
        .boxed()
    })
    .await;
    c.stage = was;
}
