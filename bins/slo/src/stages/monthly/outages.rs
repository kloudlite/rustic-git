//! Monthly drills that take a dependency away — ClickHouse, Redis — and prove the platform keeps
//! writing without it.

use super::*;


/// `ws.interrupted` and `env.clone.interrupted`: NOT AUTOMATED, for `drill.dead.node`'s reason.
///
/// Both refusals exist only while a node is genuinely down — a taint evicts pods but leaves the
/// node Ready, so `/v1` would place the worktree happily and the 409 these ids are about would
/// never be offered. They are walked by the same operator drill, in the window it opens.
pub(crate) async fn interrupted(c: &mut Ctx) {
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
pub(crate) async fn clickhouse_down(c: &mut Ctx) {
    let Some(host) = c.cfg.clickhouse_host.clone() else {
        return c.skip("drill.clickhouse.down", "no KLOUDLITE_SLO_CLICKHOUSE_HOST to deny");
    };
    let k = match drill::incluster() {
        Ok(k) => k,
        Err(e) => return c.skip("drill.clickhouse.down", &format!("no in-cluster client: {e:#}")),
    };
    match drill::netpol_enforced(&k).await {
        Ok(true) => {}
        Ok(false) => return c.skip("drill.clickhouse.down", drill::NETPOL_UNENFORCED),
        Err(e) => return c.skip("drill.clickhouse.down", &format!("{e:#}")),
    }
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
                let _ = super::super::call(c, reqwest::Method::DELETE, &api(c, &format!("/v1/repos/{owner}/{name}")), &jwt, None).await;
                // And the history reads degrade rather than break: 503 is the contract the web
                // renders a placeholder for, and a 500 is the page falling over.
                let (status, text) = super::super::raw(c, reqwest::Method::GET, &history, &admin_jwt, None, &[]).await?;
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


/// Everywhere but ClickHouse, for the one process that writes the `kloudlite` database.
pub(crate) fn deny_clickhouse(ips: &[String]) -> Value {
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
pub(crate) async fn redis_down(c: &mut Ctx) {
    let Some(host) = c.cfg.redis_host.clone() else {
        return c.skip("drill.redis.down", "no KLOUDLITE_SLO_REDIS_HOST to deny");
    };
    let k = match drill::incluster() {
        Ok(k) => k,
        Err(e) => return c.skip("drill.redis.down", &format!("no in-cluster client: {e:#}")),
    };
    match drill::netpol_enforced(&k).await {
        Ok(true) => {}
        Ok(false) => return c.skip("drill.redis.down", drill::NETPOL_UNENFORCED),
        Err(e) => return c.skip("drill.redis.down", &format!("{e:#}")),
    }
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


/// Everywhere but Redis, for the two tiers that nudge it.
///
/// Expressed as "allow the world EXCEPT these addresses" because Kubernetes NetworkPolicy has no
/// deny rule: an egress policy is an allow-list, and `except` inside a wide CIDR is the only way to
/// punch one hole in it. DNS is opened separately — without it the pods cannot resolve anything at
/// all, and the drill would be measuring a DNS outage rather than a Redis one.
pub(crate) fn deny_egress(ips: &[String]) -> Value {
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
pub(crate) async fn resolve(c: &Ctx, host: &str) -> Result<Vec<String>> {
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
pub(crate) async fn without_redis(c: &Ctx, name: &str) -> Result<()> {
    let probe = c.probe_user.clone();
    let jwt = c.probe_jwt.clone();
    let body = json!({ "owner": probe, "name": name, "visibility": "private" });
    post(c, &api(c, "/v1/repos"), &jwt, body).await.context("the repo would not create")?;

    let work = c.tmp.join("git").join(name);
    std::fs::create_dir_all(&work).context("could not make a working tree")?;
    let g = |a: Vec<String>| super::super::git::git(c, a, Some(&work));
    g(vec!["init".into(), "-q".into(), "--initial-branch=main".into()]).await?;
    std::fs::write(work.join("README.md"), format!("# {name}\n")).context("could not write")?;
    g(vec!["add".into(), "-A".into()]).await?;
    g(vec!["commit".into(), "-q".into(), "-m".into(), "seed".into()]).await?;
    let url = format!("{}/{probe}/{name}.git", c.cfg.git_url.trim_end_matches('/'));
    let push = super::super::git::authed(c, &["push", "-q", &url, "main"]);
    super::super::git::git(c, push, Some(&work)).await.context("the push failed with Redis down")?;

    g(vec!["checkout".into(), "-q".into(), "-b".into(), "slo".into()]).await?;
    std::fs::write(work.join("change.txt"), format!("{}\n", c.run_id)).context("could not write")?;
    g(vec!["add".into(), "-A".into()]).await?;
    g(vec!["commit".into(), "-q".into(), "-m".into(), "change".into()]).await?;
    let push = super::super::git::authed(c, &["push", "-q", &url, "slo"]);
    super::super::git::git(c, push, Some(&work)).await.context("the branch push failed")?;

    let refs = api(c, &format!("/api/{probe}/{name}/refs"));
    let head = super::super::git::oid_of(&get(c, &refs, &jwt).await?, "slo")
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
        super::super::git::oid_of(r, "main").as_deref() == Some(head.as_str())
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
    poll_json(c, &feed, &jwt, FEED_FALLBACK, |v| created(v, &probe, name))
        .await
        .context("the activity feed never showed the repo")?;

    // The admin process is the `history` consumer group, and the claim is that it IDLES: with the
    // stream unreachable it must keep answering its own reads rather than wedging on the consumer.
    // A 503 is the no-ClickHouse deployment and is fine; a 500 or a hang is the claim being false.
    let history = admin(c, "/admin/history/audit_events?range=1d&step=1h");
    let (status, text) = super::super::raw(c, reqwest::Method::GET, &history, &c.admin_jwt, None, &[]).await?;
    match status.as_u16() {
        503 => Ok(()),
        code if (200..300).contains(&code) => Ok(()),
        code => Err(anyhow!("the admin process answered {code} with Redis down: {}", text.chars().take(160).collect::<String>())),
    }
}


/// A feed row's `repo` is the BARE name (`feed.rs`: `repo: r.name`); the owner is only in `href`.
/// Matching `owner/name` against it never matched anything, which is what failed this drill on
/// every run before the netpol was even in question.
pub(crate) fn created(feed: &Value, owner: &str, name: &str) -> bool {
    let href = format!("/{owner}/{name}");
    let events = feed.get("events").and_then(Value::as_array).or_else(|| feed.as_array());
    events.is_some_and(|rows| {
        rows.iter().any(|e| {
            e.get("kind").and_then(Value::as_str) == Some("repo_created")
                && e.get("repo").and_then(Value::as_str) == Some(name)
                && e.get("href").and_then(Value::as_str) == Some(href.as_str())
        })
    })
}

// ── shared ──────────────────────────────────────────────────────────────


/// This run's cold workspace (stage 12's), found by the same `run-{id}` name prefix everything else
/// in this probe is addressed by — the weekly stage's own id is not carried across a stage boundary
/// and re-reading the listing is cheaper than a field that can go stale.
pub(crate) async fn probe_workspace(c: &Ctx) -> Option<String> {
    let want = format!("{}-cold", c.prefix());
    let rows = get(c, &api(c, "/v1/workspaces"), &c.probe_jwt).await.ok()?;
    rows.as_array()?
        .iter()
        .find(|r| r.get("name").and_then(Value::as_str) == Some(want.as_str()))
        .and_then(|r| r.get("id").and_then(Value::as_str))
        .map(str::to_string)
}


pub(crate) async fn node_of(c: &Ctx, ws: &str) -> Option<String> {
    get(c, &api(c, &format!("/v1/workspaces/{ws}")), &c.probe_jwt)
        .await
        .ok()?
        .get("placement")
        .and_then(Value::as_str)
        .map(str::to_string)
}
