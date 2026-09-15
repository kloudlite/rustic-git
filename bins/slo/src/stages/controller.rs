//! 15 · Cluster controller. The controller is invisible from outside — a person never calls it —
//! so every id here is judged the way a person would notice it failing: a workspace that cannot
//! reach its environment, or one that still can after the grant was taken away.
//!
//! Two exceptions, both deliberate: `ctl.leader` reads the Lease, because the lease IS the output
//! being asserted, and `ctl.agent.nowrite` reads `managedFields`, because "which process wrote
//! this" has no other observable — and it is the one assertion that catches a mixed build
//! silently re-acquiring two writers.
//!
//! Runs right after stage 6, whose workspace and environment it grants between; stage 6 ends with
//! the space cleared, so every choice here starts from nothing. Dials are busybox `nc` speaking
//! redis's inline `PING`, from the WORKSPACE pod: `redis-cli` is in the environment's image, not
//! the workspace's, and the workspace is the side a grant is for. Every refusal is judged beside a
//! positive control from the target environment's own service pod, so a redis that is down never
//! reads as a policy taking effect.
//!
//! Every pod dialled here is a k3s region pod, and the controller and its policies are k3s-only,
//! so what enforces them is the k3s CNI — AKS's missing network-policy engine is not in this path.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;
use k8s_openapi::api::coordination::v1::Lease;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::jiff::Timestamp;
use kube::api::{Api, DeleteParams, ListParams};
use kloudlite_workspaces::crd;
use kloudlite_workspaces::slo::catalogue::Suite;

use super::environment::{create_running, my_space, EXEC_CEILING, PORT, SERVICE};
use super::workspace::ws_exec;
use super::{call, clip};
use crate::ctx::Ctx;

pub const CONTROLLER: &str = "15 · Cluster controller";

/// `bins/controller/src/lease.rs`'s `LEASE_NAME`/`LEASE_NAMESPACE`, and `deploy/k3s/controller.yaml`'s
/// pod label.
const LEASE: &str = "kloudlite-controller";
const LEASE_NS: &str = "kube-system";
const POD_SELECTOR: &str = "app=kloudlite-controller";

const LEADER_CEILING: Duration = Duration::from_secs(10);
/// The catalogue's 30 s bound for all three grant ids.
const GRANT_CEILING: Duration = Duration::from_secs(30);
const FAILOVER_CEILING: Duration = Duration::from_secs(120);
const ELECT_WITHIN: Duration = Duration::from_secs(20);
const NOWRITE_CEILING: Duration = Duration::from_secs(30);

const GRANT_IDS: [&str; 3] = ["ctl.grant.set", "ctl.grant.switch", "ctl.grant.cleared"];
/// Every hourly id, so a skip path names each one: an id a run never reports reads as passed.
const HOURLY_IDS: [&str; 3] = ["ctl.failover", "ctl.agent.nowrite", "ctl.fanout"];
/// The probe holds no ClickHouse credential (`edge.rs` asks the admin process for the same
/// reason) and no admin route serves the controller's `grant.rendered` lines, so the count has no
/// reader. A skip that says so, never a pass without a measurement.
const FANOUT_UNMEASURABLE: &str = "fan-out is not measurable from the probe: no history reader configured";

pub async fn run(c: &mut Ctx) {
    let hourly = c.suite == Suite::Hourly;
    if c.kube.is_none() {
        c.skip("ctl.leader", "no kubeconfig");
        skip_all(c, &GRANT_IDS, "no kubeconfig");
        if hourly {
            skip_all(c, &HOURLY_IDS, "no kubeconfig");
        }
        return;
    }
    leader(c).await;
    let granted = grants(c).await;
    // Whatever path `grants` left by, later stages see the space as a fast run leaves it: clear.
    clear(c).await;
    if hourly {
        match granted {
            Some((ws, env)) => failover(c, &ws, &env).await,
            None => c.skip("ctl.failover", "ctl.grant.set did not pass, so a converged choice has nothing to be compared with"),
        }
        // Before the clear: the grant `ctl.failover` made is what gives this id policies to read.
        agent_nowrite(c).await;
        clear(c).await;
        c.skip("ctl.fanout", FANOUT_UNMEASURABLE);
    }
}

