//! The bench tool token (docs/superpowers/specs/2026-09-14-bench-tool-credential-design.md): the
//! api mints it into a Secret the bench pod mounts, the pod's tools call `/v1` with it, and it dies
//! with the bench's stop. The probe never mints this Secret itself (ruling 1, task 9 fix round 1):
//! every mint seen here comes from the api, on bench create/start or its keys beat.
//!
//! The pod calls run inside the bench container by exec and print a status and a clipped body,
//! never the token. The one exception is the stop check: a stop deletes the pod, so an exec after
//! it races the kubelet — the probe reads the live token out of the pod once, holds it in memory
//! only, and asks from here; the gate that refuses it is the same one either way.
//!
//! Walked after `bench::claude_tools` in group 3, so it never resets `bench.idle.wake`'s wait.

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
/// `bench.tool.revoked`: target 90 s — up to 60 s for the stop's teardown, then the recheck.
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

fn refused(status: u16) -> bool {
    matches!(status, 401 | 403)
}

async fn stop_kills(c: &Ctx, token: String) -> Result<()> {
    let token = token.as_str();
    let url = api(c, "/v1/regions");
    let (status, _) = raw(c, reqwest::Method::GET, &url, token, None, &[]).await?;
    if !status.is_success() {
        bail!("the live token answered {status} before the stop");
    }
    call(c, reqwest::Method::POST, &api(c, "/v1/bench/stop"), &c.probe_jwt, None).await?;
    let start = Instant::now();
    loop {
        let (status, _) = raw(c, reqwest::Method::GET, &url, token, None, &[]).await?;
        if refused(status.as_u16()) {
            return Ok(());
        }
        if start.elapsed() >= REVOKE_WINDOW {
            bail!("a stopped bench's tool token still answers {status}, not 401/403, {} s after the stop", REVOKE_WINDOW.as_secs());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// `bench.tool.revoked`: the live token the pod holds right now stops working once the bench is
/// stopped. Read it before the stop (a pod gone after the stop cannot be exec'd into).
async fn revoked(c: &mut Ctx) {
    let live = match node(c, &[READ_JS]).await {
        Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
        Ok(_) => return c.skip("bench.tool.revoked", "the pod holds no tool token to revoke"),
        Err(e) => return c.skip("bench.tool.revoked", &format!("reading the live token: {}", super::clip(&format!("{e:#}")))),
    };
    c.step("bench.tool.revoked", REVOKED_CEILING, move |c| stop_kills(c, live).boxed()).await;
    // Untimed teardown: group 0's tool round trip dials this bench after this group ends.
    let restarted = async {
        call(c, reqwest::Method::POST, &api(c, "/v1/bench/start"), &c.probe_jwt, None).await?;
        super::bench::wait_phase(c, "ready", Duration::from_secs(120)).await
    };
    if let Err(e) = restarted.await {
        tracing::warn!(error = %format!("{e:#}"), "slo.bench_tool.restart");
    }
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
    let ready = c
        .step("bench.tool.token", TOKEN_CEILING, move |c| {
            async move {
                call(c, reqwest::Method::POST, &api(c, "/v1/bench/start"), &c.probe_jwt, None).await?;
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
    if !ready {
        c.skip("bench.tool.audience", "the pod never held a working tool token");
        c.skip("bench.tool.revoked", "the pod never held a working tool token");
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
    revoked(c).await;
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
