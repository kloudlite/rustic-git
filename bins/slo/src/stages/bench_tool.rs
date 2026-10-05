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

use super::{api, call};
use crate::ctx::Ctx;

/// `bench.tool.token`: target 120 s, most of it the kubelet syncing the projected Secret.
const TOKEN_CEILING: Duration = Duration::from_secs(120);
const AUDIENCE_CEILING: Duration = Duration::from_secs(30);
const EXEC: Duration = Duration::from_secs(20);

const IDS: [&str; 2] = ["bench.tool.token", "bench.tool.audience"];

/// `(status, body)` of one `/v1` call from inside the pod with the mounted token. Status 0 is a
/// fetch that never answered (no `KL_API_URL`, no file): a failure, never a refusal.
const CALL_JS: &str = r#"const [p,m]=process.argv.slice(1);let t="";try{t=require("fs").readFileSync(process.env.KL_TOOL_TOKEN_FILE,"utf8").trim()}catch{}fetch((process.env.KL_API_URL||"")+p,{method:m,headers:{authorization:"Bearer "+t}}).then(async r=>{console.log(r.status);console.log((await r.text()).slice(0,300))}).catch(e=>{console.log(0);console.log(String(e).slice(0,300))})"#;
/// A digest of the mounted token, empty while there is none: enough to see a new one land.
const DIGEST_JS: &str = r#"let d="";try{d=require("fs").readFileSync(process.env.KL_TOOL_TOKEN_FILE,"utf8").trim()}catch{}console.log(d?require("crypto").createHash("sha256").update(d).digest("hex"):"")"#;

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

/// Mints and applies the bench's `bench-tool` Secret directly from the probe's own
/// `kloudlite-jwt`, the way the api's resync beat does — the route that used to do this on demand
/// is gone (ruling 3, task 9 brief).
async fn mint(c: &Ctx) -> Result<()> {
    c.mint_bench_tool(&c.cfg.probe_user, &c.cfg.probe_user).await
}

/// For group 0's tool round trip, after this group's restart: a fresh tool token minted and seen
/// in the pod. `Err(why)` is a skip — the probe could not hand the bench a token, so the round
/// trip would measure nothing.
pub(crate) async fn arm(c: &Ctx) -> std::result::Result<(), String> {
    if c.kube.is_none() {
        return Err("no kubeconfig to see the tool token reach the bench pod".into());
    }
    if super::bench::wait_phase(c, "ready", Duration::from_secs(30)).await.is_err() {
        return Err("the probe bench is not running (group 3's restart did not complete)".into());
    }
    let why = |what: &str, e: anyhow::Error| format!("{what}: {}", super::clip(&format!("{e:#}")));
    let old = node(c, &[DIGEST_JS]).await.unwrap_or_default().trim().to_string();
    mint(c).await.map_err(|e| why("the tool token mint was refused", e))?;
    wait_new_token(c, &old, TOKEN_CEILING).await.map_err(|e| why("the tool token never reached the pod", e))?;
    Ok(())
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
    let old = node(c, &[DIGEST_JS]).await.unwrap_or_default().trim().to_string();
    let minted = c
        .step("bench.tool.token", TOKEN_CEILING, move |c| {
            async move {
                mint(c).await?;
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
    // Untimed teardown: group 0's tool round trip dials this bench after this group ends.
    let restarted = async {
        call(c, reqwest::Method::POST, &api(c, "/v1/bench/start"), &c.probe_jwt, None).await?;
        super::bench::wait_phase(c, "ready", Duration::from_secs(120)).await
    };
    if let Err(e) = restarted.await {
        tracing::warn!(error = %format!("{e:#}"), "slo.bench_tool.restart");
    }
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
        let (id, cap) = ("bench.tool.token", TOKEN_CEILING);
        assert!(cap.as_millis() >= find(id).unwrap().target.max_ms.unwrap() as u128, "{id}");
        for id in IDS {
            assert_eq!(crate::suite::group_of(id), 3, "{id}");
        }
    }
}
