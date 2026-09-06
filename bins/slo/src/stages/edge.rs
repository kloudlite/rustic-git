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
    worker_lanes(c).await;
    agent_heartbeat(c).await;
}

/// `worker.lane.health`: every worker lane's heartbeat is fresh.
///
/// The worker's liveness contract is "N fresh `worker-alive.*` files" — a WEDGED lane beside a
/// live sibling keeps the pod alive and its jobs unclaimed, which is exactly the failure the file
/// count exists to catch and the one nothing was watching. The number is read off the worker's own
/// `/metrics` (`worker_lane_heartbeat_age_seconds`, a gauge per lane, port 9464 on every worker
/// pod), because the files themselves live inside a pod this probe has no exec grant on.
///
/// The threshold is the liveness probe's own window (`-mmin -30`), not something tighter: a lane
/// draining a long merge touches its heartbeat per entry, and inventing a stricter ceiling here
/// would page for a fleet Kubernetes itself calls healthy.
async fn worker_lanes(c: &mut Ctx) {
    let aks = match crate::drill::incluster() {
        Ok(k) => k,
        Err(e) => return c.skip("worker.lane.health", &format!("no in-cluster client: {e:#}")),
    };
    c.step("worker.lane.health", READ_CEILING, move |c| {
        async move {
            let ages = lane_ages(c, &aks).await?;
            if ages.is_empty() {
                return Err(anyhow!("no worker lane reports a heartbeat at all"));
            }
            let stale: Vec<String> = ages
                .iter()
                .filter(|(_, age)| *age > LANE_MAX_AGE_SECS)
                .map(|(lane, age)| format!("{lane} at {age:.0} s"))
                .collect();
            if !stale.is_empty() {
                return Err(anyhow!("a lane has stopped beating: {}", stale.join(", ")));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// The worker's liveness window (`find … -mmin -30` in deploy/kloudlite.yaml), in seconds.
const LANE_MAX_AGE_SECS: f64 = 1800.0;

/// `(pod/lane, age)` for every lane of every worker pod, from their own `/metrics`.
async fn lane_ages(c: &Ctx, aks: &kube::Client) -> Result<Vec<(String, f64)>> {
    use k8s_openapi::api::core::v1::Pod;
    let api: kube::Api<Pod> = kube::Api::namespaced(aks.clone(), "kloudlite");
    let pods = api
        .list(&kube::api::ListParams::default().labels("app=kloudlite-worker"))
        .await
        .map_err(|e| anyhow!("could not list the worker pods: {e}"))?;
    let mut out = vec![];
    for pod in &pods.items {
        let name = kube::ResourceExt::name_any(pod);
        let Some(ip) = pod.status.as_ref().and_then(|s| s.pod_ip.clone()) else { continue };
        let body = c
            .http
            .get(format!("http://{ip}:9464/metrics"))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| anyhow!("{name} would not answer /metrics: {}", e.without_url()))?
            .text()
            .await
            .unwrap_or_default();
        out.extend(parse_lane_ages(&body).into_iter().map(|(lane, age)| (format!("{name}/{lane}"), age)));
    }
    Ok(out)
}

/// `worker_lane_heartbeat_age_seconds{lane="0"} 3.2` -> `("0", 3.2)`. A pure function so the
/// judgement is testable without a worker: the exposition format is the contract here.
fn parse_lane_ages(body: &str) -> Vec<(String, f64)> {
    body.lines()
        .filter(|l| l.starts_with("worker_lane_heartbeat_age_seconds{"))
        .filter_map(|l| {
            let lane = l.split("lane=\"").nth(1)?.split('"').next()?.to_string();
            let age = l.rsplit(' ').next()?.trim().parse().ok()?;
            Some((lane, age))
        })
        .collect()
}

/// `agent.heartbeat`: every node's agent is beating, and the DaemonSet is whole.
///
/// The agent is a controller with no queue and no registration: nothing else notices when one
/// wedges, and a node whose agent stopped reconciling looks exactly like a node with nothing to
/// do. Its own liveness file (`{pool}/.agent-heartbeat`, written on a beat) is the one number that
/// says otherwise, read through the same exec grant the workspace steps use.
async fn agent_heartbeat(c: &mut Ctx) {
    let Some(k) = c.kube.clone() else { return c.skip("agent.heartbeat", "no kubeconfig") };
    c.step("agent.heartbeat", Duration::from_secs(30), move |_| {
        async move {
            use k8s_openapi::api::apps::v1::DaemonSet;
            use k8s_openapi::api::core::v1::Pod;
            let ds: kube::Api<DaemonSet> = kube::Api::namespaced(k.clone(), "kube-system");
            let agent = ds.get("kloudlite-agent").await.map_err(|e| anyhow!("no agent DaemonSet: {e}"))?;
            let st = agent.status.ok_or_else(|| anyhow!("the agent DaemonSet reports no status"))?;
            if st.number_ready < st.desired_number_scheduled {
                return Err(anyhow!(
                    "the agent DaemonSet is {}/{} ready",
                    st.number_ready,
                    st.desired_number_scheduled
                ));
            }
            let pods: kube::Api<Pod> = kube::Api::namespaced(k.clone(), "kube-system");
            let list = pods
                .list(&kube::api::ListParams::default().labels("app=kloudlite-agent"))
                .await
                .map_err(|e| anyhow!("could not list the agent pods: {e}"))?;
            if list.items.is_empty() {
                return Err(anyhow!("the agent DaemonSet has no pods"));
            }
            for pod in &list.items {
                let name = kube::ResourceExt::name_any(pod);
                // `WS_POOL` comes from the container's own env, which an exec inherits — a path
                // repeated here would go stale the day a region moved its pool.
                let script = "stat -c %Y \"${WS_POOL:-/mnt/wspool}/.agent-heartbeat\"; date +%s";
                let (code, out, err) =
                    // NAMED: the agent pod carries more than one container, and an exec that names
                    // none is answered `400 Bad Request` at the WebSocket upgrade — which reads as
                    // a broken agent rather than a probe that did not say where to run.
                    crate::kube::exec(&k, "kube-system", &name, Some(AGENT_CONTAINER), &["sh", "-c", script], Duration::from_secs(15)).await?;
                if code != 0 {
                    return Err(anyhow!("{name} has no heartbeat file: exit {code}: {}", err.trim()));
                }
                let nums: Vec<i64> = out.split_whitespace().filter_map(|n| n.parse().ok()).collect();
                let [wrote, now] = nums[..] else {
                    return Err(anyhow!("{name} answered {out:?}, which is not a timestamp pair"));
                };
                if now - wrote > AGENT_MAX_AGE_SECS {
                    return Err(anyhow!("{name}'s heartbeat is {} s old", now - wrote));
                }
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// The DaemonSet's container name (`deploy/k3s/agent-daemonset.yaml`).
const AGENT_CONTAINER: &str = "agent";

/// Five minutes: the agent beats far more often than that, and the DaemonSet's own probe is what
/// restarts a pod that stopped — this is the window in which that restart should already have
/// happened.
const AGENT_MAX_AGE_SECS: i64 = 300;

/// `edge.dns`: every public hostname resolves. One step for all of them — a fleet with one
/// unresolvable hostname is one broken front door, not a fraction of one.
async fn dns(c: &mut Ctx) {
    let mut hosts = c.cfg.hosts.clone();
    // The ssh host has no HTTPS, so it is not in KLOUDLITE_SLO_HOSTS; it still has to resolve.
    if !hosts.contains(&c.cfg.ssh_host) {
        hosts.push(c.cfg.ssh_host.clone());
    }
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
                // Five seconds per host, not the whole ceiling: a host whose DNS points at a
                // dead address makes `s_client` hang, and one such host must not eat the rest.
                let out = tls::enddate(&bash, &openssl, h, Duration::from_secs(5))
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
    let Some(host) = c.cfg.hosts.first().cloned() else {
        return c.skip("edge.origin", "no hostname to pin");
    };
    // Back to a documented skip when the env is empty, and this time the reason is measured
    // rather than assumed: reading the address off the Ingress worked, and the dial timed out
    // after 15 s. A pod cannot reach its own cluster's public load-balancer address on Azure —
    // the LB does not hairpin — so no address the CLUSTER publishes is dialable from inside it.
    // The env stays as the override for a probe run from somewhere that can reach it.
    let Some(ip) = c.cfg.origin_ip.clone() else {
        return c.skip(
            "edge.origin",
            "no KLOUDLITE_SLO_ORIGIN_IP: the ingress address the cluster publishes does not hairpin back into the pod network, so a probe inside the cluster cannot dial the origin",
        );
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
            // `/admin/workloads` is the CENTRAL list, so coverage judged over it alone was really
            // "every central workload is scraped" — the region's own two DaemonSets, the agent and
            // the collector that scrapes everything else, were the two nothing checked.
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
            let region: Vec<String> = REGION_SCRAPED
                .iter()
                .filter(|name| !instances.iter().any(|i| i.starts_with(&format!("{name}-"))))
                .map(|n| (*n).to_string())
                .collect();
            let missing: Vec<String> = missing.into_iter().chain(region).collect();
            if !missing.is_empty() {
                return Err(anyhow!("nothing is scraping {}", missing.join(", ")));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// The region's own scrape targets, which `/admin/workloads` does not list: the agent DaemonSet
/// and the collector DaemonSet that scrapes every annotated pod on the node.
const REGION_SCRAPED: [&str; 2] = ["kloudlite-agent", "kloudlite-otel-agent"];

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
    /// The exposition line the whole `worker.lane.health` judgement rests on. A worker that stopped
    /// exporting the gauge must read as "no lane reports", never as "every lane is fresh".
    #[test]
    fn lane_ages_are_read_off_the_exposition_or_not_at_all() {
        let body = "# HELP worker_lane_heartbeat_age_seconds age\n\
                    worker_lane_heartbeat_age_seconds{lane=\"0\"} 3.5\n\
                    worker_lane_heartbeat_age_seconds{lane=\"1\"} 2400\n\
                    worker_jobs_total 7\n";
        let ages = super::parse_lane_ages(body);
        assert_eq!(ages, vec![("0".to_string(), 3.5), ("1".to_string(), 2400.0)]);
        assert!(super::parse_lane_ages("worker_jobs_total 7\n").is_empty());
    }

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
                "worker.lane.health",
                "agent.heartbeat",
            ]
        );
    }
}
