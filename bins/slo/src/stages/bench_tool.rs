//! The bench tool token (docs/superpowers/specs/2026-09-14-bench-tool-credential-design.md): the
//! api mints it into a Secret the bench pod mounts, the pod's tools call `/v1` with it, and it dies
//! with its parent login or the bench's stop.
//!
//! Every login here is the probe's OWN, minted per run under a `run-{id}` device name (the name
//! sweep collects a leftover), so revoking one never touches a sibling group's credential. The pod
//! calls run inside the bench container by exec and print a status and a clipped body, never the
//! token. The one exception is the stop check: a stop deletes the pod, so an exec after it races the
//! kubelet — the probe reads the first token out of the pod once, holds it in memory only, and asks
//! from here; the gate that refuses it is the same one either way.
//!
//! Walked after `bench::hourly` in group 3, so it never resets `bench.idle.wake`'s wait, and it
//! restarts the bench before returning because group 0's tool round trip dials it next.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use futures::FutureExt;
use kloudlite_workspaces::{crd, k8s};
use serde_json::Value;

use super::{api, call, raw};
use crate::ctx::Ctx;

/// `bench.tool.token`: target 120 s, most of it the kubelet syncing the projected Secret.
const TOKEN_CEILING: Duration = Duration::from_secs(120);
const AUDIENCE_CEILING: Duration = Duration::from_secs(30);
/// `bench.tool.revoked`: target 90 s — up to 60 s for the revocation to reach the gate, then a stop.
const REVOKED_CEILING: Duration = Duration::from_secs(90);
const REVOKE_WINDOW: Duration = Duration::from_secs(60);
const EXEC: Duration = Duration::from_secs(20);

const IDS: [&str; 3] = ["bench.tool.token", "bench.tool.audience", "bench.tool.revoked"];

/// `(status, body)` of one `/v1` call from inside the pod with the mounted token. Status 0 is a
/// fetch that never answered (no `KL_API_URL`, no file): a failure, never a refusal.
const CALL_JS: &str = r#"const [p,m]=process.argv.slice(1);let t="";try{t=require("fs").readFileSync(process.env.KL_TOOL_TOKEN_FILE,"utf8").trim()}catch{}fetch((process.env.KL_API_URL||"")+p,{method:m,headers:{authorization:"Bearer "+t}}).then(async r=>{console.log(r.status);console.log((await r.text()).slice(0,300))}).catch(e=>{console.log(0);console.log(String(e).slice(0,300))})"#;
/// A digest of the mounted token, empty while there is none: enough to see a new one land.
const DIGEST_JS: &str = r#"let d="";try{d=require("fs").readFileSync(process.env.KL_TOOL_TOKEN_FILE,"utf8").trim()}catch{}console.log(d?require("crypto").createHash("sha256").update(d).digest("hex"):"")"#;
const READ_JS: &str = r#"process.stdout.write(require("fs").readFileSync(process.env.KL_TOOL_TOKEN_FILE,"utf8").trim())"#;

async fn node(c: &Ctx, argv: &[&str]) -> Result<String> {
    let k = c.kube.clone().context("no kubeconfig")?;
    let owner = &c.cfg.probe_user;
    let mut full = vec!["node", "-e"];
    full.extend_from_slice(argv);
    let pod = super::bench_pod(c, None).await?;
    let (code, out, _) =
        crate::kube::exec(&k, &crd::ws_namespace(owner, owner), &pod, Some(k8s::BENCH_CONTAINER), &full, EXEC).await?;
    if code != 0 {
        bail!("node in the bench pod exited {code}");
    }
    Ok(out)
}

async fn pod_call(c: &Ctx, method: &str, path: &str) -> Result<(u16, String)> {
    let out = node(c, &[CALL_JS, path, method]).await?;
    parse_call(&out)
}

fn parse_call(out: &str) -> Result<(u16, String)> {
    let (status, body) = out.split_once('\n').unwrap_or((out, ""));
    Ok((status.trim().parse().with_context(|| format!("no status in {:?}", super::clip(out)))?, body.trim().to_string()))
}

