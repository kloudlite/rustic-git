//! Stage 10 · Edge: the front door, and the telemetry pipeline everything else is judged through.
//!
//! The edge half is deliberately the only place the probe uses the host's own tools — `dig` and
//! `openssl` answer what a resolver and a TLS client outside the cluster would see, which is the
//! question, whereas an in-process lookup would go through the pod's own resolver and an
//! in-process handshake would hand back a verified session rather than the leaf's dates.
//!
//! The pipeline half asks the admin process rather than ClickHouse: the probe holds no ClickHouse
//! credential, by the same rule that keeps it a client of everything it measures.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;
use serde_json::Value;

use super::{admin, get};
use crate::ctx::Ctx;
use crate::tls;
use crate::tools;

/// Worst case 150 s if every step here times out (10 + 20 + 15 + 10 + 60 + 15 + 10 + 10), which
/// with stages 8 and 9's 60 s each is what these three stages may cost the fast suite's deadline.
const DNS_CEILING: Duration = Duration::from_secs(10);
const CERT_CEILING: Duration = Duration::from_secs(20);
const ORIGIN_CEILING: Duration = Duration::from_secs(15);
/// The dial itself gets 5 s: an SSH load balancer that has not accepted a TCP connection in five
/// seconds has failed this SLI whether the probe keeps waiting or not.
const SSH_DIAL: Duration = Duration::from_secs(5);
/// The catalogue bounds every `tel.*` id at 60 s, and for `tel.log.latency` that bound IS the
/// wait: a log line slower than this has missed the SLI already.
const TEL_CEILING: Duration = Duration::from_secs(60);
/// The two pipeline reads and the coverage read are plain gets, not waits.
const READ_CEILING: Duration = Duration::from_secs(15);
const PIPELINE_CEILING: Duration = Duration::from_secs(10);

/// A certificate this close to expiring is a page, not a surprise: two weeks is longer than any
/// renewal cycle here and longer than a weekend nobody is on call for.
// Cloudflare's edge certificates are renewed about ten days out, so 14 pages on every rotation.
const CERT_MIN_DAYS: i64 = 7;
/// Above this the consumer is keeping up; a real backlog is orders of magnitude larger.
const MAX_STREAM_PENDING: f64 = 1000.0;
/// ClickHouse holds 400-day rollups; under a fifth free is the point somebody must act.
const MIN_DISK_FREE_PCT: f64 = 20.0;

pub async fn run(c: &mut Ctx) {
    dns(c).await;
    cert(c).await;
    origin(c).await;
    ssh_lb(c).await;
    log_latency(c).await;
    pod_coverage(c).await;
    pipeline(c).await;
}

