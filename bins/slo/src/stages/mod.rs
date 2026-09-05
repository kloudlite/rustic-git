//! Stage 0 (boot) and the last stage (teardown). Every journey stage between them lands here as
//! its own module.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use kloudlite_workspaces::slo::catalogue::Suite;
use serde_json::Value;

use crate::ctx::Ctx;

pub mod admin;
pub mod edge;
pub mod environment;
pub mod experience;
pub mod experience_teams;
pub mod experience_ws;
pub mod experience_env;
pub mod experience_gaps;
pub mod experience_admin;
pub mod git;
pub mod identity;
pub mod lifecycle;
pub mod monthly;
pub mod pr;
pub mod registry;
pub mod security;
pub mod weekly;
pub mod workspace;

/// The journey stages this file's neighbours implement, named as the catalogue's "Journey
/// step" column names them. Stamped onto every step, so a failed run reads as a place in the
/// journey rather than an id.
pub const IDENTITY: &str = "1 · Identity";
pub const GIT: &str = "2 · Git";
pub const PULL_REQUEST: &str = "3 · Pull request";
pub const REGISTRY: &str = "4 · Registry";
pub const WORKSPACE: &str = "5 · Workspace";
pub const ENVIRONMENT: &str = "6 · Environment";
pub const LIFECYCLE: &str = "7 · Lifecycle";
pub const ADMIN: &str = "8 · Admin";
pub const SECURITY: &str = "9 · Security";
/// Edge AND pipeline: the two halves are one journey step, and `deploy/slo.md` names it once.
pub const EDGE: &str = "10 · Edge";
/// The two suite-only stages. Both run AFTER the whole fast journey — weekly and monthly are the
/// fast journey plus their own stage, never a different journey.
pub const WEEKLY: &str = "12 · Weekly";
pub const MONTHLY: &str = "13 · Monthly";
/// Hourly's own stage. Numbered after monthly because the stage numbers are stored in ClickHouse
/// and renumbering two of them to keep the list contiguous would rewrite history for nothing.
pub use experience::EXPERIENCE;

/// One HTTP call, with the body carried into the error.
///
/// A non-2xx is an `Err` rather than a value: every caller here is a step whose meaning is "this
/// worked", and the few places that care about a REFUSAL ask for the status explicitly through
/// `status`. The body is clipped because a 500 from a tier can be a whole HTML page, and the
/// interesting part is always its first line.
pub(crate) async fn call(
    c: &Ctx,
    method: reqwest::Method,
    url: &str,
    token: &str,
    body: Option<Value>,
) -> Result<Value> {
    let (status, text) = raw(c, method, url, token, body, &[]).await?;
    if !status.is_success() {
        return Err(anyhow!("{status}: {}", text.chars().take(300).collect::<String>()));
    }
    // Every route here answers JSON except the ones that answer nothing (204, and the api's
    // plain-text 202 on a merge); `Null` is the honest reading of both.
    Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
}

pub(crate) async fn get(c: &Ctx, url: &str, token: &str) -> Result<Value> {
    call(c, reqwest::Method::GET, url, token, None).await
}

pub(crate) async fn post(c: &Ctx, url: &str, token: &str, body: Value) -> Result<Value> {
    call(c, reqwest::Method::POST, url, token, Some(body)).await
}

/// The status and body, whatever they are. For the steps that measure a REFUSAL, and for the web
/// app, which answers HTML.
pub(crate) async fn raw(
    c: &Ctx,
    method: reqwest::Method,
    url: &str,
    token: &str,
    body: Option<Value>,
    headers: &[(&str, String)],
) -> Result<(reqwest::StatusCode, String)> {
    let mut req = c.http.request(method, url);
    if !token.is_empty() {
        req = req.header("authorization", c.bearer(token));
    }
    for (k, v) in headers {
        req = req.header(*k, v.clone());
    }
    if let Some(b) = body {
        req = req.json(&b);
    }
    // `without_url`: reqwest puts the whole URL in its Display, and these URLs carry query
    // strings — `?poll=` on the CLI handshake is a one-shot credential. A connection error is the
    // one path where a secret reaches a step detail without anyone formatting it there.
    let r = req.send().await.map_err(|e| anyhow!("{}", e.without_url()))?;
    let status = r.status();
    Ok((status, r.text().await.unwrap_or_default()))
}

