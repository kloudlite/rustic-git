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
//! (`SSH_SESSION_TTL_SECS`) and the bench is marked paused only on the api's keys beat (300 s), so
//! a pre-pause token reads as 401 there — expiry, not the pause. The probe mints the ticket itself
//! from `kloudlite-jwt` once the bench reads paused, which is a valid ticket the gateway can refuse
//! only for the reason being measured.
//!
//! The unpause runs under `drill::undoing`, so a failed or timed-out body never leaves the probe
//! member paused; a killed pod leaves a `run-`-prefixed team the next run's sweep deletes.

use std::time::Instant;

use super::*;
use kloudlite_workspaces::k8s;
use crate::stages::call;

pub(crate) const PAUSED_ID: &str = "team.member.paused";
/// `bound(360_000)`: the 60 s refusal window, the unpause, a cold bench start and the canary read.
///
/// Raised from 240 s with the number measured on the fleet (2026-09-18): a fresh team bench takes
/// ~120 s from create to `Ready` and one took 480 s, of which the packages are 1–17 s — the rest
/// is `harness-bench --ping` not serving yet, and the pod is not Ready until it does. The shell
/// sidecar is no longer part of this (89dbef10); this is the sessions container's own start.
const PAUSED_BODY: Duration = Duration::from_secs(360);
pub(super) const PAUSED_CEILING: Duration = Duration::from_secs(PAUSED_BODY.as_secs() + UNDO_SLACK);
/// Pause reconciles the member's bench at once (`on_member_state`); the rest is the api's
/// `membership.forget` and the gate's cache — seconds, with room for a slow bench patch.
const PAUSE_WINDOW: Duration = Duration::from_secs(60);
/// 120 s was exactly the median a fresh bench needed, so half the runs lost the race. Measured
/// again on 2026-09-18: create to `Ready` was 120 s on one bench and 480 s on another.
const READY_WAIT: Duration = Duration::from_secs(240);
const EXEC: Duration = Duration::from_secs(20);
/// What the canary file holds — and what the read must give back. The PATH is derived in the pod
/// (`canary_js`), so the marker is the only constant either side shares.
const CANARY: &str = "kloudlite-slo-pause-canary";

/// The canary's path, computed INSIDE the bench container from the same two facts the container
/// itself is built from: `HOME` (the live worktree IS the home since 2026-09-22) and
/// `k8s::BENCH_SUBDIR`, which is what `harness-bench --dir` is given as `{HOME}/.bench`. It read
/// `KL_WORKSPACE`, which is `~/workspace` since that ruling, and ENOENT'd (hourly 2026-09-23). It used
/// to be the literal `/bench`, the retired Bench pod's own mount — nothing mounts that now, and
/// the write failed `ENOENT` on every run (hourly, 2026-09-17). One expression for the write and
/// the read, so the two can never drift apart.
fn canary_js(body: &str) -> String {
    format!(
        r#"const w=process.env.HOME;if(!w){{console.error("no HOME in the bench container");process.exit(2)}}const p=require("path").join(w,{subdir:?},".slo-canary");{body}"#,
        subdir = k8s::BENCH_SUBDIR,
    )
}
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
    // Named by the bench's workspace id, asked for rather than assumed — see `stages::bench_pod`.
    let pod = super::super::bench_pod(c, Some(team)).await?;
    let (code, out, err) = crate::kube::exec(k, &ns, &pod, Some(k8s::BENCH_CONTAINER), &["node", "-e", js, arg], EXEC).await?;
    if code != 0 {
        return Err(anyhow!("node in the member's bench exited {code}: {}", clip(&err)));
    }
    Ok(out)
}

async fn ready(c: &Ctx, team: &str, cap: Duration) -> Result<()> {
    poll_json(c, &bench_url(c, "", team), &c.probe_jwt, cap, |v| v.get("phase").and_then(Value::as_str) == Some("ready")).await
}

/// Genuinely back up, not a `ready` left over from before the pause.
///
/// On 2026-09-16 (hourly-1789515300-g2) the unpause, the start and this check all landed inside
/// 0.3 s, `ready()`'s first read answered with the phase from BEFORE the pause — the stop had not
/// reached it yet — and the exec met a pod that no longer existed. Every clause is a fact the
/// restart itself establishes: the spec carries the unpause and the start, and the status names a
/// pod, which the stop cleared and only a new pod restores.
fn back_up(b: Option<&crd::Workspace>) -> bool {
    let Some(b) = b else { return false };
    crd::is_bench(b)
        && b.spec.access == crd::Access::Full
        && b.spec.desired_state == crd::DesiredState::Running
        && b.status.as_ref().is_some_and(|s| s.phase == crd::Phase::Ready && s.pod_ref.is_some())
}

