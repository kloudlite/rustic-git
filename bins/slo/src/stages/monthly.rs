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
use crate::ctx::Ctx;
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
/// Two marker-reconcile beats plus drift — see `without_redis`. The `repo_created` half of the
/// feed comes off the index markers, never the stream, and one beat was not a wait.
const FEED_FALLBACK: Duration = Duration::from_secs(150);

/// Long enough that a fleet leaning on Redis for anything load-bearing would show it, short enough
/// that the CronJob's two hours still fit the dead-node drill after it.
const REDIS_DOWN: Duration = Duration::from_secs(300);

/// A step's ceiling for a body that has an undo: always the body's own plus a minute. `Ctx::step`
/// times out by DROPPING the step's future, so an outer timeout that fired first would take the
/// undo with it — the drill's own cap has to be the one that wins.
fn step_cap(body: Duration) -> Duration {
    body + Duration::from_secs(60)
}

pub async fn run(c: &mut Ctx) {
    backups(c).await;
    dead_node(c).await;
    interrupted(c).await;
    drain(c).await;
    decommission(c).await;
    redis_down(c).await;
    clickhouse_down(c).await;
}

/// `ws.interrupted` and `env.clone.interrupted`: NOT AUTOMATED, for `drill.dead.node`'s reason.
///
/// Both refusals exist only while a node is genuinely down — a taint evicts pods but leaves the
/// node Ready, so `/v1` would place the worktree happily and the 409 these ids are about would
/// never be offered. They are walked by the same operator drill, in the window it opens.
async fn interrupted(c: &mut Ctx) {
    c.skip("ws.interrupted", NODE_LEVEL_DRILL);
    c.skip("env.clone.interrupted", NODE_LEVEL_DRILL);
}


/// `drill.clickhouse.down`: the history layer is optional, and the fleet must behave as if it is.
///
/// `KLOUDLITE_CLICKHOUSE_URL` unset is a supported deployment answered with `503 history
/// unavailable`, which the console renders as a flat placeholder — but an OUTAGE is a different
/// thing wearing the same shape, and nothing proved the tiers behave the same way through one. So
/// egress to ClickHouse is denied for the admin process, and what must hold is that every /v1 verb
/// still works and `/admin/history/*` answers 503 rather than 500.
async fn clickhouse_down(c: &mut Ctx) {
    let Some(host) = c.cfg.clickhouse_host.clone() else {
        return c.skip("drill.clickhouse.down", "no KLOUDLITE_SLO_CLICKHOUSE_HOST to deny");
    };
    let k = match drill::incluster() {
        Ok(k) => k,
        Err(e) => return c.skip("drill.clickhouse.down", &format!("no in-cluster client: {e:#}")),
    };
    let ips = match resolve(c, &host).await {
        Ok(ips) => ips,
        Err(e) => return c.skip("drill.clickhouse.down", &format!("{e:#}")),
    };
    let body_cap = Duration::from_secs(180);
    c.step("drill.clickhouse.down", step_cap(body_cap), move |c| {
        let jwt = c.probe_jwt.clone();
        let admin_jwt = c.admin_jwt.clone();
        let repos = api(c, "/v1/repos");
        let quota = api(c, "/v1/quota");
        let history = admin(c, "/admin/history/audit_events?range=1d&step=1h");
        let name = format!("{}-chd", c.prefix());
        async move {
            let body = async {
                // Long enough that a pooled connection has certainly failed over.
                tokio::time::sleep(Duration::from_secs(60)).await;
                // Ordinary work, unaffected: the history layer is a reader, never the record.
                get(c, &quota, &jwt).await.context("`/v1/quota` stopped answering with ClickHouse down")?;
                let owner = c.probe_user.clone();
                post(c, &repos, &jwt, json!({ "owner": owner, "name": name, "visibility": "private" }))
                    .await
                    .context("a repo could not be created with ClickHouse down")?;
                let _ = super::call(c, reqwest::Method::DELETE, &api(c, &format!("/v1/repos/{owner}/{name}")), &jwt, None).await;
                // And the history reads degrade rather than break: 503 is the contract the web
                // renders a placeholder for, and a 500 is the page falling over.
                let (status, text) = super::raw(c, reqwest::Method::GET, &history, &admin_jwt, None, &[]).await?;
                match status.as_u16() {
                    503 => Ok(()),
                    // Still answering is fine too: a replica may have taken over, and this drill
                    // is about what happens when it does NOT.
                    code if (200..300).contains(&code) => Ok(()),
                    code => Err(anyhow!("`/admin/history/*` answered {code} with ClickHouse down, not 503: {}", text.chars().take(160).collect::<String>())),
                }
            };
            drill::with_netpol(&k, "kloudlite", CH_NETPOL, deny_clickhouse(&ips), body_cap, body).await
        }
        .boxed()
    })
    .await;
}