/// `{api_url}{path}`, with the trailing slash the deployment may or may not have set removed once.
pub(crate) fn api(c: &Ctx, path: &str) -> String {
    format!("{}{path}", c.cfg.api_url.trim_end_matches('/'))
}

/// The same, against the admin process. Deliberately a second function rather than a flag: the two
/// URLs are different processes, and `sec.user.process` is the SLO that says so.
pub(crate) fn admin(c: &Ctx, path: &str) -> String {
    format!("{}{path}", c.cfg.admin_url.trim_end_matches('/'))
}

/// `GET url` until `want` is satisfied, or `cap` elapses.
///
/// Concrete rather than a generic "poll this closure": all three waits in the journey are the same
/// shape — a read that converges — and an async closure over `&Ctx` is exactly the thing that does
/// not survive being boxed into a `Send` step future.
///
/// The interval is fixed and short: these waits are seconds, and a backoff would turn a 900 ms
/// convergence into a 2 s sample and quietly reshape every latency SLO built on one.
pub(crate) async fn poll_json(
    c: &Ctx,
    url: &str,
    token: &str,
    cap: Duration,
    want: impl Fn(&Value) -> bool,
) -> Result<()> {
    let start = std::time::Instant::now();
    // Two seconds inside the caller's ceiling, so a poll that never sees what it wants reports
    // WHAT it last saw instead of the step's own bare "timed out" swallowing the evidence.
    let cap = cap.saturating_sub(Duration::from_secs(2));
    let mut why;
    loop {
        match get(c, url, token).await {
            Ok(v) if want(&v) => return Ok(()),
            Ok(v) => {
                let seen = v.to_string();
                let cut = seen.char_indices().nth(160).map_or(seen.len(), |(i, _)| i);
                why = format!("the answer does not have it yet; last answer: {}", &seen[..cut]);
            }
            Err(e) => why = format!("{e:#}"),
        }
        if start.elapsed() >= cap {
            return Err(anyhow!("not there after {} ms: {why}", cap.as_millis()));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// A leftover this old cannot belong to a run still in flight — the fast suite's own
/// `activeDeadlineSeconds` is 900 — so boot may sweep it. Without the age test, two overlapping
/// runs would delete each other's live objects, which is a far worse failure than a leak.
const STALE_SECS: i64 = 3600;

/// Stage 0: make the working tree, and sweep what a crashed earlier run left behind.
///
/// Sweeping at boot rather than only at teardown is what makes a killed run self-healing: the
/// probe's owner holds nothing but probe objects, so anything older than `STALE_SECS` is litter.
pub async fn boot(c: &mut Ctx) {
    if let Err(e) = std::fs::create_dir_all(&c.tmp) {
        tracing::error!(op = "mkdir", name = %c.tmp.display(), error = %e, "slo.boot.failed");
    }
    let now = chrono::Utc::now().timestamp();
    let swept = sweep_all(c, move |name| stale(name, now)).await;
    tracing::info!(count = swept, "slo.boot.completed");
}

/// The last stage. It runs unconditionally, including after a panic, and every delete is
/// best-effort: a leak is swept by the next run's boot, while an error propagated from here would
/// lose the report that says why the run failed in the first place.
pub async fn teardown(c: &mut Ctx) {
    undo_drills(c).await;
    undo_grants(c).await;
    hide(c).await;
    unprotect(c).await;
    let prefix = c.prefix();
    let mut swept = sweep_all(c, move |name| name.starts_with(&prefix)).await;
    swept += drop_env_volume(c).await;
    swept += drop_extra_volumes(c).await;
    tracing::info!(count = swept, "slo.teardown.completed");
}

/// Put the three fleet mutations a drill makes back, on EVERY run — not only the monthly one.
///
/// A drill undoes itself (`crate::drill`), and that undo is the first thing a killed pod loses:
/// the journey runs in a child process, and a child that is OOM-killed or hits the CronJob's
/// deadline mid-drill leaves a node tainted, a node cordoned or the fleet cut off from Redis with
/// nothing running that would put it back. This is the parent's sweep, so it runs after the child
/// whatever the child did — and unconditionally, because the run that left the mess is by
/// definition not the run that is cleaning it up.
async fn undo_drills(c: &mut Ctx) {
    if let Some(k) = c.kube.clone() {
        crate::drill::sweep_nodes(&k, &c.tmp).await;
    }
    // The NetworkPolicy is on the OTHER cluster — the one the probe runs in — so it needs its own
    // client, and not having one is the ordinary case outside a pod.
    match crate::drill::incluster() {
        Ok(k) => {
            use crate::drill::Cluster;
            if let Err(e) = k.netpol("kloudlite", crate::stages::monthly::NETPOL, None).await {
                tracing::warn!(kind = "netpol", error = %format!("{e:#}"), "slo.drill.sweep.failed");
            }
        }
        Err(e) => tracing::debug!(error = %format!("{e:#}"), "slo.drill.sweep.skipped"),
    }
}

/// Put the two GRANTS stage 14 makes back, on every run and whatever the child did.
///
/// Both are the same class of leftover as a taint: they are made by a step and undone by the same
/// step, so the run that is killed mid-step is exactly the one that leaves them standing — and
/// neither has a name teardown's prefix sweep could ever match. A raised quota is allocation
/// nobody decided on; a second superadmin is the whole authorization model.
async fn undo_grants(c: &mut Ctx) {
    let other_email = c.other_email.clone();
    let probe = c.probe_user.clone();
    // Read first, for the same reason as the roster below: the PUT files an audit row, and the
    // fast suite never touches the quota, so every five minutes would otherwise log a restore of
    // a limit that was never moved. Only the six limits are compared — `regions` is a grant this
    // probe never writes and the PUT body never mentions.
    let want = experience_admin::probe_quota();
    let detail = admin(c, &format!("/admin/owners/{probe}"));
    let same = get(c, &detail, &c.admin_jwt.clone()).await.ok().and_then(|v| v.get("limit").cloned()).is_some_and(|have| {
        want.as_object().unwrap().iter().all(|(k, v)| have.get(k) == Some(v))
    });
    if !same {
        let url = admin(c, &format!("/admin/quota/{probe}"));
        let body = serde_json::json!({ "spec": want, "note": "slo probe quota restore" });
        match call(c, reqwest::Method::PUT, &url, &c.admin_jwt.clone(), Some(body)).await {
            Ok(_) => tracing::info!(kind = "quota", name = probe, "slo.teardown.restored"),
            Err(e) => tracing::warn!(kind = "quota", op = "restore", error = %format!("{e:#}"), "slo.teardown.failed"),
        }
    }
    // Read first: the DELETE is an admin write with an audit row of its own, and filing one per
    // run for an account that is not on the roster is noise in the log a human reads.
    let all = admin(c, "/api/admin/superadmins");
    let listed = get(c, &all, &c.admin_jwt.clone()).await.ok().is_some_and(|v| {
        v.as_array().unwrap_or(&vec![]).iter().any(|r| {
            r.get("_id").and_then(Value::as_str).is_some_and(|u| u.eq_ignore_ascii_case(&other_email))
        })
    });
    if listed {
        let one = admin(c, &format!("/api/admin/superadmins/{other_email}"));
        let body = serde_json::json!({ "note": "slo probe teardown" });
        match call(c, reqwest::Method::DELETE, &one, &c.admin_jwt.clone(), Some(body)).await {
            Ok(_) => tracing::info!(kind = "superadmin", name = other_email, "slo.teardown.restored"),
            Err(e) => tracing::warn!(kind = "superadmin", op = "revoke", error = %format!("{e:#}"), "slo.teardown.failed"),
        }
    }
}

/// Take the protection rule off every `run-*` repo, BEFORE the deletes.
///
/// `repo.protection` protects `main` and unprotects it again, so only a run killed between the two
/// leaves one — and it leaves it on a repo the sweep below is about to delete. That delete is
/// best-effort like every other one here: a rule left on a repo that survives is what breaks the
/// NEXT run's stage 2 push, and this is the only thing that removes it.
async fn unprotect(c: &mut Ctx) {
    let probe = c.probe_user.clone();
    let prefix = c.prefix();
    let jwt = c.probe_jwt.clone();
    let repos = list(
        c,
        &Kind { kind: "repo", list: "/v1/repos", name_field: "name", id_field: "name", del: |_, _| String::new() },
        &probe,
        &jwt,
    )
    .await;
    for (name, _) in repos.into_iter().filter(|(n, _)| n.starts_with(&prefix)) {
        let url = api(c, &format!("/v1/repos/{probe}/{name}/protection"));
        let body = serde_json::json!({ "pattern": "main", "remove": true });
        if let Err(e) = post(c, &url, &jwt, body).await {
            tracing::warn!(kind = "repo", op = "unprotect", name = %name, error = %format!("{e:#}"), "slo.teardown.failed");
        }
    }
}

/// Flip everything this run may have published back to private, BEFORE the deletes.
///
/// `web.repo.page` and `reg.visibility` both publish on purpose and both restore on their own — but
/// a run that died between the two halves leaves a public repo or a public image standing until the
/// next run's boot sweep reaches it, and until then stage 9's `sec.private.repo` reads a hole and
/// calls it a passing security check. Deleting a public object is not the same as making it
/// private first: a delete that is refused (a volume still referenced, an image the registry is
/// mid-GC on) leaves it public. Best effort and logged, like every other line in teardown.
async fn hide(c: &mut Ctx) {
    let probe = c.probe_user.clone();
    if let Some(repo) = c.state.repo.clone() {
        let url = api(c, &format!("/v1/repos/{probe}/{repo}"));
        let body = serde_json::json!({ "visibility": "private" });
        match call(c, reqwest::Method::PATCH, &url, &c.probe_jwt.clone(), Some(body)).await {
            Ok(_) => tracing::info!(kind = "repo", name = %repo, "slo.teardown.hidden"),
            Err(e) => tracing::warn!(kind = "repo", op = "hide", name = %repo, error = %format!("{e:#}"), "slo.teardown.failed"),
        }
    }
    let prefix = c.prefix();
    for name in images(c, &probe, &c.probe_jwt.clone(), &|n: &str| n.starts_with(&prefix)).await {
        let url = api(c, &format!("/api/{probe}/{name}/imagevisibility?visibility=private"));
        match post(c, &url, &c.probe_jwt.clone(), Value::Null).await {
            Ok(_) => tracing::info!(kind = "image", name = %name, "slo.teardown.hidden"),
            Err(e) => tracing::warn!(kind = "image", op = "hide", name = %name, error = %format!("{e:#}"), "slo.teardown.failed"),
        }
    }
}

/// The environment's volume, by name.
///
/// The prefix sweep above cannot always reach it: a `Volume` is reference-counted, and the delete
/// is refused while the environment's own finalizer is still running — so this runs AFTER the
/// sweep, snapshot first (the last snapshot of a detached volume takes the volume with it) and
/// then the volume itself. Best effort, like every other delete here: what is left is litter the
/// next run's boot sweep collects.
async fn drop_env_volume(c: &mut Ctx) -> usize {
    let Some(volume) = c.state.env_volume.clone() else { return 0 };
    let mut gone = 0;
    let snapshot = c.state.env_snapshot.clone();
    for url in snapshot
        .map(|s| api(c, &format!("/v1/volumes/{volume}/snapshots/{s}")))
        .into_iter()
        .chain(std::iter::once(api(c, &format!("/v1/volumes/{volume}"))))
    {
        match call(c, reqwest::Method::DELETE, &url, &c.probe_jwt.clone(), None).await {
            Ok(_) => {
                gone += 1;
                tracing::info!(kind = "volume", name = %volume, "slo.teardown.deleted");
            }
            Err(e) => tracing::warn!(kind = "volume", op = "delete", name = %volume, error = %format!("{e:#}"), "slo.teardown.failed"),
        }
    }
    gone
}

/// The volumes a stage registered by name, after the prefix sweep has taken their working copies.
///
/// The sweep matches a volume on `display_name` and usually gets there first; usually is not good
/// enough for an object that is a subvolume on a node — a volume the sweep raced (its parent's
/// finalizer still running) would otherwise be left for the next run's boot, and a boot sweep that
/// never happens is a leak per hour. `DELETE /v1/volumes/{name}` takes a detached volume with all
/// its snapshots, so one call each is the whole of it.
async fn drop_extra_volumes(c: &mut Ctx) -> usize {
    let mut gone = 0;
    for v in c.state.extra_volumes.clone() {
        let url = api(c, &format!("/v1/volumes/{v}"));
        gone += del(c, "volume", &v, &url, &c.probe_jwt.clone()).await as usize;
    }
    gone
}

/// One `/v1` collection teardown owns: how to list it, which field carries the name the prefix is
/// matched against, and how to address one for delete.
struct Kind {
    kind: &'static str,
    list: &'static str,
    /// The caller-chosen name — the only field a prefix means anything in. A `Request`'s
    /// generated `req-…` id would match nothing, which is why requests are swept separately.
    name_field: &'static str,
    /// What the delete path takes. Often the same field; `_id` for a credential, whose name is
    /// not unique.
    id_field: &'static str,
    /// `(id, owner)`. The owner matters for repos only, which are addressed under theirs — every
    /// other collection is addressed by id alone and ignores it.
    del: fn(&str, &str) -> String,
}

/// Order matters at exactly one point: a volume is reference-counted, so its workspace and
/// environment must be gone before a volume delete can succeed at all.
const KINDS: &[Kind] = &[
    Kind { kind: "workspace", list: "/v1/workspaces", name_field: "name", id_field: "id", del: |id, _| format!("/v1/workspaces/{id}") },
    Kind { kind: "environment", list: "/v1/environments", name_field: "name", id_field: "id", del: |id, _| format!("/v1/environments/{id}") },
    // `display_name`, not `name`: a volume's `name` is the ws/env id (`ws-a1b2…`), which carries no
    // run prefix at all — the caller-chosen name only survives on `display_name`, so matching on
    // `name` swept nothing and every probe volume leaked.
    Kind { kind: "volume", list: "/v1/volumes", name_field: "display_name", id_field: "name", del: |n, _| format!("/v1/volumes/{n}") },
    Kind { kind: "repo", list: "/v1/repos", name_field: "name", id_field: "name", del: |n, owner| format!("/v1/repos/{owner}/{n}") },
    Kind { kind: "token", list: "/v1/tokens", name_field: "name", id_field: "_id", del: |id, _| format!("/v1/tokens/{id}") },
    // The id is escaped, not interpolated: an ssh credential's id is its `SHA256:<base64>`
    // fingerprint and base64 contains `/`, so the plain `format!` built a three-segment path that
    // matched no route and fell through to the GET-only fallback as a 405 — every probe key this
    // sweep ever tried to delete was left standing.
    Kind { kind: "key", list: "/v1/keys", name_field: "name", id_field: "_id", del: |id, _| format!("/v1/keys/{}", experience_gaps::path_seg(id)) },
    // `id.cli.flow` mints a real 30-day CLI token every five minutes. Its own collection, because
    // a CLI token is not listed by `/v1/tokens` — without this the probe would leak one credential
    // per run forever, which is a worse thing to own than the SLO is to measure.
    Kind { kind: "cli-token", list: "/v1/cli/tokens", name_field: "name", id_field: "id", del: |id, _| format!("/v1/cli/tokens/{id}") },
];

/// Both tenants. The Experience suite makes the second user a real participant — it is invited to a
/// team, clones a team repo and is refused one — so `slo-other` can now own `run-*` objects of its
/// own (a key, a token, an accepted invite's membership), and a sweep that only ever ran as
/// `slo-probe` would leak every one of them.
async fn sweep_all<M: Fn(&str) -> bool>(c: &mut Ctx, matches: M) -> usize {
    let other = c.other_user.clone();
    let probe = c.probe_user.clone();
    let mut gone = sweep(c, &probe, c.probe_jwt.clone(), &matches).await;
    gone += sweep(c, &other, c.other_jwt.clone(), &matches).await;
    gone
}

/// List every object one owner holds and delete the ones `matches` claims. Returns how many went.
async fn sweep<M: Fn(&str) -> bool>(c: &mut Ctx, owner: &str, jwt: String, matches: &M) -> usize {
    let mut gone = 0;
    for k in KINDS {
        for (name, id) in list(c, k, owner, &jwt).await {
            if !matches(&name) {
                continue;
            }
            let url = format!("{}{}", c.cfg.api_url.trim_end_matches('/'), (k.del)(&id, owner));
            match c.http.delete(&url).header("authorization", c.bearer(&jwt)).send().await {
                Ok(r) if r.status().is_success() => {
                    gone += 1;
                    tracing::info!(kind = k.kind, name = %name, "slo.teardown.deleted");
                }
                // Best-effort by design: a 409 here is usually "something still references it",
                // and the next run's boot sweep gets it once that reference is gone.
                Ok(r) => tracing::warn!(kind = k.kind, op = "delete", name = %name, error = %r.status(), "slo.teardown.failed"),
                Err(e) => tracing::warn!(kind = k.kind, op = "delete", name = %name, error = %e, "slo.teardown.failed"),
            }
        }
    }
    gone += deny_requests(c, owner, &jwt, matches).await;
    gone += sweep_images(c, owner, &jwt, matches).await;
    gone += sweep_teams(c, &jwt, matches).await;
    gone
}

/// Teams, last: a team is deleted only by an OWNER (`crates/api/src/teams.rs`, `delete_team`), and
/// the delete is refused with 409 while the team still owns repositories — so the team's own repos
/// go first, under the same prefix rule. `/v1/teams` lists the teams the CALLER is in, which is why
/// this is per-JWT rather than per-owner: the second user sees the same team while its membership
/// lasts and must not try to delete it (it is not the owner) — the 403 is logged and harmless.
async fn sweep_teams<M: Fn(&str) -> bool>(c: &mut Ctx, jwt: &str, matches: &M) -> usize {
    let mut gone = 0;
    let teams = list(
        c,
        &Kind { kind: "team", list: "/v1/teams", name_field: "_id", id_field: "_id", del: |s, _| format!("/v1/teams/{s}") },
        "",
        jwt,
    )
    .await;
    for (slug, _) in teams {
        if !matches(&slug) {
            continue;
        }
        // The team's repositories, which block the delete. Same prefix, the team as the owner.
        for (name, _) in
            list(c, &Kind { kind: "repo", list: "/v1/repos", name_field: "name", id_field: "name", del: |_, _| String::new() }, &slug, jwt)
                .await
        {
            if matches(&name) {
                gone += del(c, "repo", &name, &api(c, &format!("/v1/repos/{slug}/{name}")), jwt).await as usize;
            }
        }
        // Every credential minted UNDER the team, not only the ones whose name carries the run
        // prefix: `team.repo.shared`'s token is named after the run id without it, and a git
        // credential that outlives its team is a credential nobody can see to revoke.
        for (name, id) in list(
            c,
            &Kind { kind: "token", list: "/v1/tokens", name_field: "name", id_field: "_id", del: |_, _| String::new() },
            &slug,
            jwt,
        )
        .await
        {
            gone += del(c, "token", &name, &api(c, &format!("/v1/tokens/{id}")), jwt).await as usize;
        }
        // A team with an orphaned workspace is worse than a leaked team: the workspace is billed
        // to an owner that no longer exists and no listing anywhere shows it. So the team is
        // deleted only once its workspaces are gone, and the sweep gives up on it otherwise.
        if let Err(e) = drain_team(c, &slug, jwt).await {
            tracing::warn!(kind = "team", op = "drain", name = %slug, error = %format!("{e:#}"), "slo.teardown.failed");
            continue;
        }
        gone += del(c, "team", &slug, &api(c, &format!("/v1/teams/{slug}")), jwt).await as usize;
    }
    gone
}

/// How long a team's workspaces have to actually go before the team may be deleted.
pub(crate) const TEAM_DRAIN: Duration = Duration::from_secs(60);

/// Delete every workspace a team holds and wait until the listing is empty.
///
/// `Err` means DO NOT DELETE THE TEAM — a listing that failed, a delete that was refused, or a
/// workspace still standing at the cap. Deleting the team then would strand a subvolume under an
/// owner that no longer resolves, which no sweep on any tier can find again; a leaked team is a
/// row the next run's prefix sweep picks up.
pub(crate) async fn drain_team(c: &Ctx, slug: &str, jwt: &str) -> Result<()> {
    let url = api(c, &format!("/v1/workspaces?team={slug}"));
    let rows = get(c, &url, jwt).await.context("could not list the team's workspaces")?;
    let ids: Vec<String> = rows
        .as_array()
        .map(|rows| rows.iter().filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_string)).collect())
        .unwrap_or_default();
    for id in ids {
        call(c, reqwest::Method::DELETE, &api(c, &format!("/v1/workspaces/{id}")), jwt, None)
            .await
            .with_context(|| format!("could not delete the team workspace {id}"))?;
    }
    // The delete is a wish — the workspace goes when its finalizer has dropped the worktree — so
    // the listing going empty is the only thing that says the team is safe to take.
    poll_json(c, &url, jwt, TEAM_DRAIN, |v| v.as_array().is_some_and(|rows| rows.is_empty()))
        .await
        .context("the team still holds a workspace")
}

/// One best-effort DELETE, logged the way every other line in teardown logs. `true` when it went.
async fn del(c: &Ctx, kind: &'static str, name: &str, url: &str, jwt: &str) -> bool {
    match c.http.delete(url).header("authorization", c.bearer(jwt)).send().await {
        Ok(r) if r.status().is_success() => {
            tracing::info!(kind, name = %name, "slo.teardown.deleted");
            true
        }
        Ok(r) => {
            tracing::warn!(kind, op = "delete", name = %name, error = %r.status(), "slo.teardown.failed");
            false
        }
        Err(e) => {
            tracing::warn!(kind, op = "delete", name = %name, error = %e, "slo.teardown.failed");
            false
        }
    }
}

/// `Request` has no delete on any tier — only a superadmin decision — so the sweep DENIES a
/// leftover instead. That is what teardown actually needs: a pending request blocks the next
/// run's `req.queue` step (one pending per owner per kind), and a denied one does not.
async fn deny_requests<M: Fn(&str) -> bool>(c: &mut Ctx, owner: &str, jwt: &str, matches: &M) -> usize {
    let mut gone = 0;
    // The reason, not the name: the id is a server-generated `req-…` that no prefix can match.
    for (reason, id) in
        list(c, &Kind { kind: "request", list: "/v1/requests", name_field: "reason", id_field: "id", del: |_, _| String::new() }, owner, jwt).await
    {
        if !matches(&reason) {
            continue;
        }
        let url = format!("{}/admin/requests/{id}/deny", c.cfg.admin_url.trim_end_matches('/'));
        match c
            .http
            .post(&url)
            .header("authorization", c.bearer(&c.admin_jwt))
            .json(&serde_json::json!({ "note": "slo probe teardown" }))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                gone += 1;
                tracing::info!(kind = "request", name = %id, "slo.teardown.deleted");
            }
            // 409: already decided (the admin stage denied it), which is the state teardown wants.
            Ok(r) if r.status() == reqwest::StatusCode::CONFLICT => gone += 1,
            Ok(r) => tracing::warn!(kind = "request", op = "deny", name = %id, error = %r.status(), "slo.teardown.failed"),
            Err(e) => tracing::warn!(kind = "request", op = "deny", name = %id, error = %e, "slo.teardown.failed"),
        }
    }
    gone
}