/// `edge.dns`: every public hostname resolves. One step for all of them — a fleet with one
/// unresolvable hostname is one broken front door, not a fraction of one.
async fn dns(c: &mut Ctx) {
    let hosts = c.cfg.hosts.clone();
    if hosts.is_empty() {
        return c.skip("edge.dns", "KLOUDLITE_SLO_HOSTS names no hostname");
    }
    c.step("edge.dns", DNS_CEILING, move |c| {
        let dig = c.programs.dig.clone();
        async move {
            for h in &hosts {
                let out = tools::plain(&dig, &["+short", h], DNS_CEILING)
                    .await
                    .with_context(|| format!("could not resolve {h}"))?;
                if out.trim().is_empty() {
                    return Err(anyhow!("{h} resolves to nothing"));
                }
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `edge.cert`: every hostname's certificate is still valid, with room to renew.
async fn cert(c: &mut Ctx) {
    let hosts = c.cfg.hosts.clone();
    if hosts.is_empty() {
        return c.skip("edge.cert", "KLOUDLITE_SLO_HOSTS names no hostname");
    }
    c.step("edge.cert", CERT_CEILING, move |c| {
        let (bash, openssl) = (c.programs.bash.clone(), c.programs.openssl.clone());
        async move {
            for h in &hosts {
                let out = tls::enddate(&bash, &openssl, h, CERT_CEILING)
                    .await
                    .with_context(|| format!("could not read {h}'s certificate"))?;
                let days = tls::days_left(&out, chrono::Utc::now())
                    .with_context(|| format!("{h}'s certificate"))?;
                if days <= CERT_MIN_DAYS {
                    return Err(anyhow!("{h}'s certificate expires in {days} days"));
                }
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `edge.origin`: the origin answers when reached directly, with the proxy's own SNI.
///
/// A dedicated client rather than `Ctx::http`: the whole point is to pin this hostname to the
/// ingress address the proxy uses, and reqwest's `resolve` does that without a `curl` in the image.
/// ANY status is a pass — a 404 from the origin is still the origin answering, and only a
/// connection failure is the outage this SLI is about.
async fn origin(c: &mut Ctx) {
    let (Some(ip), Some(host)) = (c.cfg.origin_ip.clone(), c.cfg.hosts.first().cloned()) else {
        return c.skip("edge.origin", "no KLOUDLITE_SLO_ORIGIN_IP, or no hostname to pin");
    };
    c.step("edge.origin", ORIGIN_CEILING, move |_| {
        async move {
            let addr: std::net::SocketAddr = format!("{ip}:443")
                .parse()
                .with_context(|| format!("KLOUDLITE_SLO_ORIGIN_IP {ip:?} is not an address"))?;
            let client = reqwest::Client::builder()
                .resolve(&host, addr)
                .timeout(ORIGIN_CEILING)
                .build()
                .context("could not build the pinned client")?;
            client
                .get(format!("https://{host}/"))
                .send()
                .await
                // `without_url`: the same rule as `stages::raw` — reqwest's Display carries the
                // whole URL, and a step detail is stored forever.
                .map_err(|e| anyhow!("the origin did not answer: {}", e.without_url()))?;
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `edge.ssh.lb`: the SSH load balancer accepts a TCP connection.
///
/// A bare dial, not an SSH handshake: `ssh.clone.ok` already covers the protocol, and what breaks
/// here is a load balancer with no healthy backend — which is visible at connect.
async fn ssh_lb(c: &mut Ctx) {
    let (host, port) = c.cfg.ssh_endpoint();
    let (host, port) = (host.to_string(), port);
    c.step("edge.ssh.lb", SSH_DIAL * 2, move |c| {
        let bash = c.programs.bash.clone();
        async move {
            // bash's own /dev/tcp, so nothing needs `nc` in the image.
            let script = format!("exec 3<>/dev/tcp/{host}/{port}");
            tools::plain(&bash, &["-c", &script], SSH_DIAL)
                .await
                .with_context(|| format!("could not connect to {host}:{port}"))?;
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `tel.log.latency`: a line this process logs reaches the collector's tables.
///
/// The marker is the run id, which is already on every other line the run writes — so the query
/// behind `/admin/slo/marker/{id}` is answering about the probe's real logs, not a special one.
async fn log_latency(c: &mut Ctx) {
    let run_id = c.run_id.clone();
    c.step("tel.log.latency", TEL_CEILING, move |c| {
        let jwt = c.admin_jwt.clone();
        let url = admin(c, &format!("/admin/slo/marker/{run_id}"));
        async move {
            tracing::info!(run_id = %run_id, "slo.marker");
            super::poll_json(c, &url, &jwt, TEL_CEILING, |v| {
                v.get("found").and_then(Value::as_bool) == Some(true)
            })
            .await
            .context("the marker never reached the collector's tables")
        }
        .boxed()
    })
    .await;
}

/// `tel.pod.coverage`: every workload of ours that has a ready pod is being scraped.
///
/// Matched on the `{workload}-` prefix rather than anywhere in the string: `service.instance.id`
/// is a pod name, which is the workload's name plus a hash, and a substring match would let
/// `kloudlite-api` be "covered" by a `kloudlite-api-admin` pod. A workload with at least
/// one instance reporting is the strongest claim the two lists can make together, and it catches
/// the failure that matters (a collector that stopped seeing a whole workload).
async fn pod_coverage(c: &mut Ctx) {
    c.step("tel.pod.coverage", READ_CEILING, |c| {
        let jwt = c.admin_jwt.clone();
        let (workloads, coverage) =
            (admin(c, "/admin/workloads"), admin(c, "/admin/slo/coverage"));
        async move {
            let rows = get(c, &workloads, &jwt).await.context("could not list the workloads")?;
            let seen = get(c, &coverage, &jwt).await.context("could not read the coverage")?;
            let instances: Vec<String> = seen
                .get("instances")
                .and_then(Value::as_array)
                .map(|v| v.iter().filter_map(|i| i.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let missing: Vec<String> = rows
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter(|w| w.get("ready").and_then(Value::as_i64).unwrap_or(0) > 0)
                .filter_map(|w| w.get("name").and_then(Value::as_str))
                .filter(|name| !instances.iter().any(|i| i.starts_with(&format!("{name}-"))))
                .map(str::to_string)
                .collect();
            if !missing.is_empty() {
                return Err(anyhow!("nothing is scraping {}", missing.join(", ")));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `tel.stream.lag` and `tel.ch.disk`: the two pipeline numbers, from one route.
///
/// An ABSENT number fails its step. Nothing reporting a value is exactly the state these SLOs
/// exist to catch, and reading it as a zero would make both of them pass forever.
async fn pipeline(c: &mut Ctx) {
    for (id, field, judge) in [
        (
            "tel.stream.lag",
            "stream_pending",
            (|v| (v < MAX_STREAM_PENDING).then_some(()).ok_or_else(|| anyhow!("{v} entries pending")))
                as fn(f64) -> Result<()>,
        ),
        ("tel.ch.disk", "ch_disk_free_pct", |v| {
            (v > MIN_DISK_FREE_PCT).then_some(()).ok_or_else(|| anyhow!("{v:.1} % free"))
        }),
    ] {
        c.step(id, PIPELINE_CEILING, move |c| {
            let jwt = c.admin_jwt.clone();
            let url = admin(c, "/admin/slo/pipeline");
            async move {
                let v = get(c, &url, &jwt).await.context("could not read the pipeline")?;
                let n = v
                    .get(field)
                    .and_then(Value::as_f64)
                    .ok_or_else(|| anyhow!("nothing is reporting {field}"))?;
                judge(n)
            }
            .boxed()
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no hosts configured and nothing reachable, every id is still produced exactly once.
    #[tokio::test]
    async fn edge_produces_every_id_once() {
        let mut c = crate::testkit::ctx().await;
        // Time is paused: `tel.log.latency` polls for a minute, and no test should sit through
        // one. Every real call here fails at connect (port 1), so nothing depends on the clock.
        tokio::time::pause();
        // Nothing here may dial a real hostname: the only outbound calls left go to port 1.
        c.programs.bash = "false".into();
        c.programs.dig = "false".into();
        run(&mut c).await;
        let ids: Vec<&str> = c.steps.iter().map(|s| s.slo_id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "edge.dns",
                "edge.cert",
                "edge.origin",
                "edge.ssh.lb",
                "tel.log.latency",
                "tel.pod.coverage",
                "tel.stream.lag",
                "tel.ch.disk",
            ]
        );
    }
}