/// The second policy this probe ever writes, deleted on every path out and blind in teardown.
pub const CH_NETPOL: &str = "slo-drill-clickhouse";

/// Everywhere but ClickHouse, for the one process that writes the `kloudlite` database.
fn deny_clickhouse(ips: &[String]) -> Value {
    json!({
        "podSelector": { "matchExpressions": [
            { "key": "app", "operator": "In", "values": ["kloudlite-admin", "kloudlite-api"] }
        ]},
        "policyTypes": ["Egress"],
        "egress": [
            { "to": [{ "ipBlock": {
                "cidr": "0.0.0.0/0",
                "except": ips.iter().map(|ip| format!("{ip}/32")).collect::<Vec<_>>(),
            }}]},
            { "ports": [{ "protocol": "UDP", "port": 53 }, { "protocol": "TCP", "port": 53 }] },
        ],
    })
}

/// `cluster.decommission`: the 409 gate, and the cordon behind it.
///
/// Drain and decommission are two distinct actions and only drain was drilled. The interesting
/// half is the REFUSAL: a decommission is 409 "not drained yet" until the node's own agent has
/// stamped the sticky `drained <RFC 3339>`, and that gate is the only thing between an operator
/// and cordoning a node that is still holding somebody's bytes. Both halves in one step, in that
/// order — a fleet that refused everything would pass the first alone, and one that cordoned
/// anything would pass the second.
///
/// Everything it does is undone: the drain is lifted and the cordon taken off on every path out —
/// including the path where the GATE IS OPEN and the first POST cordons the node, which is why
/// both decommission attempts live inside the `undoing` region rather than in front of it. It
/// never deletes anything — the console stops at the
/// cordon by design, and deleting the VM is a human's separate step.
async fn decommission(c: &mut Ctx) {
    let (Some(k), region) = (c.kube.clone(), c.cfg.region.clone()) else {
        return c.skip("cluster.decommission", "no kubeconfig");
    };
    // The same choice `drill.drain` makes, and it must not be the node that drill just used: two
    // nodes retiring at once on a shared cluster is a fleet with nowhere left to place anything.
    let busy = match probe_workspace(c).await {
        Some(ws) => node_of(c, &ws).await,
        None => None,
    };
    let node = match idle_node(&k, busy.as_deref()).await {
        Ok(n) => n,
        Err(e) => return c.skip("cluster.decommission", &format!("{e:#}")),
    };
    c.step("cluster.decommission", step_cap(DRAIN_CAP), move |c| {
        let jwt = c.admin_jwt.clone();
        let base = admin(c, &format!("/admin/clusters/{region}/nodes/{node}"));
        let reason = json!({ "reason": format!("slo probe decommission drill {}", c.run_id) });
        async move {
            // The undo is established BEFORE the first decommission POST, not after it. The gate
            // being OPEN is the very failure this id exists to catch — and it is also the state a
            // node an earlier run left stamped `drained` is in — so a POST outside this region
            // would cordon the node and return `Err` with nothing to uncordon it. Both mutations
            // go back on every path out, in the order that leaves the node usable.
            let undo = || async {
                use crate::drill::Cluster;
                let uncordon = k.cordon(&node, false).await.context("the node was left CORDONED");
                let undrained = verb(c, &base, "undrain", &jwt, &reason).await.context("the node was left DRAINING");
                uncordon.and(undrained)
            };
            let body = async {
                // Before the drain: nothing has stamped `drained`, so this must be refused.
                refused_until_drained(c, &base, &jwt, &reason).await?;
                verb(c, &base, "drain", &jwt, &reason).await.context("the drain was refused")?;
                stamped(&k, &node, DRAIN_CAP - Duration::from_secs(60))
                    .await
                    .context("the node never finished draining, so the gate could not be tried")?;
                // Now it must be TAKEN, and the node must actually be cordoned afterwards: the
                // console's own contract is that a decommission stops at `spec.unschedulable`.
                verb(c, &base, "decommission", &jwt, &reason)
                    .await
                    .context("the decommission was refused even though the agent had stamped `drained`")?;
                cordoned(&k, &node).await
            };
            drill::undoing(DRAIN_CAP, body, undo).await
        }
        .boxed()
    })
    .await;
}