/// Images are not a `/v1` collection: they are listed and deleted through the server tier's
/// browse API, which the api process proxies at `/api/{owner}/…`. A delete is a POST with no body
/// (`crates/api/src/images.rs`), not a DELETE, which is why this cannot be another `Kind`.
async fn sweep_images<M: Fn(&str) -> bool>(c: &mut Ctx, owner: &str, jwt: &str, matches: &M) -> usize {
    let mut gone = 0;
    for name in images(c, owner, jwt, matches).await {
        let del = api(c, &format!("/api/{owner}/{name}/imagedelete"));
        match post(c, &del, jwt, Value::Null).await {
            Ok(_) => {
                gone += 1;
                tracing::info!(kind = "image", name = %name, "slo.teardown.deleted");
            }
            Err(e) => tracing::warn!(kind = "image", op = "delete", name = %name, error = %format!("{e:#}"), "slo.teardown.failed"),
        }
    }
    gone
}

/// The probe-owned image names `matches` claims. A listing that fails is an empty list, like every
/// other read in teardown.
async fn images<M: Fn(&str) -> bool + ?Sized>(c: &Ctx, owner: &str, jwt: &str, matches: &M) -> Vec<String> {
    let url = api(c, &format!("/api/{owner}/images"));
    let rows: Vec<serde_json::Value> = match get(c, &url, jwt).await {
        Ok(v) => serde_json::from_value(v).unwrap_or_default(),
        Err(e) => {
            tracing::warn!(kind = "image", op = "list", error = %format!("{e:#}"), "slo.teardown.failed");
            return vec![];
        }
    };
    rows.iter()
        .filter_map(|r| r.get("name").and_then(|v| v.as_str()).map(str::to_string))
        .filter(|n| matches(n))
        .collect()
}

