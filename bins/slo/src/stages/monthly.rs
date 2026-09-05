//! Stage 13 · Monthly: the backups nobody watches, and the three drills that break the fleet on
//! purpose.
//!
//! Every row in `deploy/BACKUPS.md` is a RETENTION SETTING, which is the kind of thing that fails
//! silently by definition — a switch somebody turned off during an incident stays off until the
//! restore that needed it. The four `bak.*` ids are that page, read rather than claimed.
//!
//! The three drills are the other half: the fleet's resilience is only true if somebody keeps
//! proving it, and the one absolute rule is that a drill undoes itself on every path out of it
//! (`crate::drill`) — a monthly probe that left a node tainted is an outage nobody would think to
//! look for.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use futures::{FutureExt, TryStreamExt};
use serde_json::{json, Value};

use super::{admin, api, get, poll_json, post};
use crate::ctx::{Ctx, PROBE_USER};
use crate::{drill, tools};

/// The container and the fixed slot names `deploy/k3s/backup-controlplane.sh` writes: 24 hourly
/// slots covering a day and 7 daily ones covering a week, all overwritten in place. Repeated here
/// rather than derived — the shell script is the contract, and a probe that inferred the names
/// would go green the day somebody renamed them.
const BACKUP_CONTAINER: &str = "k3s-backup";
const SLOT_SUFFIX: &str = ".tgz.enc";
/// The timer runs hourly, so anything past two hours has MISSED one — the same threshold
/// `deploy/BACKUPS.md`'s verification step names. In MINUTES because `num_hours` truncates: a
/// backup 119 minutes old and one 60 minutes old are both "1 hour", and the comparison would let
/// the age drift most of an hour past the threshold before anyone heard about it.
const MAX_TARBALL_AGE_MINS: i64 = 120;

const READ_CEILING: Duration = Duration::from_secs(60);
/// The dead-node drill waits out `nodeDeadSecs` and then a start elsewhere; the drain waits out the
/// agent's own beat, which the console gives ten minutes.
const DRAIN_CAP: Duration = Duration::from_secs(600);
/// Long enough that a fleet leaning on Redis for anything load-bearing would show it, short enough
/// that the CronJob's two hours still fit the dead-node drill after it.
const REDIS_DOWN: Duration = Duration::from_secs(300);
/// `WS_NODE_DEAD_SECS`'s compiled default, used when the region does not publish one.
const NODE_DEAD_FALLBACK: u64 = 180;
/// The grace on top of it: the sweep is a beat, not an instant, and a drill that started asking one
/// second after the deadline would report a flake as a failure.
const DEAD_SLACK: u64 = 60;

/// A step's ceiling for a body that has an undo: always the body's own plus a minute. `Ctx::step`
/// times out by DROPPING the step's future, so an outer timeout that fired first would take the
/// undo with it — the drill's own cap has to be the one that wins.
fn step_cap(body: Duration) -> Duration {
    body + Duration::from_secs(60)
}

pub async fn run(c: &mut Ctx) {
    backups(c).await;
    dead_node(c).await;
    drain(c).await;
    redis_down(c).await;
}

// ── backups ─────────────────────────────────────────────────────────────

async fn backups(c: &mut Ctx) {
    tarball_age(c).await;
    daily_slots(c).await;
    versioning(c).await;
    cosmos(c).await;
}

/// Every blob in the backup container, newest-modified first. The storage credential is
/// `AZURE_STORAGE_ACCOUNT_NAME`/`_KEY`, which `object_store` reads from the environment itself —
/// the same Secret every other tier mounts, given to the MONTHLY CronJob only.
async fn slots() -> Result<Vec<(String, chrono::DateTime<Utc>)>> {
    let store = object_store::azure::MicrosoftAzureBuilder::from_env()
        .with_container_name(BACKUP_CONTAINER)
        .build()
        .context("could not reach the backup container")?;
    let objects: Vec<object_store::ObjectMeta> =
        object_store::ObjectStore::list(&store, None).try_collect().await.context("could not list it")?;
    Ok(objects.into_iter().map(|o| (o.location.to_string(), o.last_modified)).collect())
}

