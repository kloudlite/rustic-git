//! `team.member.paused`: a paused member loses every surface at once, and nothing of theirs.
//!
//! Its own team (`run-{id}-pause`), so pausing the probe member never touches the team group 0
//! walks or the intercept team group 1 stands up; hence group 2 (`suite::group_of`). A pause is
//! per team membership, so the probe tenant stays whole in every other journey.
//!
//! Roles: the PROBE tenant is the paused member and owns the bench and canary, because a bench
//! create charges its caller and the second tenant's Quota is all zeros by design (it exists only
//! to be refused — the 15 Sep fleet 409). The second tenant owns the team and pauses; the crash
//! sweep finds the team under its JWT.
//!
//! The gateway half cannot use a session token minted before the pause: those live 60 s
//! (`SSH_SESSION_TTL_SECS`) and the Bench is marked paused only on the api's keys beat (300 s), so
//! a pre-pause token reads as 401 there — expiry, not the pause. The probe mints the ticket itself
//! from `kloudlite-jwt` once the Bench reads paused, which is a valid ticket the gateway can refuse
//! only for the reason being measured.
//!
//! The unpause runs under `drill::undoing`, so a failed or timed-out body never leaves the probe
//! member paused; a killed pod leaves a `run-`-prefixed team the next run's sweep deletes.

use std::time::Instant;

use super::*;
use kloudlite_workspaces::k8s;
use crate::stages::call;

pub(crate) const PAUSED_ID: &str = "team.member.paused";
/// `bound(240_000)`: the 60 s refusal window, the unpause, a cold bench start and the canary read.
const PAUSED_BODY: Duration = Duration::from_secs(240);
pub(super) const PAUSED_CEILING: Duration = Duration::from_secs(PAUSED_BODY.as_secs() + UNDO_SLACK);
/// Pause reconciles the member's Bench at once (`on_member_state`); the rest is the api's
/// `membership.forget` and the gate's cache — seconds, with room for a slow Bench patch.
const PAUSE_WINDOW: Duration = Duration::from_secs(60);
const READY_WAIT: Duration = Duration::from_secs(120);
const EXEC: Duration = Duration::from_secs(20);
const CANARY: &str = "/bench/.slo-canary";
const TOKEN_JS: &str = r#"let d="";try{d=require("fs").readFileSync(process.env.KL_TOOL_TOKEN_FILE,"utf8").trim()}catch{}process.stdout.write(d)"#;

pub(super) fn pause_team(c: &Ctx) -> String {
    format!("{}-pause", c.prefix())
}

fn bench_url(c: &Ctx, path: &str, team: &str) -> String {
    api(c, &format!("/v1/bench{path}?team={team}"))
}

struct Prep {
    tool: String,
    gateway: String,
    cli_id: String,
}

async fn in_bench(c: &Ctx, team: &str, js: &str, arg: &str) -> Result<String> {
    let k = c.kube.as_ref().ok_or_else(|| anyhow!("no kubeconfig"))?;
    let ns = crd::ws_namespace(&c.probe_user, team);
    let (code, out, err) = crate::kube::exec(k, &ns, k8s::BENCH_POD, Some(k8s::BENCH_CONTAINER), &["node", "-e", js, arg], EXEC).await?;
    if code != 0 {
        return Err(anyhow!("node in the member's bench exited {code}: {}", clip(&err)));
    }
    Ok(out)
}

async fn ready(c: &Ctx, team: &str, cap: Duration) -> Result<()> {
    poll_json(c, &bench_url(c, "", team), &c.probe_jwt, cap, |v| v.get("phase").and_then(Value::as_str) == Some("ready")).await
}

/// The paused member's bench, charged to the probe tenant — the one with a Quota.
async fn make_bench(c: &Ctx, team: &str) -> Result<Value> {
    post(c, &api(c, "/v1/bench"), &c.probe_jwt, serde_json::json!({ "team": team, "region": c.cfg.region }))
        .await
        .context("could not create the member's bench")
}