/// The bench object itself, through the same client the exec uses, so the check and the exec cannot
/// be looking at two different objects. A bench IS a Workspace (`crd::is_bench`) — the retired
/// `Bench` kind is gone from the cluster, and reading it answered `benches.kloudlite.io
/// "bench-…" not found` on every run (hourly, 2026-09-17).
async fn back_up_within(c: &Ctx, team: &str, cap: Duration) -> Result<()> {
    let k = c.kube.as_ref().ok_or_else(|| anyhow!("no kubeconfig"))?;
    crate::kube::wait_for::<crd::Workspace>(k, &crd::bench_id(&c.probe_user, team), cap, back_up).await
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
    in_bench(c, team, &canary_js(r#"require("fs").writeFileSync(p,process.argv[1])"#), CANARY)
        .await
        .context("could not write the canary")?;
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
    let b = kube::Api::<crd::Workspace>::all(k).get(&id).await.context("could not read the member's bench")?;
    if !crd::is_bench(&b) {
        return Err(anyhow!("{id} is not a bench workspace"));
    }
    if b.spec.access != crd::Access::Paused || b.spec.desired_state != crd::DesiredState::Stopped {
        return Err(anyhow!("the bench is {:?}/{:?}, not paused and stopped", b.spec.access, b.spec.desired_state));
    }
    // And the STATUS has caught up with the pause: the agent has torn the pod down and cleared
    // `podRef`. Without this the pre-pause `ready` is still readable when the unpause runs, and
    // the readiness check below cannot tell it from the one the restart earns.
    let st = b.status.as_ref();
    if st.is_none_or(|s| s.phase == crd::Phase::Ready || s.pod_ref.is_some()) {
        return Err(anyhow!("the bench still reads {:?}, with a pod", st.map(|s| s.phase)));
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
                back_up_within(c, &t, PAUSED_BODY.saturating_sub(start.elapsed())).await.context("the bench never came back")?;
                let got = in_bench(c, &t, &canary_js(r#"process.stdout.write(require("fs").readFileSync(p,"utf8"))"#), "").await?;
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
mod canary_tests {
    use super::*;

    /// The path is the pod's to compute, from the two facts the bench container is built from.
    /// A literal `/bench` here is the 2026-09-17 failure: nothing mounts it any more.
    #[test]
    fn the_canary_path_comes_from_the_containers_own_env() {
        let js = canary_js("read(p)");
        assert!(js.contains("process.env.HOME"), "{js}");
        assert!(js.contains(&format!("{:?}", k8s::BENCH_SUBDIR)), "{js}");
        assert!(js.ends_with("read(p)"), "{js}");
        assert!(!js.contains("\"/bench\""), "the retired Bench mount is back: {js}");
    }
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

    /// The 16 Sep failure as a predicate: the phase from before the pause must not pass for the
    /// restart, and neither must a spec the unpause or the start has not reached.
    #[test]
    fn a_stale_ready_is_not_back_up() {
        let bench = |access, desired, phase, pod: Option<&str>| {
            let mut b = crd::Workspace::new(
                "b",
                crd::WorkspaceSpec {
                    trees: Vec::new(),
                    owner: "p".into(),
                    team: "t".into(),
                    name: "bench".into(),
                    region: "r1".into(),
                    image: "i".into(),
                    storage: None,
                    desired_state: desired,
                    resources: Default::default(),
                    packages: vec![],
                    locks: vec![],
                    attached_environment: None,
                    // What MAKES it a bench, and the first thing `back_up` checks.
                    bench: Some(crd::BenchOptions { model: "m".into(), wake_at: None }),
                    access,
                },
            );
            b.status = Some(crd::WorkspaceStatus { phase, pod_ref: pod.map(Into::into), ..Default::default() });
            b
        };
        use crd::{Access::*, DesiredState::*, Phase};
        let up = bench(Full, Running, Phase::Ready, Some("ns/bench"));
        assert!(back_up(Some(&up)));
        // An ordinary workspace of the same shape is not this team's bench.
        let mut plain = up.clone();
        plain.spec.bench = None;
        assert!(!back_up(Some(&plain)));
        // Ready, but the stop cleared the pod: the very read that passed on 16 Sep.
        assert!(!back_up(Some(&bench(Full, Running, Phase::Ready, None))));
        assert!(!back_up(Some(&bench(Paused, Running, Phase::Ready, Some("ns/bench")))));
        assert!(!back_up(Some(&bench(Full, Stopped, Phase::Ready, Some("ns/bench")))));
        assert!(!back_up(Some(&bench(Full, Running, Phase::Starting, Some("ns/bench")))));
        assert!(!back_up(None));
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