/// Best-effort: an error here is teardown's to retry, never a sample.
async fn clear(c: &Ctx) {
    let url = my_space(c);
    if let Err(e) = call(c, reqwest::Method::DELETE, &url, &c.probe_jwt, None).await {
        tracing::warn!(kind = "space", op = "clear", error = %format!("{e:#}"), "slo.controller.clear.failed");
    }
}

fn skip_all(c: &mut Ctx, ids: &[&'static str], why: &str) {
    for id in ids {
        c.skip(id, why);
    }
}

/// `ctl.leader`: the Lease names a holder that renewed within its own TTL, and that holder is the
/// one controller pod Running.
async fn leader(c: &mut Ctx) {
    c.step("ctl.leader", LEADER_CEILING, move |c| {
        async move {
            let k = c.kube.clone().ok_or_else(|| anyhow!("no kubeconfig"))?;
            let lease = Api::<Lease>::namespaced(k.clone(), LEASE_NS).get(LEASE).await.context("could not read the controller lease")?;
            let holder = holder(&lease, Timestamp::now())?;
            let running = running_controllers(&k).await?;
            if running.len() != 1 {
                return Err(anyhow!("{} controller pods are Running, wanted exactly one: {running:?}", running.len()));
            }
            if running[0] != holder {
                return Err(anyhow!("the lease names {holder}, but the Running controller pod is {}: a stale lease, not a leader", running[0]));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// The holder of a lease that is still in term at `now`.
fn holder(lease: &Lease, now: Timestamp) -> Result<String> {
    let spec = lease.spec.as_ref().ok_or_else(|| anyhow!("the lease has no spec"))?;
    let who = spec.holder_identity.clone().filter(|h| !h.is_empty()).ok_or_else(|| anyhow!("the lease names no holder"))?;
    let renewed = spec.renew_time.as_ref().ok_or_else(|| anyhow!("the lease held by {who} was never renewed"))?.0;
    let ttl = i64::from(spec.lease_duration_seconds.ok_or_else(|| anyhow!("the lease carries no duration"))?);
    let age = now.duration_since(renewed).as_secs();
    if age > ttl {
        return Err(anyhow!("{who} last renewed {age} s ago, past its {ttl} s term"));
    }
    Ok(who)
}

async fn running_controllers(k: &kube::Client) -> Result<Vec<String>> {
    let pods = Api::<Pod>::namespaced(k.clone(), LEASE_NS)
        .list(&ListParams::default().labels(POD_SELECTOR))
        .await
        .context("could not list the controller pods")?;
    Ok(pods
        .items
        .into_iter()
        .filter(|p| p.metadata.deletion_timestamp.is_none())
        .filter(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running"))
        .filter_map(|p| p.metadata.name)
        .collect())
}

/// `ctl.grant.set`, `ctl.grant.switch`, `ctl.grant.cleared`. Answers the workspace and the
/// environment `ctl.grant.set` reached, which `ctl.failover` chooses again.
async fn grants(c: &mut Ctx) -> Option<(String, String)> {
    let (Some(ws), Some(env)) = (c.state.workspace.clone(), c.state.environment.clone()) else {
        skip_all(c, &GRANT_IDS, "stages 5 and 6 left no running workspace and environment to grant between");
        return None;
    };
    let old_ip = Arc::new(Mutex::new(String::new()));
    let (w, e, ip) = (ws.clone(), env.clone(), old_ip.clone());
    let set = c
        .step("ctl.grant.set", GRANT_CEILING, move |c| {
            async move {
                choose(c, &e).await?;
                let script = format!("{}; echo ==; {}", resolve(), ping(SERVICE));
                let out = until(c, &w, &script, GRANT_CEILING, |o| {
                    let (dns, dial) = o.split_once("==").unwrap_or((o, ""));
                    ip_of(dns).is_some() && pong(dial)
                }, &format!("`{SERVICE}` never resolved and answered PONG in the workspace"))
                .await?;
                *ip.lock().unwrap() = ip_of(out.split("==").next().unwrap_or("")).unwrap_or_default();
                Ok(())
            }
            .boxed()
        })
        .await;
    if !set {
        c.skip("ctl.grant.switch", "ctl.grant.set did not pass, so there is no choice to switch from");
        c.skip("ctl.grant.cleared", "ctl.grant.set did not pass, so there is no choice to clear");
        return None;
    }
    let old_ip = old_ip.lock().unwrap().clone();

    // Stood up OUTSIDE the step: a create is `env.create.p95`'s measurement, and inside a 30 s
    // bound it would be the whole sample. `run-` prefixed, so teardown's sweep takes it; its Volume
    // is named by its id, which is what `extra_volumes` is for (the same path `env.clone.p95` uses).
    let name = format!("{}-ctlenv", c.prefix());
    let second = match create_running(c, &name).await {
        Ok(id) => {
            c.state.extra_volumes.push(id.clone());
            c.save_state();
            id
        }
        Err(e) => {
            c.skip("ctl.grant.switch", &format!("the second environment never became ready: {}", clip(&format!("{e:#}"))));
            c.skip("ctl.grant.cleared", "ctl.grant.switch did not run, so which environment is chosen is not known");
            return Some((ws, env));
        }
    };

    let new_ip = Arc::new(Mutex::new(String::new()));
    let (w, s, e, ip, old) = (ws.clone(), second.clone(), env.clone(), new_ip.clone(), old_ip.clone());
    let switched = c
        .step("ctl.grant.switch", GRANT_CEILING, move |c| {
            async move {
                choose(c, &s).await?;
                // The old side by ClusterIP, never by name: the name stops resolving once
                // resolv.conf is re-rendered, and that DNS failure would pass this id with the old
                // policy still standing.
                let script = format!("{}; echo ==; {}; echo ==; {}", resolve(), ping(SERVICE), ping(&old));
                let out = until_refused(c, &w, &script, &e, GRANT_CEILING, |o, in_env| {
                    let parts: Vec<&str> = o.split("==").collect();
                    parts.len() == 3 && ip_of(parts[0]).is_some_and(|i| i != old) && pong(parts[1]) && refusal_proven(in_env, parts[2])
                }, &format!("the new `{SERVICE}` never answered while the old ClusterIP {old} stopped answering"))
                .await?;
                *ip.lock().unwrap() = ip_of(out.split("==").next().unwrap_or("")).unwrap_or_default();
                Ok(())
            }
            .boxed()
        })
        .await;
    if !switched {
        c.skip("ctl.grant.cleared", "ctl.grant.switch did not pass, so which environment is chosen is not known");
        return Some((ws, env));
    }
    let target = new_ip.lock().unwrap().clone();

    let (w, s) = (ws.clone(), second.clone());
    let namespaces = [crd::env_namespace(&env), crd::env_namespace(&second)];
    let probe = c.probe_user.clone();
    c.step("ctl.grant.cleared", GRANT_CEILING, move |c| {
        async move {
            let url = my_space(c);
            call(c, reqwest::Method::DELETE, &url, &c.probe_jwt, None).await.context("could not clear the space's choice")?;
            let start = Instant::now();
            until_refused(c, &w, &ping(&target), &s, GRANT_CEILING, |o, in_env| refusal_proven(in_env, o), &format!("{target}:{PORT} still answers PONG after the clear")).await?;
            // The SECOND assertion: the refusal above is the output, and this says the refusal is
            // the controller's removal rather than a pod that happened to be restarting.
            let k = c.kube.clone().ok_or_else(|| anyhow!("no kubeconfig"))?;
            let policy = kloudlite_workspaces::k8s::space_ingress_name(&crd::ws_namespace(&probe, ""));
            loop {
                let mut left = vec![];
                for ns in &namespaces {
                    if Api::<NetworkPolicy>::namespaced(k.clone(), ns).get_opt(&policy).await?.is_some() {
                        left.push(format!("{ns}/{policy}"));
                    }
                }
                if left.is_empty() {
                    return Ok(());
                }
                if start.elapsed() + Duration::from_secs(2) >= GRANT_CEILING {
                    return Err(anyhow!("the connect was refused but {left:?} still stands"));
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
        .boxed()
    })
    .await;
    Some((ws, env))
}

/// `ctl.failover`, hourly: delete the leader pod, choose during the gap, and assert a DIFFERENT
/// holder inside 20 s and the choice converged once it is up.
///
/// The probe holds `pods: delete` in kube-system fenced by admission to the controller's own pods
/// (`deploy/k3s/slo-rbac.yaml`); a region that has not applied that file answers 403 and this id
/// SKIPS with that status rather than failing.
async fn failover(c: &mut Ctx, ws: &str, env: &str) {
    const ID: &str = "ctl.failover";
    let Some(k) = c.kube.clone() else { return c.skip(ID, "no kubeconfig") };
    let old = match Api::<Lease>::namespaced(k.clone(), LEASE_NS).get(LEASE).await {
        Ok(l) => match holder(&l, Timestamp::now()) {
            Ok(h) => h,
            Err(e) => return c.skip(ID, &format!("no leader to fail over from: {e:#}")),
        },
        Err(e) => return c.skip(ID, &format!("could not read the controller lease: {e}")),
    };
    match Api::<Pod>::namespaced(k.clone(), LEASE_NS).delete(&old, &DeleteParams::default()).await {
        Ok(_) => {}
        Err(kube::Error::Api(e)) if e.code == 403 => {
            return c.skip(ID, &format!("slo-rbac.yaml not applied on this region (the API answered {} {})", e.code, e.reason));
        }
        Err(e) => return c.skip(ID, &format!("could not delete the leader pod {old}: {e}")),
    }
    let (ws, env) = (ws.to_string(), env.to_string());
    c.step(ID, FAILOVER_CEILING, move |c| {
        async move {
            let start = Instant::now();
            // Immediately, into the gap: a wish lost in the handover is the failure this catches.
            choose(c, &env).await?;
            let leases = Api::<Lease>::namespaced(k, LEASE_NS);
            loop {
                let now = leases.get(LEASE).await.ok().and_then(|l| holder(&l, Timestamp::now()).ok());
                match now {
                    Some(h) if h != old => break,
                    _ if start.elapsed() >= ELECT_WITHIN => {
                        return Err(anyhow!("no holder other than {old} within {} s (last read: {now:?})", ELECT_WITHIN.as_secs()));
                    }
                    _ => tokio::time::sleep(Duration::from_secs(1)).await,
                }
            }
            let left = FAILOVER_CEILING.saturating_sub(start.elapsed());
            until(c, &ws, &ping(SERVICE), left, pong, "the choice made during the gap never converged").await?;
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `ctl.agent.nowrite`, hourly: every `space-*` NetworkPolicy is `kloudlite-controller`'s and no
/// agent's, read from `managedFields`.
async fn agent_nowrite(c: &mut Ctx) {
    const ID: &str = "ctl.agent.nowrite";
    let Some(k) = c.kube.clone() else { return c.skip(ID, "no kubeconfig") };
    // An empty list is not evidence; a list that fails is the step's to fail, below.
    let listed = space_policies(&k).await;
    if listed.as_ref().is_ok_and(|p| p.is_empty()) {
        return c.skip(ID, "no space grants in the cluster");
    }
    c.step(ID, NOWRITE_CEILING, move |_| {
        async move {
            let offenders: Vec<String> = listed?
                .into_iter()
                .filter_map(|p| {
                    let managers: Vec<String> =
                        p.metadata.managed_fields.unwrap_or_default().into_iter().filter_map(|m| m.manager).collect();
                    let bad = !managers.iter().any(|m| m == crd::CONTROLLER_FIELD_MANAGER)
                        || managers.iter().any(|m| m == crd::AGENT_FIELD_MANAGER);
                    bad.then(|| format!("{}/{} {managers:?}", p.metadata.namespace.unwrap_or_default(), p.metadata.name.unwrap_or_default()))
                })
                .collect();
            if offenders.is_empty() {
                Ok(())
            } else {
                Err(anyhow!("space policies not solely the controller's: {offenders:?}"))
            }
        }
        .boxed()
    })
    .await;
}

async fn space_policies(k: &kube::Client) -> Result<Vec<NetworkPolicy>> {
    let all = Api::<NetworkPolicy>::all(k.clone()).list(&ListParams::default()).await.context("could not list NetworkPolicies")?;
    Ok(all.items.into_iter().filter(|p| p.metadata.name.as_deref().is_some_and(|n| n.starts_with("space-"))).collect())
}

async fn choose(c: &Ctx, env: &str) -> Result<()> {
    let url = my_space(c);
    call(c, reqwest::Method::PUT, &url, &c.probe_jwt, Some(serde_json::json!({ "environment": env })))
        .await
        .map(|_| ())
        .with_context(|| format!("could not choose {env} for the space"))
}

fn resolve() -> String {
    format!("getent hosts {SERVICE} || nslookup {SERVICE}")
}

/// redis's inline `PING` over busybox `nc`, the one client every image here carries.
fn ping(host: &str) -> String {
    format!(r"printf 'PING\r\n' | nc -w 2 {host} {PORT}")
}

fn pong(out: &str) -> bool {
    out.contains("PONG")
}

/// The service's address from `getent` (one line) or `nslookup` (the resolver's own address first,
/// as `ip:53`, which does not parse): the last bare IP either prints.
fn ip_of(out: &str) -> Option<String> {
    out.split_whitespace().rev().find(|t| t.parse::<std::net::IpAddr>().is_ok()).map(str::to_string)
}

/// A refusal counts only beside its positive control: the target service answered PONG from
/// inside its own namespace in the same poll. A redis that is down refuses the workspace too.
fn refusal_proven(in_env: &str, from_ws: &str) -> bool {
    pong(in_env) && !pong(from_ws)
}

/// Poll `script` in the workspace AND `redis-cli ping` in `env`'s own service pod (the `env.dns`
/// shape) until `ok(ws_stdout, env_stdout)`. A failed exec on either side reads as an empty answer,
/// which no `ok` here accepts as a refusal.
async fn until_refused(c: &Ctx, ws: &str, script: &str, env: &str, cap: Duration, ok: impl Fn(&str, &str) -> bool, what: &str) -> Result<String> {
    let k = c.kube.clone().ok_or_else(|| anyhow!("no kubeconfig"))?;
    let ns = crd::env_namespace(env);
    let pod = format!("{SERVICE}-0");
    let control = format!("redis-cli -h {SERVICE} -p {PORT} ping");
    let start = Instant::now();
    let mut last = String::new();
    loop {
        let in_env = crate::kube::exec(&k, &ns, &pod, None, &["sh", "-c", &control], EXEC_CEILING).await.map(|(_, o, _)| o).unwrap_or_default();
        if let Ok((_, out, _)) = ws_exec(c, ws, script, EXEC_CEILING).await {
            if ok(&out, &in_env) {
                return Ok(out);
            }
            last = out;
        }
        if start.elapsed() + Duration::from_secs(2) >= cap {
            if !pong(&in_env) {
                return Err(anyhow!("target environment's service did not answer from inside its own namespace ({ns}/{pod}); last answer {:?}", clip(in_env.trim())));
            }
            return Err(anyhow!("{what} within {} ms; last output {:?}", cap.as_millis(), clip(last.trim())));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Poll `script` in the workspace until `ok(stdout)`. A failed exec is never read as either
/// answer: a pod mid-restart refuses every connect, and that must not pass a refusal.
async fn until(c: &Ctx, ws: &str, script: &str, cap: Duration, ok: impl Fn(&str) -> bool, what: &str) -> Result<String> {
    let start = Instant::now();
    let mut last = String::new();
    loop {
        if let Ok((_, out, _)) = ws_exec(c, ws, script, EXEC_CEILING).await {
            if ok(&out) {
                return Ok(out);
            }
            last = out;
        }
        if start.elapsed() + Duration::from_secs(2) >= cap {
            return Err(anyhow!("{what} within {} ms; last output {:?}", cap.as_millis(), clip(last.trim())));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;
    use k8s_openapi::api::coordination::v1::LeaseSpec;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;

    fn lease(holder: &str, renewed_secs_ago: i64) -> (Lease, Timestamp) {
        let now = Timestamp::now();
        let renew = now.checked_sub(k8s_openapi::jiff::SignedDuration::from_secs(renewed_secs_ago)).unwrap();
        let spec = LeaseSpec {
            holder_identity: Some(holder.into()),
            lease_duration_seconds: Some(15),
            renew_time: Some(MicroTime(renew)),
            ..Default::default()
        };
        (Lease { spec: Some(spec), ..Default::default() }, now)
    }

    #[test]
    fn a_lease_is_held_only_inside_its_term_and_by_a_named_holder() {
        let (l, now) = lease("ctl-a", 3);
        assert_eq!(holder(&l, now).unwrap(), "ctl-a");
        let (l, now) = lease("ctl-a", 16);
        assert!(holder(&l, now).is_err(), "a lease past its TTL read as held");
        let (l, now) = lease("", 1);
        assert!(holder(&l, now).is_err(), "an empty holder read as a leader");
    }

    #[test]
    fn a_refusal_is_proven_only_beside_the_in_env_pong() {
        assert!(refusal_proven("PONG\n", ""), "env PONG + no ws PONG is a proven refusal");
        assert!(!refusal_proven("", ""), "a down target read as a refusal");
        assert!(!refusal_proven("PONG", "+PONG\r\n"));
    }

    #[test]
    fn the_address_is_the_service_not_the_resolver() {
        assert_eq!(ip_of("10.43.7.9      redis.env-x.svc.cluster.local  redis\n").as_deref(), Some("10.43.7.9"));
        let nslookup = "Server:\t\t10.43.0.10\nAddress:\t10.43.0.10:53\n\nName:\tredis.env-x.svc.cluster.local\nAddress: 10.43.7.9\n";
        assert_eq!(ip_of(nslookup).as_deref(), Some("10.43.7.9"));
        assert_eq!(ip_of("nslookup: can't resolve 'redis'"), None);
    }

    /// Every id the catalogue gives this stage is reported exactly once on the skip path, and an
    /// hourly id never on a fast run.
    #[tokio::test]
    async fn every_id_is_reported_once_and_hourly_ids_only_hourly() {
        for suite in [Suite::Fast, Suite::Hourly] {
            let mut c = testkit::ctx().await;
            c.kube = None;
            c.suite = suite;
            run(&mut c).await;
            let want: Vec<&str> = kloudlite_workspaces::slo::catalogue::journey(suite)
                .into_iter()
                .filter(|(n, _)| *n == CONTROLLER)
                .flat_map(|(_, ids)| ids)
                .collect();
            assert!(!want.is_empty());
            let got: Vec<&str> = c.steps.iter().map(|s| s.slo_id.as_str()).collect();
            let mut sorted = want.clone();
            sorted.sort_unstable();
            let mut got_sorted = got.clone();
            got_sorted.sort_unstable();
            assert_eq!(got_sorted, sorted, "{suite:?}");
            assert!(c.steps.iter().all(|s| s.skipped && !s.detail.is_empty()));
        }
    }
}