/// Wait for the mounted token's digest to differ from `old` and be non-empty.
async fn wait_new_token(c: &Ctx, old: &str, cap: Duration) -> Result<String> {
    let start = Instant::now();
    loop {
        let d = node(c, &[DIGEST_JS]).await.unwrap_or_default().trim().to_string();
        if !d.is_empty() && d != old {
            return Ok(d);
        }
        if start.elapsed() >= cap {
            bail!("no new tool token reached the pod within {} s", cap.as_secs());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn mint(c: &Ctx, cli: &str) -> Result<()> {
    let (status, text) = raw(c, reqwest::Method::POST, &api(c, "/v1/bench/tool-token"), cli, None, &[]).await?;
    if status != reqwest::StatusCode::NO_CONTENT {
        bail!("POST /v1/bench/tool-token answered {status}: {}", super::clip(&text));
    }
    Ok(())
}

/// For group 0's tool round trip, after this group's restart: a fresh `run-{id}` login's tool token
/// minted and seen in the pod. `Ok(login id)` to revoke after the step; `Err(why)` is a skip — the
/// probe could not hand the bench a token, so the round trip would measure nothing.
pub(crate) async fn arm(c: &Ctx) -> std::result::Result<String, String> {
    if c.kube.is_none() {
        return Err("no kubeconfig to see the tool token reach the bench pod".into());
    }
    if super::bench::wait_phase(c, "ready", Duration::from_secs(30)).await.is_err() {
        return Err("the probe bench is not running (group 3's restart did not complete)".into());
    }
    let why = |what: &str, e: anyhow::Error| format!("{what}: {}", super::clip(&format!("{e:#}")));
    let (cli, id) = super::experience_gaps::cli_login(c, &c.probe_jwt, &format!("{}-bench-roundtrip", c.prefix()))
        .await
        .map_err(|e| why("no CLI login for the probe", e))?;
    let old = node(c, &[DIGEST_JS]).await.unwrap_or_default().trim().to_string();
    let armed = async {
        mint(c, &cli).await.map_err(|e| why("the tool token mint was refused", e))?;
        wait_new_token(c, &old, TOKEN_CEILING).await.map_err(|e| why("the tool token never reached the pod", e))
    };
    match armed.await {
        Ok(_) => Ok(id),
        Err(e) => {
            let _ = revoke_login(c, &id).await;
            Err(e)
        }
    }
}

pub(crate) async fn revoke_login(c: &Ctx, id: &str) -> Result<()> {
    call(c, reqwest::Method::DELETE, &api(c, &format!("/v1/cli/tokens/{id}")), &c.probe_jwt, None).await.map(|_| ())
}

fn refused(status: u16) -> bool {
    matches!(status, 401 | 403)
}

pub async fn run(c: &mut Ctx) {
    if c.kube.is_none() {
        return IDS.iter().for_each(|id| c.skip(id, "no kubeconfig"));
    }
    // Untimed: the bench journey before this may have left it anything but ready, and a pod that is
    // not there measures nothing about the token.
    if let Err(e) = super::bench::wait_phase(c, "ready", Duration::from_secs(90)).await {
        let why = format!("the bench is not ready: {}", super::clip(&format!("{e:#}")));
        return IDS.iter().for_each(|id| c.skip(id, &why));
    }
    let prefix = c.prefix();
    let login = super::experience_gaps::cli_login(c, &c.probe_jwt.clone(), &format!("{prefix}-bench-tool")).await;
    let (cli, cli_id) = match login {
        Ok(l) => l,
        Err(e) => {
            let why = format!("no CLI login for the probe: {}", super::clip(&format!("{e:#}")));
            return IDS.iter().for_each(|id| c.skip(id, &why));
        }
    };
    let old = node(c, &[DIGEST_JS]).await.unwrap_or_default().trim().to_string();
    let cli_t = cli.clone();
    let minted = c
        .step("bench.tool.token", TOKEN_CEILING, move |c| {
            async move {
                mint(c, &cli_t).await?;
                wait_new_token(c, &old, TOKEN_CEILING).await?;
                let (status, body) = pod_call(c, "GET", "/v1/regions").await?;
                if status != 200 || serde_json::from_str::<Value>(&body).is_err() {
                    bail!("/v1/regions from the pod answered {status}: {}", super::clip(&body));
                }
                Ok(())
            }
            .boxed()
        })
        .await;
    if !minted {
        c.skip("bench.tool.audience", "the pod never held a working tool token");
        c.skip("bench.tool.revoked", "the pod never held a working tool token");
        let _ = revoke_login(c, &cli_id).await;
        return;
    }
    c.step("bench.tool.audience", AUDIENCE_CEILING, |c| {
        async move {
            for (m, p) in [("POST", "/v1/bench/session"), ("GET", "/v1/cli/tokens"), ("GET", "/v1/keys")] {
                let (status, body) = pod_call(c, m, p).await?;
                if !refused(status) {
                    bail!("{m} {p} from the pod answered {status}: {}", super::clip(&body));
                }
            }
            Ok(())
        }
        .boxed()
    })
    .await;
    revoked(c, &prefix).await;
    let _ = revoke_login(c, &cli_id).await;
    // Untimed teardown: group 0's tool round trip dials this bench after this group ends.
    let restarted = async {
        call(c, reqwest::Method::POST, &api(c, "/v1/bench/start"), &c.probe_jwt, None).await?;
        super::bench::wait_phase(c, "ready", Duration::from_secs(120)).await
    };
    if let Err(e) = restarted.await {
        tracing::warn!(error = %format!("{e:#}"), "slo.bench_tool.restart");
    }
}

/// A throwaway login's token is minted, the login revoked, and a pod call polled for 401; then the
/// first login's token, still good, stops working the moment the bench is stopped.
async fn revoked(c: &mut Ctx, prefix: &str) {
    // Untimed prep: the kubelet's Secret sync is not the revocation's latency. The first token is
    // read BEFORE the throwaway one overwrites the Secret: its parent stays alive, so only the stop
    // can kill it.
    // An api refusal here is a platform fault and fails the id; only the kubelet being slow skips it.
    let prep = async {
        let live = node(c, &[READ_JS]).await?;
        let before = node(c, &[DIGEST_JS]).await?.trim().to_string();
        let (doomed, doomed_id) = super::experience_gaps::cli_login(c, &c.probe_jwt, &format!("{prefix}-bench-tool-rev")).await?;
        mint(c, &doomed).await?;
        anyhow::Ok((live, before, doomed_id))
    }
    .await;
    let (live, before, doomed_id) = match prep {
        Ok(p) => p,
        Err(e) => {
            let why = format!("before the revocation: {e:#}");
            c.step("bench.tool.revoked", REVOKED_CEILING, move |_| async move { bail!("{why}") }.boxed()).await;
            return;
        }
    };
    if let Err(e) = wait_new_token(c, &before, TOKEN_CEILING).await {
        let _ = revoke_login(c, &doomed_id).await;
        return c.skip("bench.tool.revoked", &format!("before the revocation: {}", super::clip(&format!("{e:#}"))));
    }
    c.step("bench.tool.revoked", REVOKED_CEILING, move |c| {
        async move {
            revoke_login(c, &doomed_id).await.context("could not revoke the throwaway login")?;
            let start = Instant::now();
            loop {
                let (status, body) = pod_call(c, "GET", "/v1/regions").await?;
                if status == 401 {
                    break;
                }
                if status != 200 {
                    bail!("/v1/regions from the pod answered {status} after the revocation: {}", super::clip(&body));
                }
                if start.elapsed() >= REVOKE_WINDOW {
                    bail!("the pod's token still answers 200 {} s after its login was revoked", REVOKE_WINDOW.as_secs());
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            tracing::info!(check = "parent.revoked", "slo.bench_tool");
            stop_kills(c, &live).await
        }
        .boxed()
    })
    .await;
}

async fn stop_kills(c: &Ctx, token: &str) -> Result<()> {
    let url = api(c, "/v1/regions");
    let (status, _) = raw(c, reqwest::Method::GET, &url, token, None, &[]).await?;
    if !status.is_success() {
        bail!("the live login's tool token answered {status} before the stop");
    }
    call(c, reqwest::Method::POST, &api(c, "/v1/bench/stop"), &c.probe_jwt, None).await?;
    let (status, _) = raw(c, reqwest::Method::GET, &url, token, None, &[]).await?;
    if status != reqwest::StatusCode::UNAUTHORIZED {
        bail!("a stopped bench's tool token answered {status}, not 401");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pod_call_parses_and_ceilings_cover_targets() {
        assert_eq!(parse_call("401\n{\"error\":\"x\"}\n").unwrap(), (401, "{\"error\":\"x\"}".to_string()));
        assert_eq!(parse_call("0\nTypeError: fetch failed").unwrap().0, 0);
        assert!(parse_call("").is_err());
        assert!(refused(401) && refused(403) && !refused(200) && !refused(0));
        use kloudlite_workspaces::slo::catalogue::find;
        for (id, cap) in [("bench.tool.token", TOKEN_CEILING), ("bench.tool.revoked", REVOKED_CEILING)] {
            assert!(cap.as_millis() >= find(id).unwrap().target.max_ms.unwrap() as u128, "{id}");
        }
        for id in IDS {
            assert_eq!(crate::suite::group_of(id), 3, "{id}");
        }
    }
}