/// Team with the member in it, their bench ready with a canary, a tool token in the pod, and the
/// gateway URL a session answers with.
async fn prepare(c: &Ctx, team: &str) -> Result<Prep> {
    let body = serde_json::json!({ "slug": team, "name": "kloudlite slo pause", "region": c.cfg.region });
    post(c, &api(c, "/v1/teams"), &c.other_jwt, body).await.context("could not create the pause team")?;
    let invite = serde_json::json!({ "email": c.probe_email, "role": "member" });
    let issued = post(c, &api(c, &format!("/v1/teams/{team}/invites")), &c.other_jwt, invite).await.context("could not invite")?;
    let token = issued.get("token").and_then(Value::as_str).filter(|t| !t.is_empty()).ok_or_else(|| anyhow!("no invite token"))?;
    post(c, &api(c, &format!("/v1/invites/{token}/accept")), &c.probe_jwt, Value::Null).await.context("the member could not join")?;
    make_bench(c, team).await?;
    ready(c, team, READY_WAIT).await.context("the member's bench never became ready")?;
    in_bench(c, team, r#"require("fs").writeFileSync(process.argv[1],process.argv[1])"#, CANARY).await.context("could not write the canary")?;
    let (cli, cli_id) = super::super::experience_gaps::cli_login(c, &c.probe_jwt, &format!("{team}-tool")).await?;
    let prep = async {
        let (status, text) = raw(c, reqwest::Method::POST, &bench_url(c, "/tool-token", team), &cli, None, &[]).await?;
        if status != reqwest::StatusCode::NO_CONTENT {
            return Err(anyhow!("tool-token mint answered {status}: {}", clip(&text)));
        }
        let start = Instant::now();
        let tool = loop {
            let t = in_bench(c, team, TOKEN_JS, "").await.unwrap_or_default();
            if !t.trim().is_empty() {
                break t.trim().to_string();
            }
            if start.elapsed() >= READY_WAIT {
                return Err(anyhow!("no tool token reached the member's bench"));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        };
        let session = post(c, &bench_url(c, "/session", team), &c.probe_jwt, Value::Null).await.context("no bench session")?;
        let gateway = session.get("gateway").and_then(Value::as_str).ok_or_else(|| anyhow!("the session named no gateway"))?;
        anyhow::Ok((tool, gateway.to_string()))
    }
    .await;
    match prep {
        Ok((tool, gateway)) => Ok(Prep { tool, gateway, cli_id }),
        Err(e) => {
            revoke(c, &cli_id).await;
            Err(e)
        }
    }
}

async fn revoke(c: &Ctx, id: &str) {
    let _ = call(c, reqwest::Method::DELETE, &api(c, &format!("/v1/cli/tokens/{id}")), &c.probe_jwt, None).await;
}

/// One pass over the five surfaces; `Err` names the first that still lets the member in.
async fn refused_everywhere(c: &Ctx, team: &str, p: &Prep) -> Result<()> {
    let (status, body) = raw(c, reqwest::Method::GET, &api(c, "/v1/regions"), &p.tool, None, &[]).await?;
    if status != reqwest::StatusCode::UNAUTHORIZED {
        return Err(anyhow!("the tool token answered {status}: {}", clip(&body)));
    }
    let (status, body) = raw(c, reqwest::Method::GET, &api(c, &format!("/v1/workspaces?team={team}")), &c.probe_jwt, None, &[]).await?;
    if status != reqwest::StatusCode::FORBIDDEN || !body.contains(&paused_sentence(team)) {
        return Err(anyhow!("the team listing answered {status}: {}", clip(&body)));
    }
    // The refusal a desktop client actually meets, one hop before the gateway.
    let (status, body) = raw(c, reqwest::Method::POST, &bench_url(c, "/session", team), &c.probe_jwt, None, &[]).await?;
    if status != reqwest::StatusCode::FORBIDDEN || !body.contains(&paused_sentence(team)) {
        return Err(anyhow!("a bench session answered {status}: {}", clip(&body)));
    }
    let k = c.kube.clone().ok_or_else(|| anyhow!("no kubeconfig"))?;
    let id = crd::bench_id(&c.probe_user, team);
    let b = kube::Api::<crd::Bench>::all(k).get(&id).await.context("could not read the member's Bench")?;
    if b.spec.access != crd::BenchAccess::Paused || b.spec.desired_state != crd::DesiredState::Stopped {
        return Err(anyhow!("the Bench is {:?}/{:?}, not paused and stopped", b.spec.access, b.spec.desired_state));
    }
    let ticket = c.mint_bench_session(&c.probe_user, &id)?;
    // A real handshake, not reqwest with upgrade headers: the edge in front of the gateway
    // answered that imitation 400 before the handler could refuse it (hourly-1789496344-g2).
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Error as WsError};
    let mut req = p.gateway.as_str().into_client_request().context("the session's gateway URL")?;
    req.headers_mut().insert("authorization", c.bearer(&ticket).parse().context("ticket header")?);
    match tokio_tungstenite::connect_async(req).await {
        Err(WsError::Http(resp)) if resp.status() == 403 => {}
        Err(WsError::Http(resp)) => return Err(anyhow!("the gateway tunnel answered {}, not 403", resp.status())),
        Err(e) => return Err(anyhow!("the gateway did not answer: {e}")),
        Ok(_) => return Err(anyhow!("the gateway tunnel opened for a paused member, not 403")),
    }
    Ok(())
}

pub(super) fn paused_sentence(team: &str) -> String {
    format!("your access to {team} is paused")
}

pub(crate) async fn member_paused(c: &mut Ctx) {
    if c.kube.is_none() {
        return c.skip(PAUSED_ID, "no kubeconfig");
    }
    let team = pause_team(c);
    let prep = match prepare(c, &team).await {
        Ok(p) => p,
        Err(e) => {
            let why = format!("before the pause: {e:#}");
            c.step(PAUSED_ID, PAUSED_CEILING, move |_| async move { Err(anyhow!("{why}")) }.boxed()).await;
            return teardown(c, &team).await;
        }
    };
    let (t, gw, tool) = (team.clone(), prep.gateway.clone(), prep.tool.clone());
    c.step(PAUSED_ID, PAUSED_CEILING, move |c| {
        async move {
            let p = Prep { tool, gateway: gw, cli_id: String::new() };
            let member = format!("/v1/teams/{t}/members/{}", c.probe_email);
            let (pause, unpause) = (api(c, &format!("{member}/pause")), api(c, &format!("{member}/unpause")));
            let c: &Ctx = c;
            let body = async {
                let start = Instant::now();
                call(c, reqwest::Method::POST, &pause, &c.other_jwt, None).await.context("could not pause the member")?;
                loop {
                    match refused_everywhere(c, &t, &p).await {
                        Ok(()) => break,
                        Err(e) if start.elapsed() >= PAUSE_WINDOW => {
                            return Err(e.context(format!("not refused within {} s", PAUSE_WINDOW.as_secs())))
                        }
                        Err(_) => tokio::time::sleep(Duration::from_secs(5)).await,
                    }
                }
                call(c, reqwest::Method::POST, &unpause, &c.other_jwt, None).await.context("could not unpause the member")?;
                // Accepted either way, but parked on `access: paused` until the unpause's own
                // reconcile writes `access: full`, which it has by the time the unpause answers.
                call(c, reqwest::Method::POST, &bench_url(c, "/start", &t), &c.probe_jwt, None).await.context("could not start the bench")?;
                ready(c, &t, PAUSED_BODY.saturating_sub(start.elapsed())).await.context("the bench never came back")?;
                let got = in_bench(c, &t, r#"process.stdout.write(require("fs").readFileSync(process.argv[1],"utf8"))"#, CANARY).await?;
                if got != CANARY {
                    return Err(anyhow!("the canary reads back as {:?}", clip(&got)));
                }
                Ok(())
            };
            let undo = || async { call(c, reqwest::Method::POST, &unpause, &c.other_jwt, None).await.map(|_| ()) };
            undoing(PAUSED_BODY, body, undo).await
        }
        .boxed()
    })
    .await;
    revoke(c, &prep.cli_id).await;
    teardown(c, &team).await;
}

/// Best effort; the `run-` prefix sweep takes whatever this misses. No drain before the team
/// delete, unlike the intercept journey: this team holds only a bench, which never blocks
/// `delete_team` the way a workspace does.
async fn teardown(c: &Ctx, team: &str) {
    let _ = call(c, reqwest::Method::POST, &bench_url(c, "/stop", team), &c.probe_jwt, None).await;
    if let Err(e) = call(c, reqwest::Method::DELETE, &api(c, &format!("/v1/teams/{team}")), &c.other_jwt, None).await {
        tracing::warn!(kind = "team", op = "delete", name = %team, error = %format!("{e:#}"), "slo.teardown.failed");
        return;
    }
    super::super::env_intercept::delete_members_now(c, team).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceiling_covers_target_and_group_is_two() {
        let target = kloudlite_workspaces::slo::catalogue::find(PAUSED_ID).unwrap().target.max_ms.unwrap();
        assert!(PAUSED_BODY.as_millis() >= target as u128);
        assert_eq!(crate::suite::group_of(PAUSED_ID), 2);
        assert_eq!(paused_sentence("acme"), "your access to acme is paused");
    }

    /// The bench create and every read of it run as the probe tenant: the second tenant's Quota is
    /// all zeros, so a bench charged to it is the 409 this journey failed on (15 Sep 2026).
    #[tokio::test]
    async fn the_bench_is_the_probe_tenants() {
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let rec = seen.clone();
        let app = axum::Router::new().fallback(axum::routing::any(move |uri: axum::http::Uri, h: axum::http::HeaderMap| {
            let rec = rec.clone();
            async move {
                let auth = h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
                rec.lock().expect("recorder").push((uri.to_string(), auth));
                axum::http::StatusCode::NOT_FOUND
            }
        }));
        let mut c = crate::testkit::ctx_against(app).await;
        c.retry_delay = Duration::from_millis(1);
        let _ = make_bench(&c, "run-hourly-1-pause").await;
        let _ = ready(&c, "run-hourly-1-pause", Duration::from_millis(1)).await;
        let calls = seen.lock().expect("recorder").clone();
        assert!(calls.iter().any(|(u, _)| u == "/v1/bench") && calls.len() >= 2, "{calls:?}");
        assert!(c.probe_jwt != c.other_jwt);
        for (u, auth) in &calls {
            assert_eq!(auth, &c.bearer(&c.probe_jwt), "{u} ran as the wrong tenant");
        }
    }

    #[tokio::test]
    async fn no_kubeconfig_skips_once() {
        let mut c = crate::testkit::ctx().await;
        c.kube = None;
        member_paused(&mut c).await;
        let rows: Vec<_> = c.steps.iter().filter(|s| s.slo_id == PAUSED_ID).collect();
        assert!(rows.len() == 1 && rows[0].skipped);
    }
}