/// The gate: a decommission before the stamp answers 409, and only 409.
///
/// A 5xx is not a refusal — the tier that fell over refused nothing, it could not answer — and a
/// 2xx is the gate being open, which is the whole failure this id exists for.
async fn refused_until_drained(c: &Ctx, base: &str, jwt: &str, reason: &Value) -> Result<()> {
    let (status, body) = super::raw(
        c,
        reqwest::Method::POST,
        &format!("{base}/decommission"),
        jwt,
        Some(reason.clone()),
        &[],
    )
    .await?;
    match status.as_u16() {
        409 => Ok(()),
        200..=299 => Err(anyhow!("a node that has not drained was ALLOWED to be decommissioned")),
        other => Err(anyhow!(
            "the decommission answered {other}, which is not the gate refusing: {}",
            body.chars().take(200).collect::<String>()
        )),
    }
}

/// The node is unschedulable — where a decommission stops, and no further.
async fn cordoned(k: &kube::Client, node: &str) -> Result<()> {
    let api: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(k.clone());
    let obj = api.get(node).await.map_err(|e| anyhow!("could not read {node}: {e}"))?;
    match obj.spec.and_then(|s| s.unschedulable) {
        Some(true) => Ok(()),
        _ => Err(anyhow!("the decommission was taken but {node} is not cordoned")),
    }
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
            let all = slots().await?;
            let newest = all
                .iter()
                .filter(|(n, _)| n.starts_with("hourly-") && n.ends_with(SLOT_SUFFIX))
                .map(|(_, at)| *at)
                .max();
            // Naming what IS there, not only what is missing: the first live monthly run found
            // `hourly-03.tgz` — the node's installed unit is an older script that writes plain,
            // unencrypted tarballs — and "no hourly tarball at all" sent the operator looking for
            // a backup that had in fact run.
            let Some(newest) = newest else {
                let seen: Vec<&str> = all
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .filter(|n| n.starts_with("hourly-"))
                    .take(3)
                    .collect();
                return Err(match seen.is_empty() {
                    true => anyhow!("the backup container holds no hourly tarball at all"),
                    false => anyhow!(
                        "the newest hourly blob is {}, not {SLOT_SUFFIX} — the encrypted backup unit is not the one installed on the node",
                        seen.join(", ")
                    ),
                });
            };
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
            let rows = slots().await?;
            let have: Vec<String> = rows.iter().map(|(n, _)| n.clone()).collect();
            // The encrypted unit fills one slot a day, so in its first week the slots from before
            // it existed cannot hold anything: only days at or after its oldest blob are due.
            let since = rows.iter().filter(|(n, _)| n.ends_with(SLOT_SUFFIX)).map(|(_, t)| *t).min();
            let missing: Vec<String> = slots_due(Utc::now(), since)
                .into_iter()
                .filter(|want| !have.contains(want))
                .collect();
            if !missing.is_empty() {
                // Same reason as `tarball_age`: an unencrypted `daily-Mon.tgz` beside the missing
                // `daily-Mon.tgz.enc` is a different problem from no backup at all, and the
                // operator should read which one this is.
                let plain: Vec<&String> = have.iter().filter(|n| n.starts_with("daily-") && !n.ends_with(SLOT_SUFFIX)).collect();
                return Err(match plain.is_empty() {
                    true => anyhow!("no backup in slot {}", missing.join(", ")),
                    false => anyhow!(
                        "no backup in slot {} — the container holds unencrypted slots instead ({}), so the encrypted unit is not the one installed",
                        missing.join(", "),
                        plain.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                    ),
                });
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// The daily slots that must be present at `now`: the last seven days' weekdays, minus any day
/// before `since` — the first encrypted blob's time, i.e. when the encrypted unit was installed.
/// `None` (no encrypted blob at all) leaves every slot due, which the caller then reports.
fn slots_due(now: chrono::DateTime<Utc>, since: Option<chrono::DateTime<Utc>>) -> Vec<String> {
    (0..7)
        .map(|back| now - chrono::Duration::days(back))
        .filter(|day| since.is_none_or(|s| day.date_naive() >= s.date_naive()))
        .map(|day| format!("daily-{}{SLOT_SUFFIX}", day.format("%a")))
        .collect()
}

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

/// `drill.dead.node`: NOT AUTOMATED, and a skip that says why.
///
/// The first live monthly run failed it by construction. The drill deleted the node's agent pod
/// and tainted the node, but the product's definition of dead is the NODE's own Ready condition
/// being non-True for `WS_NODE_DEAD_SECS` (`peer::unplaceable`) — the DaemonSet puts the agent back
/// in seconds and the node never leaves Ready, so nothing is ever un-placed and the drill was
/// measuring a state the fleet was never in.
///
/// A real node death cannot be produced from inside the cluster, and the probe must not be given
/// node-level access to produce one: a synthetic user with a way to stop kubelets is a bigger
/// hole than this id is worth. So the id stays in the catalogue and files a SKIP naming the
/// operator's own recipe — the console shows "not automated" rather than nothing at all, which is
/// the honest state. `drill.drain`, which uses the decommission label the product itself uses, is
/// the automated monthly path.
async fn dead_node(c: &mut Ctx) {
    c.skip("drill.dead.node", NODE_LEVEL_DRILL);
}

/// The reason every id that needs a genuinely dead node carries, naming where the recipe lives.
const NODE_LEVEL_DRILL: &str = "a dead node needs the operator's node-level drill: stop the kubelet on one pool node — recipe in deploy/k3s/README.md";



/// `drill.drain`: a drain does NOT interrupt what is running on the node.
///
/// The SLI says "without interrupting a running worktree" and the drill used to pick an IDLE node
/// on purpose, so that clause was vacuously true on every run: the documented guarantee — a
/// decommissioning node keeps running whatever it holds while releasing the rest — was the one
/// thing untested. So the node drained is the one holding this run's own RUNNING workspace, and
/// what is asserted is that the workspace is still running afterwards with the same pod, and that
/// the node's own beat stamped `draining` counting it.
///
/// It never waits for `drained`: a node with a running worktree on it must NOT reach that stamp,
/// and `cluster.decommission` is the id that walks the stamp on a node that can. The undrain is in
/// the undo path — a node left labelled is a node placement will not use again.
async fn drain(c: &mut Ctx) {
    let (Some(k), region) = (c.kube.clone(), c.cfg.region.clone()) else {
        return c.skip("drill.drain", "no kubeconfig");
    };
    let Some(ws) = probe_workspace(c).await else {
        return c.skip("drill.drain", "no probe workspace to keep running through a drain");
    };
    let Some(node) = node_of(c, &ws).await else {
        return c.skip("drill.drain", "the workspace names no node");
    };
    let before = pod_uid(&k, c, &ws).await;
    c.step("drill.drain", step_cap(DRAIN_CAP), move |c| {
        let jwt = c.admin_jwt.clone();
        let probe_jwt = c.probe_jwt.clone();
        let base = admin(c, &format!("/admin/clusters/{region}/nodes/{node}"));
        let doc = api(c, &format!("/v1/workspaces/{ws}"));
        let reason = json!({ "reason": format!("slo probe drill {}", c.run_id) });
        async move {
            verb(c, &base, "drain", &jwt, &reason).await.context("the drain was refused")?;
            let body = async {
                // The agent's own beat is `WS_DECOMMISSION_SECS` (30); two of them, so the stamp
                // below is a decision it made rather than one it has not reached yet.
                draining_stamp(&k, &node, DRAIN_CAP / 2).await?;
                let now = get(c, &doc, &probe_jwt).await.context("could not read the workspace")?;
                let state = now.get("state").and_then(Value::as_str).unwrap_or_default();
                if !matches!(state, "ready" | "running") {
                    return Err(anyhow!("a running workspace on a draining node went to `{state}`"));
                }
                // The pod itself, not only the phase: a controller that deleted and recreated it
                // has interrupted the person at the keyboard whatever the status says afterwards.
                let after = pod_uid(&k, c, &ws).await;
                if before.is_some() && after != before {
                    return Err(anyhow!("the workspace's pod was replaced while its node drained"));
                }
                Ok(())
            };
            drill::undoing(DRAIN_CAP, body, || verb(c, &base, "undrain", &jwt, &reason)).await
        }
        .boxed()
    })
    .await;
}

/// The workspace pod's uid, or `None` when it cannot be read — in which case the comparison above
/// is skipped rather than guessed at.
async fn pod_uid(k: &kube::Client, c: &Ctx, ws: &str) -> Option<String> {
    let ns = kloudlite_workspaces::crd::ws_namespace(&c.probe_user, "");
    let pods: kube::Api<k8s_openapi::api::core::v1::Pod> = kube::Api::namespaced(k.clone(), &ns);
    pods.get_opt(ws).await.ok()??.metadata.uid
}

/// Wait for the agent's `draining running=N …` stamp — the beat's own record that it is retiring
/// the node WITHOUT stopping what runs there. `drained` is a different stamp and a different id.
async fn draining_stamp(k: &kube::Client, node: &str, cap: Duration) -> Result<()> {
    use kloudlite_workspaces::crd;
    let api: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(k.clone());
    let at = std::time::Instant::now();
    loop {
        let obj = api.get(node).await.map_err(|e| anyhow!("could not read {node}: {e}"))?;
        let stamp = obj
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(crd::DECOMMISSION_STATUS))
            .cloned()
            .unwrap_or_default();
        if stamp.starts_with("draining") || stamp.starts_with(crd::DRAINED_PREFIX) {
            return Ok(());
        }
        if at.elapsed() >= cap {
            return Err(anyhow!("{node}'s agent never stamped its drain: it reports {stamp:?}"));
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
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
    use kloudlite_workspaces::crd;
    let busy = running_nodes(k).await?;
    let api: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(k.clone());
    let list = api.list(&kube::api::ListParams::default()).await.map_err(|e| anyhow!("could not list the nodes: {e}"))?;
    list.items
        .iter()
        .find(|n| {
            let name = kube::ResourceExt::name_any(*n);
            let labels = n.metadata.labels.as_ref();
            // A POOL node only: the drain and the `drained …` stamp are the node's own agent's
            // work, and the agent runs only where `kloudlite.io/session` or `/env` is set. The
            // control plane carries neither, has nothing to drain, and stamps nothing — picking
            // it labelled k3s-cp and waited the whole cap for a stamp that could never come.
            let pool = ["kloudlite.io/session", "kloudlite.io/env"]
                .iter()
                .any(|k| labels.and_then(|l| l.get(*k)).map(String::as_str) == Some("true"));
            pool && Some(name.as_str()) != avoid
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
    use kloudlite_workspaces::crd;
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
    use kloudlite_workspaces::crd;
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
            // The COUNTS, verbatim: the first live monthly run sat at `draining running=0 owned=0
            // copies=1 thin=0` for ten minutes — a lone replica copy that never healed or retired,
            // which is a product stall and reads as one only if the stamp is in the detail.
            return Err(anyhow!("after {} ms {node} still reports {stamp:?}", cap.as_millis()));
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
        return c.skip("drill.redis.down", "no KLOUDLITE_SLO_REDIS_HOST to deny");
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
            drill::with_netpol(&k, "kloudlite", NETPOL, deny_egress(&ips), body_cap, body).await
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
            // `kloudlite-admin` too: it is the `history` consumer group, and the claim about it is
            // that it IDLES with Redis down — which nothing was measuring, because the policy did
            // not reach it.
            // `kloudlite`, not `kloudlite-srv`: the srv pods carry the tier's name, not the
            // StatefulSet's (`app: kloudlite, role: server` in deploy/kloudlite.yaml).
            { "key": "app", "operator": "In", "values": ["kloudlite", "kloudlite-worker", "kloudlite-admin"] }
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
    let probe = c.probe_user.clone();
    let jwt = c.probe_jwt.clone();
    let body = json!({ "owner": probe, "name": name, "visibility": "private" });
    post(c, &api(c, "/v1/repos"), &jwt, body).await.context("the repo would not create")?;

    let work = c.tmp.join("git").join(name);
    std::fs::create_dir_all(&work).context("could not make a working tree")?;
    let g = |a: Vec<String>| super::git::git(c, a, Some(&work));
    g(vec!["init".into(), "-q".into(), "--initial-branch=main".into()]).await?;
    std::fs::write(work.join("README.md"), format!("# {name}\n")).context("could not write")?;
    g(vec!["add".into(), "-A".into()]).await?;
    g(vec!["commit".into(), "-q".into(), "-m".into(), "seed".into()]).await?;
    let url = format!("{}/{probe}/{name}.git", c.cfg.git_url.trim_end_matches('/'));
    let push = super::git::authed(c, &["push", "-q", &url, "main"]);
    super::git::git(c, push, Some(&work)).await.context("the push failed with Redis down")?;

    g(vec!["checkout".into(), "-q".into(), "-b".into(), "slo".into()]).await?;
    std::fs::write(work.join("change.txt"), format!("{}\n", c.run_id)).context("could not write")?;
    g(vec!["add".into(), "-A".into()]).await?;
    g(vec!["commit".into(), "-q".into(), "-m".into(), "change".into()]).await?;
    let push = super::git::authed(c, &["push", "-q", &url, "slo"]);
    super::git::git(c, push, Some(&work)).await.context("the branch push failed")?;

    let refs = api(c, &format!("/api/{probe}/{name}/refs"));
    let head = super::git::oid_of(&get(c, &refs, &jwt).await?, "slo")
        .ok_or_else(|| anyhow!("the branch never appeared"))?;
    let pulls = api(c, &format!("/v1/repos/{probe}/{name}/pulls"));
    let pr = post(c, &pulls, &jwt, json!({ "title": "slo redis drill", "base": "main", "head": "slo" }))
        .await
        .context("the pull request would not open")?;
    let number = pr.get("number").and_then(Value::as_i64).ok_or_else(|| anyhow!("no pull number"))?;
    let merge = api(c, &format!("/v1/repos/{probe}/{name}/pulls/{number}/merge?strategy=fast-forward"));
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
    // 150 s, not 60: the `repo_created` half is not a stream fallback at all — `feed.rs:206` builds
    // it from `repo_listing`, i.e. the index markers, which the owning node's own lane reconciles
    // every 30 s plus ~200 ms per repo of drift (`bins/server/src/lanes.rs:76`). The old 60 s cap
    // (58 s after `poll_json`'s own margin) was ONE beat, so a marker written just after a pass was
    // a failed drill for the fleet behaving exactly as designed. Two beats and the drift.
    let feed = api(c, &format!("/v1/activity?owner={probe}"));
    let want = format!("{probe}/{name}");
    poll_json(c, &feed, &jwt, FEED_FALLBACK, |v| created(v, &want))
        .await
        .context("the activity feed never showed the repo")?;

    // The admin process is the `history` consumer group, and the claim is that it IDLES: with the
    // stream unreachable it must keep answering its own reads rather than wedging on the consumer.
    // A 503 is the no-ClickHouse deployment and is fine; a 500 or a hang is the claim being false.
    let history = admin(c, "/admin/history/audit_events?range=1d&step=1h");
    let (status, text) = super::raw(c, reqwest::Method::GET, &history, &c.admin_jwt, None, &[]).await?;
    match status.as_u16() {
        503 => Ok(()),
        code if (200..300).contains(&code) => Ok(()),
        code => Err(anyhow!("the admin process answered {code} with Redis down: {}", text.chars().take(160).collect::<String>())),
    }
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
                "ws.interrupted",
                "env.clone.interrupted",
                "drill.drain",
                "cluster.decommission",
                "drill.redis.down",
                "drill.clickhouse.down",
            ]
        );
        assert_eq!(c.failed(), 0, "an unconfigured probe skips; it does not breach");
    }

    /// The slot names are the shell script's contract, and the daily check is about EXISTENCE:
    /// a missing weekday means a whole day's run has never succeeded, which "the newest backup is
    /// recent" cannot see.
    #[test]
    fn slots_before_the_encrypted_unit_existed_are_not_due_yet() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-06T20:38:00Z").unwrap().with_timezone(&Utc);
        assert_eq!(slots_due(now, None).len(), 7, "no encrypted blob: everything is due, and reported");
        let a_month = Some(now - chrono::Duration::days(30));
        assert_eq!(slots_due(now, a_month).len(), 7);
        let today = Some(now - chrono::Duration::hours(1));
        assert_eq!(slots_due(now, today), vec!["daily-Sun.tgz.enc".to_string()], "installed today: only today");
        let two_days = Some(now - chrono::Duration::days(2));
        assert_eq!(slots_due(now, two_days), ["daily-Sun", "daily-Sat", "daily-Fri"].map(|d| format!("{d}.tgz.enc")));
    }

    #[test]
    fn the_daily_slots_are_the_seven_the_backup_script_writes() {
        let have = slots_due(Utc::now(), None);
        assert_eq!(have.len(), 7);
        assert!(have.contains(&"daily-Mon.tgz.enc".to_string()));
        assert!(have.contains(&format!("daily-{}{SLOT_SUFFIX}", Utc::now().format("%a"))), "today's weekday is one of them");
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