/// `bak.tarball.age`: the newest hourly slot is under two hours old.
///
/// The hourly slots, not any blob: the `.hmac` companions and the daily slots are written by the
/// same run, so a `daily-Mon` from Monday would keep this green all week if the age were taken over
/// everything in the container.
async fn tarball_age(c: &mut Ctx) {
    if c.cfg.azure.is_none() {
        return c.skip("bak.tarball.age", "no Azure credential configured");
    }
    c.step("bak.tarball.age", READ_CEILING, |_| {
        async move {
            let newest = slots()
                .await?
                .into_iter()
                .filter(|(n, _)| n.starts_with("hourly-") && n.ends_with(SLOT_SUFFIX))
                .map(|(_, at)| at)
                .max()
                .ok_or_else(|| anyhow!("the backup container holds no hourly tarball at all"))?;
            let mins = (Utc::now() - newest).num_minutes();
            if mins >= MAX_TARBALL_AGE_MINS {
                return Err(anyhow!("the newest backup is {mins} minutes old"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `bak.daily.slots`: all seven daily slots exist.
///
/// Existence, not age: the slots are FIXED names that overwrite, so a missing one means a whole
/// weekday's run has never succeeded — which is exactly the failure a single "the newest backup is
/// recent" check cannot see.
async fn daily_slots(c: &mut Ctx) {
    if c.cfg.azure.is_none() {
        return c.skip("bak.daily.slots", "no Azure credential configured");
    }
    c.step("bak.daily.slots", READ_CEILING, |_| {
        async move {
            let have: Vec<String> = slots().await?.into_iter().map(|(n, _)| n).collect();
            let missing: Vec<String> = DAYS
                .iter()
                .map(|d| format!("daily-{d}{SLOT_SUFFIX}"))
                .filter(|want| !have.contains(want))
                .collect();
            if !missing.is_empty() {
                return Err(anyhow!("no backup in slot {}", missing.join(", ")));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `date +%a`'s output, which is what the backup script names the daily slots with.
const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// `bak.versioning`: blob versioning is on for the account the whole product's data lives in.
///
/// This is the switch that turns the 24+7 overwriting slots into a history longer than a week, and
/// the only thing that saves a good backup a bad one overwrote. It is also the one that is off by
/// default and stays off silently.
async fn versioning(c: &mut Ctx) {
    let Some(az) = c.cfg.azure.clone() else {
        return c.skip("bak.versioning", "no Azure subscription configured");
    };
    let path = format!(
        "/subscriptions/{}/resourceGroups/{}/providers/Microsoft.Storage/storageAccounts/{}/blobServices/default?api-version=2023-01-01",
        az.subscription, az.resource_group, az.storage_account
    );
    c.step("bak.versioning", READ_CEILING, move |c| {
        async move {
            let doc = arm(c, &path).await?;
            match doc.pointer("/properties/isVersioningEnabled").and_then(Value::as_bool) {
                Some(true) => Ok(()),
                // Absent and `false` are the same answer to the only question here.
                _ => Err(anyhow!("blob versioning is OFF on {}", az.storage_account)),
            }
        }
        .boxed()
    })
    .await;
}

/// `bak.cosmos`: the directory and PR store has a backup policy at all.
///
/// The TYPE is what is read, not a job outcome: Cosmos runs the backup itself, and the only thing
/// that can silently be wrong is an account whose policy nobody ever set — the default periodic
/// tier keeps eight hours, which `deploy/BACKUPS.md` asks be migrated to Continuous.
async fn cosmos(c: &mut Ctx) {
    let Some(az) = c.cfg.azure.clone() else {
        return c.skip("bak.cosmos", "no Azure subscription configured");
    };
    let path = format!(
        "/subscriptions/{}/resourceGroups/{}/providers/Microsoft.DocumentDB/databaseAccounts/{}?api-version=2024-05-15",
        az.subscription, az.resource_group, az.cosmos_account
    );
    c.step("bak.cosmos", READ_CEILING, move |c| {
        async move {
            let doc = arm(c, &path).await?;
            match doc.pointer("/properties/backupPolicy/type").and_then(Value::as_str) {
                Some(t) if !t.is_empty() => {
                    tracing::info!(kind = "cosmos", policy = t, "slo.backup.read");
                    Ok(())
                }
                _ => Err(anyhow!("{} has no backup policy", az.cosmos_account)),
            }
        }
        .boxed()
    })
    .await;
}

/// One ARM GET, with a token from the Azure Monitor service principal the collector already holds.
///
/// A client-credentials grant rather than anything cached: the probe runs once a month, and a token
/// cache for a process that makes two requests in its life is code that can only rot.
async fn arm(c: &Ctx, path: &str) -> Result<Value> {
    let var = |k: &str| std::env::var(k).with_context(|| format!("{k} is not set"));
    let (tenant, client, secret) =
        (var("AZURE_TENANT_ID")?, var("AZURE_CLIENT_ID")?, var("AZURE_CLIENT_SECRET")?);
    let token_url = format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token");
    let r = c
        .http
        .post(&token_url)
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form(&[
            ("grant_type", "client_credentials"),
            ("client_id", &client),
            ("client_secret", &secret),
            ("scope", "https://management.azure.com/.default"),
        ]))
        .send()
        .await
        // `without_url` and no body: a token endpoint's error carries the request back, and the
        // request is a client secret.
        .map_err(|e| anyhow!("could not reach Entra: {}", e.without_url()))?;
    if !r.status().is_success() {
        return Err(anyhow!("Entra answered {} to the token request", r.status()));
    }
    let token = r
        .json::<Value>()
        .await
        .ok()
        .and_then(|v| v.get("access_token").and_then(Value::as_str).map(str::to_string))
        .ok_or_else(|| anyhow!("Entra answered no access token"))?;
    super::get(c, &format!("https://management.azure.com{path}"), &token).await
}

/// `a=b&c=d`, percent-encoded. A client secret is a random string that may hold anything, and a
/// dependency for one form body would be the wrong trade — `reqwest` is pinned without the feature
/// that would have done it.
fn form(pairs: &[(&str, &str)]) -> String {
    let enc = |s: &str| {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                other => format!("%{other:02X}"),
            })
            .collect::<String>()
    };
    pairs.iter().map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect::<Vec<_>>().join("&")
}

// ── drills ──────────────────────────────────────────────────────────────

/// `drill.dead.node`: a node that stops existing, and a workspace that comes back somewhere else.
///
/// The taint and the agent pod together are what make the node LOOK dead rather than merely busy:
/// `NoExecute` evicts what is on it (which is what a dead node does), and the node's own agent is
/// the thing that would otherwise keep claiming its objects. The wait is `nodeDeadSecs` — read from
/// the region rather than assumed, because a cluster that raised it would otherwise fail this drill
/// for being patient — plus a beat's grace.
async fn dead_node(c: &mut Ctx) {
    let (Some(k), Some(ws)) = (c.kube.clone(), probe_workspace(c).await) else {
        return c.skip("drill.dead.node", "no kubeconfig, or no probe workspace to move");
    };
    let jwt = c.probe_jwt.clone();
    // Stopped FIRST, and outside the step: a running worktree is *interrupted* by its node dying,
    // never moved, and asking a dead node's running workspace to start is a 409 by design.
    let stop = api(c, &format!("/v1/workspaces/{ws}/stop"));
    if let Err(e) = post(c, &stop, &jwt, Value::Null).await {
        return c.skip("drill.dead.node", &format!("could not stop the workspace: {e:#}"));
    }
    let doc = api(c, &format!("/v1/workspaces/{ws}"));
    if let Err(e) = poll_json(c, &doc, &jwt, Duration::from_secs(60), |v| {
        v.get("state").and_then(Value::as_str) == Some("stopped")
    })
    .await
    {
        return c.skip("drill.dead.node", &format!("the workspace never stopped: {e:#}"));
    }
    let Some(owner) = node_of(c, &ws).await else {
        // The stop above is this function's own mutation, and it undoes it: a workspace left
        // stopped is one the LATER stages of a monthly run would find missing, and one a person
        // watching the console would have to start by hand.
        let start = api(c, &format!("/v1/workspaces/{ws}/start"));
        if let Err(e) = post(c, &start, &jwt, Value::Null).await {
            tracing::warn!(op = "restart", name = %ws, error = %format!("{e:#}"), "slo.drill.undo.failed");
        }
        return c.skip("drill.dead.node", "the workspace names no node");
    };
    let wait = Duration::from_secs(node_dead_secs(c).await + DEAD_SLACK);
    let body_cap = wait + DRAIN_CAP;
    let ws = ws.to_string();
    c.step("drill.dead.node", step_cap(body_cap), move |c| {
        let jwt = c.probe_jwt.clone();
        let start = api(c, &format!("/v1/workspaces/{ws}/start"));
        let doc = api(c, &format!("/v1/workspaces/{ws}"));
        let owner2 = owner.clone();
        async move {
            let body = async {
                kill_agent(&k, &owner2).await?;
                tokio::time::sleep(wait).await;
                post(c, &start, &jwt, Value::Null).await.context("the workspace would not start")?;
                poll_json(c, &doc, &jwt, DRAIN_CAP, |v| {
                    v.get("state").and_then(Value::as_str) == Some("ready")
                        && v.get("placement").and_then(Value::as_str).is_some_and(|n| n != owner2)
                })
                .await
                .with_context(|| format!("it never came back on a node other than {owner2}"))
            };
            drill::with_taint(&k, &owner, body_cap, body).await
        }
        .boxed()
    })
    .await;
}

/// The DaemonSet pod on one node. Deleting it is what makes the node stop reconciling its own
/// objects; the DaemonSet brings it straight back, which is fine — the taint is what keeps it off.
async fn kill_agent(k: &kube::Client, node: &str) -> Result<()> {
    let pods: kube::Api<k8s_openapi::api::core::v1::Pod> =
        kube::Api::namespaced(k.clone(), "kube-system");
    let list = pods
        .list(&kube::api::ListParams::default().labels("app=kloudlite-git-agent").fields(&format!("spec.nodeName={node}")))
        .await
        .map_err(|e| anyhow!("could not find {node}'s agent: {e}"))?;
    for p in list.items {
        let name = kube::ResourceExt::name_any(&p);
        pods.delete(&name, &kube::api::DeleteParams::default())
            .await
            .map_err(|e| anyhow!("could not delete {name}: {e}"))?;
    }
    Ok(())
}

/// The region's own `nodeDeadSecs`, or the compiled default. A cluster that raised the value would
/// otherwise fail this drill for doing exactly what it was configured to do.
async fn node_dead_secs(c: &Ctx) -> u64 {
    let url = admin(c, &format!("/admin/settings/clusters/{}", c.cfg.region));
    get(c, &url, &c.admin_jwt)
        .await
        .ok()
        .and_then(|v| v.pointer("/spec/nodeDeadSecs").and_then(Value::as_u64))
        .unwrap_or(NODE_DEAD_FALLBACK)
}

/// `drill.drain`: a planned retirement finishes, and nothing of ours was running on it.
///
/// A drain is not a cordon: it only sets the label the node's own agent watches, and the agent's
/// beat is what releases volumes and eventually stamps the sticky `drained …`. That stamp is the
/// gate an operator deletes a VM on, so it is the only thing worth waiting for. The undrain is in
/// the undo path — a node left labelled is a node placement will not use again.
async fn drain(c: &mut Ctx) {
    let (Some(k), region) = (c.kube.clone(), c.cfg.region.clone()) else {
        return c.skip("drill.drain", "no kubeconfig");
    };
    let busy = match probe_workspace(c).await {
        Some(ws) => node_of(c, &ws).await,
        None => None,
    };
    let node = match idle_node(&k, busy.as_deref()).await {
        Ok(n) => n,
        Err(e) => return c.skip("drill.drain", &format!("{e:#}")),
    };
    c.step("drill.drain", step_cap(DRAIN_CAP), move |c| {
        let jwt = c.admin_jwt.clone();
        let base = admin(c, &format!("/admin/clusters/{region}/nodes/{node}"));
        let reason = json!({ "reason": format!("slo probe drill {}", c.run_id) });
        async move {
            verb(c, &base, "drain", &jwt, &reason).await.context("the drain was refused")?;
            let body = async {
                stamped(&k, &node, DRAIN_CAP).await.context("the node never finished draining")
            };
            drill::undoing(DRAIN_CAP, body, || verb(c, &base, "undrain", &jwt, &reason)).await
        }
        .boxed()
    })
    .await;
}

/// One node verb on the admin API. Both halves take the same reason, which is what the audit row
/// carries — a drain nobody can explain is worse in the log than no drain at all.
async fn verb(c: &Ctx, base: &str, v: &str, jwt: &str, reason: &Value) -> Result<()> {
    post(c, &format!("{base}/{v}"), jwt, reason.clone()).await.map(|_| ())
}

/// A node holding nothing that is running, not already being retired, and not the one the taint
/// drill just used.
///
/// "Nothing running" is read off the WORKTREES, not the node: a drain only sets a label, and the
/// agent's beat releases volumes as they become releasable — but whatever is RUNNING there keeps
/// running, so a node with a live worktree on it would never stamp `drained` inside the drill's ten
/// minutes and the id would fail for the fleet behaving exactly as designed. Anyone's worktree
/// counts, not only the probe's: this drill touches a shared cluster.
async fn idle_node(k: &kube::Client, avoid: Option<&str>) -> Result<String> {
    use kloudlite_git_workspaces::crd;
    let busy = running_nodes(k).await?;
    let api: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(k.clone());
    let list = api.list(&kube::api::ListParams::default()).await.map_err(|e| anyhow!("could not list the nodes: {e}"))?;
    list.items
        .iter()
        .find(|n| {
            let name = kube::ResourceExt::name_any(*n);
            Some(name.as_str()) != avoid
                && !busy.contains(&name)
                && !n.metadata.labels.as_ref().is_some_and(|l| l.contains_key(crd::DECOMMISSION_LABEL))
                // A node already cordoned by a person is one somebody is retiring by hand.
                && !n.spec.as_ref().and_then(|s| s.unschedulable).unwrap_or(false)
        })
        .map(kube::ResourceExt::name_any)
        .ok_or_else(|| anyhow!("every node holds a running worktree, or is already draining"))
}

/// Every node with a Running workspace or environment placed on it.
async fn running_nodes(k: &kube::Client) -> Result<Vec<String>> {
    use kloudlite_git_workspaces::crd;
    let mut out = vec![];
    let ws: kube::Api<crd::Workspace> = kube::Api::all(k.clone());
    let env: kube::Api<crd::Environment> = kube::Api::all(k.clone());
    let p = kube::api::ListParams::default();
    for (node, running) in ws
        .list(&p)
        .await
        .map_err(|e| anyhow!("could not list the workspaces: {e}"))?
        .items
        .iter()
        .map(|w| (w.status.as_ref().map(|s| s.node_name.clone()), is_running(w.status.as_ref().map(|s| s.phase.as_str()))))
        .chain(
            env.list(&p)
                .await
                .map_err(|e| anyhow!("could not list the environments: {e}"))?
                .items
                .iter()
                .map(|e| (e.status.as_ref().map(|s| s.node_name.clone()), is_running(e.status.as_ref().map(|s| s.phase.as_str())))),
        )
    {
        if let (Some(node), true) = (node.filter(|n| !n.is_empty()), running) {
            out.push(node);
        }
    }
    Ok(out)
}

/// Anything but a stopped or failed phase is something a person could be typing into.
fn is_running(phase: Option<&str>) -> bool {
    !matches!(phase.unwrap_or_default(), "" | "Stopped" | "stopped" | "Failed" | "failed")
}

/// Wait for the agent's sticky `drained <RFC 3339>` stamp.
async fn stamped(k: &kube::Client, node: &str, cap: Duration) -> Result<()> {
    use kloudlite_git_workspaces::crd;
    let api: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(k.clone());
    let at = std::time::Instant::now();
    loop {
        let obj = api.get(node).await.map_err(|e| anyhow!("could not read {node}: {e}"))?;
        // Annotation first, label second: the agent stamps an annotation and `undrain` clears one,
        // but a value that long is not a legal label, so reading only labels would wait forever.
        let stamp = obj
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(crd::DECOMMISSION_STATUS))
            .or_else(|| obj.metadata.labels.as_ref().and_then(|l| l.get(crd::DECOMMISSION_STATUS)))
            .cloned()
            .unwrap_or_default();
        if stamp.starts_with(crd::DRAINED_PREFIX) {
            return Ok(());
        }
        if at.elapsed() >= cap {
            return Err(anyhow!("after {} ms it still reports {stamp:?}", cap.as_millis()));
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// `drill.redis.down`: the fleet keeps working with the Redis stream cut off.
///
/// The stream is a NUDGE and a view, never the record — every consumer that matters has a fallback
/// that does not depend on it. This drill is what keeps that sentence true: with egress to Redis
/// denied for the server and worker pods, a repo is still created and listed in the activity feed
/// (whose `repo_created` half has the fallback), a push still lands, and a PR still merges through
/// the worker's own beat.
///
/// The policy goes on the AKS cluster through an EXPLICIT in-cluster client, like `cp.failover`'s:
/// `Ctx::kube` follows `KUBECONFIG` into k3s, where none of those pods run.
async fn redis_down(c: &mut Ctx) {
    let Some(host) = c.cfg.redis_host.clone() else {
        return c.skip("drill.redis.down", "no KLOUDLITE_GIT_SLO_REDIS_HOST to deny");
    };
    let k = match drill::incluster() {
        Ok(k) => k,
        Err(e) => return c.skip("drill.redis.down", &format!("no in-cluster client: {e:#}")),
    };
    let ips = match resolve(c, &host).await {
        Ok(ips) => ips,
        Err(e) => return c.skip("drill.redis.down", &format!("{e:#}")),
    };
    let name = format!("{}-redis", c.prefix());
    let body_cap = REDIS_DOWN + Duration::from_secs(300);
    c.step("drill.redis.down", step_cap(body_cap), move |c| {
        async move {
            let body = async {
                // Long enough that anything holding a Redis connection has noticed, then the work
                // itself — the assertion is about what the fleet does WHILE it is cut off.
                tokio::time::sleep(REDIS_DOWN).await;
                without_redis(c, &name).await
            };
            drill::with_netpol(&k, "kloudlite-git", NETPOL, deny_egress(&ips), body_cap, body).await
        }
        .boxed()
    })
    .await;
}

/// The one NetworkPolicy this probe ever writes, named here because teardown deletes it blind on
/// every run — including runs that never went near a drill.
pub const NETPOL: &str = "slo-drill-redis";

/// Everywhere but Redis, for the two tiers that nudge it.
///
/// Expressed as "allow the world EXCEPT these addresses" because Kubernetes NetworkPolicy has no
/// deny rule: an egress policy is an allow-list, and `except` inside a wide CIDR is the only way to
/// punch one hole in it. DNS is opened separately — without it the pods cannot resolve anything at
/// all, and the drill would be measuring a DNS outage rather than a Redis one.
fn deny_egress(ips: &[String]) -> Value {
    json!({
        "podSelector": { "matchExpressions": [
            { "key": "app", "operator": "In", "values": ["kloudlite-git-srv", "kloudlite-git-worker"] }
        ]},
        "policyTypes": ["Egress"],
        "egress": [
            { "to": [{ "ipBlock": {
                "cidr": "0.0.0.0/0",
                "except": ips.iter().map(|ip| format!("{ip}/32")).collect::<Vec<_>>(),
            }}]},
            // Cluster DNS is inside the pod network, which the `except` above does not touch, but
            // an egress policy with no UDP/53 rule blocks it on some CNIs regardless.
            { "ports": [{ "protocol": "UDP", "port": 53 }, { "protocol": "TCP", "port": 53 }] },
        ],
    })
}

/// The addresses `host` resolves to, through the same `dig` stage 10 uses.
async fn resolve(c: &Ctx, host: &str) -> Result<Vec<String>> {
    let out = tools::plain(&c.programs.dig, &["+short", host], Duration::from_secs(10))
        .await
        .with_context(|| format!("could not resolve {host}"))?;
    let ips: Vec<String> = out
        .lines()
        .map(str::trim)
        // `dig +short` on a CNAME chain prints the intermediate names too; only the addresses can
        // go in an `ipBlock`, and a policy built from a hostname would silently deny nothing.
        .filter(|l| l.parse::<std::net::Ipv4Addr>().is_ok())
        .map(str::to_string)
        .collect();
    if ips.is_empty() {
        return Err(anyhow!("{host} resolves to no IPv4 address"));
    }
    Ok(ips)
}

/// The work that must still happen with the stream cut off: a repo created and visible in the feed,
/// a push, and a PR that merges.
async fn without_redis(c: &Ctx, name: &str) -> Result<()> {
    let jwt = c.probe_jwt.clone();
    let body = json!({ "owner": PROBE_USER, "name": name, "visibility": "private" });
    post(c, &api(c, "/v1/repos"), &jwt, body).await.context("the repo would not create")?;

    let work = c.tmp.join("git").join(name);
    std::fs::create_dir_all(&work).context("could not make a working tree")?;
    let g = |a: Vec<String>| super::git::git(c, a, Some(&work));
    g(vec!["init".into(), "-q".into(), "--initial-branch=main".into()]).await?;
    std::fs::write(work.join("README.md"), format!("# {name}\n")).context("could not write")?;
    g(vec!["add".into(), "-A".into()]).await?;
    g(vec!["commit".into(), "-q".into(), "-m".into(), "seed".into()]).await?;
    let url = format!("{}/{PROBE_USER}/{name}.git", c.cfg.git_url.trim_end_matches('/'));
    let push = super::git::authed(c, &["push", "-q", &url, "main"]);
    super::git::git(c, push, Some(&work)).await.context("the push failed with Redis down")?;

    g(vec!["checkout".into(), "-q".into(), "-b".into(), "slo".into()]).await?;
    std::fs::write(work.join("change.txt"), format!("{}\n", c.run_id)).context("could not write")?;
    g(vec!["add".into(), "-A".into()]).await?;
    g(vec!["commit".into(), "-q".into(), "-m".into(), "change".into()]).await?;
    let push = super::git::authed(c, &["push", "-q", &url, "slo"]);
    super::git::git(c, push, Some(&work)).await.context("the branch push failed")?;

    let refs = api(c, &format!("/api/{PROBE_USER}/{name}/refs"));
    let head = super::git::oid_of(&get(c, &refs, &jwt).await?, "slo")
        .ok_or_else(|| anyhow!("the branch never appeared"))?;
    let pulls = api(c, &format!("/v1/repos/{PROBE_USER}/{name}/pulls"));
    let pr = post(c, &pulls, &jwt, json!({ "title": "slo redis drill", "base": "main", "head": "slo" }))
        .await
        .context("the pull request would not open")?;
    let number = pr.get("number").and_then(Value::as_i64).ok_or_else(|| anyhow!("no pull number"))?;
    let merge = api(c, &format!("/v1/repos/{PROBE_USER}/{name}/pulls/{number}/merge?strategy=fast-forward"));
    post(c, &merge, &jwt, Value::Null).await.context("the merge was refused")?;
    // The merge runs in the worker, which is announced through the stream AND re-announced on the
    // owner's own 15 s beat — that fallback is what this half of the drill is about.
    poll_json(c, &refs, &jwt, Duration::from_secs(120), |r| {
        super::git::oid_of(r, "main").as_deref() == Some(head.as_str())
    })
    .await
    .context("the merge never landed with Redis down")?;

    // `repo_created` specifically: the PR half of the feed is stream-only ON PURPOSE
    // (`feed.rs`, "no fallback here"), so with Redis down it is expected to be quiet and asserting
    // on it would fail a drill that the system passed by design.
    let feed = api(c, &format!("/v1/activity?owner={PROBE_USER}"));
    let want = format!("{PROBE_USER}/{name}");
    poll_json(c, &feed, &jwt, Duration::from_secs(60), |v| created(v, &want))
        .await
        .context("the activity feed never showed the repo")
}

fn created(feed: &Value, repo: &str) -> bool {
    let events = feed.get("events").and_then(Value::as_array).or_else(|| feed.as_array());
    events.is_some_and(|rows| {
        rows.iter().any(|e| {
            e.get("kind").and_then(Value::as_str) == Some("repo_created")
                && e.get("repo").and_then(Value::as_str) == Some(repo)
        })
    })
}

// ── shared ──────────────────────────────────────────────────────────────

/// This run's cold workspace (stage 12's), found by the same `run-{id}` name prefix everything else
/// in this probe is addressed by — the weekly stage's own id is not carried across a stage boundary
/// and re-reading the listing is cheaper than a field that can go stale.
async fn probe_workspace(c: &Ctx) -> Option<String> {
    let want = format!("{}-cold", c.prefix());
    let rows = get(c, &api(c, "/v1/workspaces"), &c.probe_jwt).await.ok()?;
    rows.as_array()?
        .iter()
        .find(|r| r.get("name").and_then(Value::as_str) == Some(want.as_str()))
        .and_then(|r| r.get("id").and_then(Value::as_str))
        .map(str::to_string)
}

async fn node_of(c: &Ctx, ws: &str) -> Option<String> {
    get(c, &api(c, &format!("/v1/workspaces/{ws}")), &c.probe_jwt)
        .await
        .ok()?
        .get("placement")
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every id exactly once, whatever is configured. With no Azure credential, no kubeconfig and
    /// no Redis host, the console still owes seven rows — a stage that dropped ids when its
    /// preconditions were absent would make an unconfigured probe look like a healthy one.
    #[tokio::test]
    async fn monthly_produces_every_id_once() {
        let mut c = crate::testkit::ctx().await;
        c.kube = None;
        tokio::time::pause();
        run(&mut c).await;
        let ids: Vec<&str> = c.steps.iter().map(|s| s.slo_id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "bak.tarball.age",
                "bak.daily.slots",
                "bak.versioning",
                "bak.cosmos",
                "drill.dead.node",
                "drill.drain",
                "drill.redis.down",
            ]
        );
        assert_eq!(c.failed(), 0, "an unconfigured probe skips; it does not breach");
    }

    /// The slot names are the shell script's contract, and the daily check is about EXISTENCE:
    /// a missing weekday means a whole day's run has never succeeded, which "the newest backup is
    /// recent" cannot see.
    #[test]
    fn the_daily_slots_are_the_seven_the_backup_script_writes() {
        let have: Vec<String> = DAYS.iter().map(|d| format!("daily-{d}{SLOT_SUFFIX}")).collect();
        assert_eq!(have.len(), 7);
        assert!(have.contains(&"daily-Mon.tgz.enc".to_string()));
        // `date +%a`'s own answer for today has to be one of them, or the daily check is asking
        // for slot names the script never writes.
        assert!(DAYS.contains(&Utc::now().format("%a").to_string().as_str()));
    }

    /// A policy built from a hostname denies nothing: `ipBlock` takes CIDRs, and `dig +short` on a
    /// CNAME chain prints the intermediate NAMES alongside the addresses.
    #[test]
    fn the_redis_policy_only_ever_excepts_addresses() {
        let spec = deny_egress(&["10.0.0.7".into(), "10.0.0.8".into()]);
        let except = spec.pointer("/egress/0/to/0/ipBlock/except").and_then(Value::as_array).expect("except");
        assert_eq!(except.len(), 2);
        assert_eq!(except[0], "10.0.0.7/32");
        // DNS stays open: a pod that cannot resolve is a DNS outage, not the Redis one being drilled.
        assert!(spec.to_string().contains("\"port\":53"), "{spec}");
    }

    /// Only `repo_created`, and only this repo's. The PR half of the feed is stream-only on
    /// purpose, so asserting on it would fail a drill the system passed by design.
    #[test]
    fn only_this_repos_creation_counts() {
        let feed = json!([
            { "kind": "repo_created", "repo": "slo-probe/run-monthly-1-redis" },
            { "kind": "pull_merged", "repo": "slo-probe/run-monthly-1-redis" },
        ]);
        assert!(created(&feed, "slo-probe/run-monthly-1-redis"));
        assert!(!created(&feed, "slo-probe/run-monthly-2-redis"));
    }
}