/// `(name, id)` for every object of one kind under the probe's owner. A list that fails is an
/// empty list: teardown cannot fix an unreachable API, and the next run tries again.
async fn list(c: &Ctx, k: &Kind, owner: &str, jwt: &str) -> Vec<(String, String)> {
    let url = format!("{}{}?owner={owner}", c.cfg.api_url.trim_end_matches('/'), k.list);
    let rows: Vec<serde_json::Value> = match c
        .http
        .get(&url)
        .header("authorization", c.bearer(jwt))
        .timeout(Duration::from_secs(30))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
        Ok(r) => {
            tracing::warn!(kind = k.kind, op = "list", error = %r.status(), "slo.teardown.failed");
            return vec![];
        }
        Err(e) => {
            tracing::warn!(kind = k.kind, op = "list", error = %e, "slo.teardown.failed");
            return vec![];
        }
    };
    rows.iter()
        .filter_map(|v| {
            Some((v.get(k.name_field)?.as_str()?.to_string(), v.get(k.id_field)?.as_str()?.to_string()))
        })
        .collect()
}

/// `run-{suite}-{unix}-…` older than `STALE_SECS`. Both the suite AND the timestamp have to
/// parse: the sweep only ever deletes what it can positively identify as its own litter, and
/// `run-` is a prefix a person could plausibly give a repo of their own.
fn stale(name: &str, now: i64) -> bool {
    let Some(rest) = name.strip_prefix("run-") else { return false };
    let mut parts = rest.split('-');
    let (Some(suite), Some(ts)) = (parts.next(), parts.next()) else { return false };
    if Suite::parse(suite).is_none() {
        return false;
    }
    ts.parse::<i64>().map(|t| now - t > STALE_SECS).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_probe_object_past_the_deadline_is_stale() {
        let now = 10_000_000;
        assert!(stale("run-fast-1000-repo", now));
        assert!(stale("run-fast-1000", now));
        // Inside the window: another run may still be using it.
        assert!(!stale(&format!("run-fast-{}-repo", now - 60), now));
        // Not ours, and not shaped like ours.
        assert!(!stale("someones-repo", now));
        // A `run-` prefix somebody else chose: the suite segment is not one of ours.
        assert!(!stale("run-anything-1000-x", now));
        assert!(!stale("run-fast-notanumber-repo", now));
        assert!(!stale("run-fast", now));
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use crate::testkit;

    /// A URL is not a safe thing to put in a step detail. `/v1/cli/token?poll=…` carries a
    /// one-shot credential in its query string, and reqwest's own `Display` prints the whole URL —
    /// so a connection error is the one path where a secret reaches ClickHouse with nobody having
    /// formatted it there.
    #[tokio::test]
    async fn a_connection_error_never_carries_the_url() {
        let c = testkit::ctx().await;
        // Port 1, refused: the failure is reqwest's, which is the one that carries the URL.
        let url = format!("{}/v1/cli/token?poll=SECRETPOLLVALUE", c.cfg.api_url);
        let e = get(&c, &url, "").await.expect_err("nothing is listening");
        let detail = format!("{e:#}");
        assert!(!detail.contains("SECRETPOLLVALUE"), "the poll secret leaked: {detail}");
        assert!(!detail.contains("/v1/cli/token"), "the url leaked: {detail}");
    }
}
