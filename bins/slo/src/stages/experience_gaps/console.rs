//! Experience probes on the admin console: listings, stops and deletes as a superadmin, the ten
//! screens, workloads, audit export, request kinds, the legacy union, region status.

use super::*;


/// `vol.list`: the listing names the volumes this run is holding.
///
/// `vol.history` reads one volume's chain; nothing read the LIST, which is what the console's own
/// pages are drawn from and the one place a volume with no working copy is visible at all. Matched
/// on `display_name`, because a volume's `name` is the ws/env id and carries no run prefix —
/// exactly the trap that made teardown's sweep miss every probe volume.
pub(crate) async fn vol_list(c: &mut Ctx) {
    let prefix = c.prefix();
    c.step("vol.list", READ_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, "/v1/volumes");
        async move {
            let rows = get(c, &url, &jwt).await.context("could not list the volumes")?;
            let rows = rows.as_array().ok_or_else(|| anyhow!("the volume list is not a list"))?;
            let mine = rows
                .iter()
                .filter(|r| {
                    r.get("display_name").and_then(Value::as_str).is_some_and(|n| n.starts_with(&prefix))
                })
                .count();
            if mine == 0 {
                return Err(anyhow!("the listing names none of the {} volumes this run holds", rows.len()));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

// ── admin ───────────────────────────────────────────────────────────────────


/// `admin.stop.environment`: `admin.stop.workspace`'s twin.
///
/// The OWNER's own read is what says it happened, exactly as the workspace one does: an admin
/// route that only satisfies itself proves nothing about what the person whose environment it was
/// can see.
pub(crate) async fn admin_stop_environment(c: &mut Ctx) {
    let Some(env) = c.state.env_multi.clone() else {
        return c.skip("admin.stop.environment", "no environment to stop");
    };
    c.step("admin.stop.environment", ADMIN_ENV_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let admin_jwt = c.admin_jwt();
        let one = api(c, &format!("/v1/environments/{env}"));
        let stop = admin(c, &format!("/admin/environments/{env}/stop"));
        async move {
            // Running first: stopping something already stopped answers 2xx and measures nothing.
            poll_json(c, &one, &jwt, Duration::from_secs(120), |v| {
                v.get("state").and_then(Value::as_str) == Some("running")
            })
            .await
            .context("the environment was not running to begin with")?;
            post(c, &stop, &admin_jwt, json!({ "note": NOTE })).await.context("the admin stop was refused")?;
            poll_json(c, &one, &jwt, Duration::from_secs(45), |v| {
                v.get("state").and_then(Value::as_str) == Some("stopped")
            })
            .await
            .context("the owner's own read never showed it stopped")
        }
        .boxed()
    })
    .await;
}


/// `admin.delete.workload`: the console's own deletes, on objects of the probe's own making.
///
/// Both kinds in one step: the two admin handlers are separate code paths over the same finalizer,
/// and either one silently doing nothing is the same failure — an operator who thinks they have
/// taken something away and has not.
pub(crate) async fn admin_delete(c: &mut Ctx) {
    let name = format!("{}-ad", c.prefix());
    c.step("admin.delete.workload", ADMIN_DELETE_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let admin_jwt = c.admin_jwt();
        let region = c.cfg.region.clone();
        let workspaces = api(c, "/v1/workspaces");
        let environments = api(c, "/v1/environments");
        async move {
            let ws = json!({ "name": format!("{name}-w"), "region": region, "quota_gb": QUOTA_GB, "packages": [] });
            let ws = id_of(&post(c, &workspaces, &jwt, ws).await.context("could not create a workspace to delete")?)?;
            let env = json!({
                "name": format!("{name}-e"),
                "region": region,
                "quota_gb": QUOTA_GB,
                "services": [{ "name": "redis", "image": "redis:7-alpine", "command": [], "env": {}, "mounts": [], "ports": [6379] }],
            });
            let env = id_of(&post(c, &environments, &jwt, env).await.context("could not create an environment to delete")?)?;
            // No wait for `ready`: the delete is the SLI and a create that is still converging is
            // deleted the same way. Both are named `run-…`, so teardown finds either one anyway.
            for (kind, id, path) in [
                ("workspace", ws, "workspaces"),
                ("environment", env, "environments"),
            ] {
                let del = admin(c, &format!("/admin/{path}/{id}"));
                call(c, reqwest::Method::DELETE, &del, &admin_jwt, Some(json!({ "note": NOTE })))
                    .await
                    .with_context(|| format!("the admin delete of the {kind} was refused"))?;
                // The OWNER's read, like the stop above: the admin route agreeing with itself is
                // not the same as the object being gone.
                let one = api(c, &format!("/v1/{path}/{id}"));
                let start = std::time::Instant::now();
                loop {
                    let (status, _) = raw(c, reqwest::Method::GET, &one, &jwt, None, &[]).await?;
                    if status == reqwest::StatusCode::NOT_FOUND {
                        break;
                    }
                    if start.elapsed() >= Duration::from_secs(60) {
                        return Err(anyhow!("the {kind} still answers {status} to its owner"));
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}


/// `admin.screens`: the three console reads nothing else covers.
///
/// One step over three routes, because they are one claim — "the console renders" — and each is a
/// different module's own reader (`admin/owners.rs`, `admin/clusters.rs`, `admin/overview.rs`).
/// The overview is composed from every other module's reader, so it is the one that goes red first
/// when any of them stops answering.
pub(crate) async fn screens(c: &mut Ctx) {
    let probe = c.probe_user.clone();
    c.step("admin.screens", READ_CEILING, move |c| {
        let jwt = c.admin_jwt();
        let owners = admin(c, "/admin/owners");
        let owner = admin(c, &format!("/admin/owners/{probe}"));
        let clusters = admin(c, "/admin/clusters");
        let overview = admin(c, "/admin/overview");
        async move {
            // The probe's own owner row, not merely a non-empty list: a listing that answered `[]`
            // is a console screen with nothing on it, and a 200 all the same.
            let rows = get(c, &owners, &jwt).await.context("the owners screen")?;
            let listed = rows.as_array().is_some_and(|rows| {
                rows.iter().any(|r| {
                    [r.get("slug"), r.get("owner"), r.get("name")]
                        .into_iter()
                        .flatten()
                        .any(|v| v.as_str() == Some(probe.as_str()))
                })
            });
            if !listed {
                return Err(anyhow!("the owners screen does not list {probe}"));
            }
            get(c, &owner, &jwt).await.context("the owner detail screen")?;
            get(c, &clusters, &jwt).await.context("the clusters screen")?;
            get(c, &overview, &jwt).await.context("the overview screen")?;
            Ok(())
        }
        .boxed()
    })
    .await;
}


/// `admin.workloads.read`: the roll-target list the infrastructure tab and every `Mark::Boot` save
/// are drawn from. A save that would roll a reader it cannot see is a save that never lands.
pub(crate) async fn workloads(c: &mut Ctx) {
    c.step("admin.workloads.read", READ_CEILING, |c| {
        let jwt = c.admin_jwt();
        let url = admin(c, "/admin/workloads");
        async move {
            let rows = get(c, &url, &jwt).await.context("could not read the workloads")?;
            let rows = rows.get("workloads").and_then(Value::as_array).or_else(|| rows.as_array()).cloned();
            let rows = match rows {
                Some(rows) if !rows.is_empty() => rows,
                // Empty is the failure, not a quiet fleet: `KNOWN` is compiled in, so a list with
                // nothing on it means the reader could not see the cluster at all.
                Some(_) => return Err(anyhow!("the workloads list names no roll target at all")),
                None => return Err(anyhow!("the answer carried no workloads")),
            };
            // Non-empty was never the thing that mattered: a `Mark::Boot` save prechecks EVERY
            // reader, so a list missing one names a workload nothing will ever wait for — and the
            // save would go out ahead of the pods that read it. So the whole `KNOWN` list, with
            // `ready` and `desired` on each, which is what the precheck reads.
            let named: Vec<&str> = rows.iter().filter_map(|r| r.get("name").and_then(Value::as_str)).collect();
            let missing: Vec<&str> = kloudlite_workspaces::api::workloads::KNOWN_CENTRAL
                .iter()
                .chain(kloudlite_workspaces::api::workloads::KNOWN_PER_REGION)
                .map(|(name, _)| *name)
                .filter(|name| !named.contains(name))
                .collect();
            if !missing.is_empty() {
                return Err(anyhow!("the workloads list does not name {}", missing.join(", ")));
            }
            if let Some(bad) = rows.iter().find(|r| r.get("ready").is_none() || r.get("desired").is_none()) {
                return Err(anyhow!(
                    "`{}` is listed without ready/desired, which is what a Boot save prechecks",
                    bad.get("name").and_then(Value::as_str).unwrap_or("a workload")
                ));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}


/// `audit.export`: the CSV a person downloads off the Audit screen.
///
/// A header AND a row: the export is the object-store log rendered, and an export that answers a
/// header with nothing under it is what a broken reader produces — the fast suite's `audit.row`
/// has already filed at least one row by the time this runs.
pub(crate) async fn audit_export(c: &mut Ctx) {
    c.step("audit.export", READ_CEILING, |c| {
        let jwt = c.admin_jwt();
        let url = admin(c, "/admin/audit.csv?limit=50");
        async move {
            let (status, body) = raw(c, reqwest::Method::GET, &url, &jwt, None, &[]).await?;
            if !status.is_success() {
                return Err(anyhow!("{status}: {}", clip(&body)));
            }
            let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
            match lines.as_slice() {
                [] => Err(anyhow!("the export is empty — not even a header")),
                [_] => Err(anyhow!("the export carries a header and no rows")),
                [head, ..] if head.contains(',') => Ok(()),
                _ => Err(anyhow!("the export's first line is not a CSV header")),
            }
        }
        .boxed()
    })
    .await;
}


/// `req.decide.kinds`: an ACCESS approval grants what it says, and a deny closes with its reason.
///
/// Only the quota kind was probed, and approve is kind-specific: access grants team membership
/// through the directory the admin process already holds, and each kind writes its effect BEFORE
/// marking the request. The membership read is what says the effect landed — a request marked
/// `approved` with no grant behind it is the failure that kind exists to avoid.
///
/// Its own team, made and taken back inside the step: see the module header.
pub(crate) async fn decide_kinds(c: &mut Ctx) {
    let slug = format!("{}-rq", c.prefix());
    c.step("req.decide.kinds", DECIDE_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let other = c.other_jwt.clone();
        let admin_jwt = c.admin_jwt();
        let team = api(c, &format!("/v1/teams/{slug}"));
        async move {
            post(c, &api(c, "/v1/teams"), &jwt, json!({ "slug": slug, "name": "slo probe requests" }))
                .await
                .context("could not create the team the access request is for")?;
            let drop_team = || async {
                call(c, reqwest::Method::DELETE, &team, &jwt, None)
                    .await
                    .map(|_| ())
                    .context("the request team was left standing")
            };
            let both = async {
                // Approve: the SECOND tenant asks to join, and its own read of the team is what
                // says the grant landed — `/v1/teams/{slug}` answers 404 to a non-member, which is
                // exactly the question.
                let ask = json!({
                    "kind": "access",
                    "reason": format!("{} slo probe access", c.prefix()),
                    "access": { "team": slug, "role": "member" },
                });
                let made = post(c, &api(c, "/v1/requests"), &other, ask).await.context("could not open the access request")?;
                let id = id_of(&made)?;
                post(c, &admin(c, &format!("/admin/requests/{id}/approve")), &admin_jwt, json!({ "note": NOTE }))
                    .await
                    .context("the access approval was refused")?;
                poll_json(c, &team, &other, Duration::from_secs(20), |v| {
                    v.get("slug").and_then(Value::as_str) == Some(slug.as_str())
                })
                .await
                .context("the approved access request granted no membership")?;
                // Deny: a second request, of a different kind so the one-pending-per-kind rule
                // does not refuse it, closed with the reason the asker reads.
                let ask = json!({
                    "kind": "other",
                    "reason": format!("{} slo probe deny", c.prefix()),
                    "other": { "title": "slo probe", "body": "deny me" },
                });
                let made = post(c, &api(c, "/v1/requests"), &other, ask).await.context("could not open the request to deny")?;
                let id = id_of(&made)?;
                let note = format!("slo probe denied {}", c.run_id);
                post(c, &admin(c, &format!("/admin/requests/{id}/deny")), &admin_jwt, json!({ "note": note }))
                    .await
                    .context("the deny was refused")?;
                let seen = get(c, &api(c, &format!("/v1/requests/{id}")), &other).await.context("the asker cannot read it back")?;
                denied_with(&seen, &note)
            };
            undoing(DECIDE_CEILING - Duration::from_secs(20), both, drop_team).await
        }
        .boxed()
    })
    .await;
}


/// A denied request reads back as denied AND carries the reason. A state with no note is a
/// decision the asker cannot act on, which is the half a status check would miss.
pub(crate) fn denied_with(request: &Value, note: &str) -> Result<()> {
    let state = request.get("state").and_then(Value::as_str).unwrap_or_default();
    if state != "denied" {
        return Err(anyhow!("the request reads back as `{state}`, not denied"));
    }
    let carried = request.to_string();
    if !carried.contains(note) {
        return Err(anyhow!("the denied request carries no reason the asker can read"));
    }
    Ok(())
}


/// `req.legacy.union`: the retired `QuotaRequest` queue is still readable and still migrates.
///
/// `QuotaRequest` is deliberately kept alive — unioned into `GET /admin/requests`, copied over once
/// by `POST /admin/requests/migrate` — until it is deleted as a CRD in a later release. Both are
/// asked, because the union going quiet and the migration failing are the same outcome for whoever
/// filed one: a request nobody will ever see.
pub(crate) async fn legacy_union(c: &mut Ctx) {
    c.step("req.legacy.union", READ_CEILING, |c| {
        let jwt = c.admin_jwt();
        let legacy = admin(c, "/admin/quota-requests");
        let migrate = admin(c, "/admin/requests/migrate");
        let queue = admin(c, "/admin/requests");
        async move {
            // Reading the retired queue must ANSWER; an empty list is the ordinary state and is
            // not a failure — the CRD may legitimately hold nothing left to migrate.
            let rows = get(c, &legacy, &jwt).await.context("the retired quota-request queue")?;
            if !rows.is_array() {
                return Err(anyhow!("the retired queue did not answer a list"));
            }
            post(c, &migrate, &jwt, json!({ "note": NOTE })).await.context("the migration was refused")?;
            let unioned = get(c, &queue, &jwt).await.context("the admin queue")?;
            unioned
                .as_array()
                .map(|_| ())
                .ok_or_else(|| anyhow!("the admin queue did not answer a list after the migration"))
        }
        .boxed()
    })
    .await;
}


/// `region.status`: the region list a create offers, and this run's own cluster's status.
///
/// The CREATE is deliberately not here: a `Region` has no delete on any tier — a second POST only
/// retires or renames one — so a probe region would be shared state nobody could ever take back.
/// What every run does need is that the region it is running in is listed and answers, which is
/// the read every workspace create is validated against.
pub(crate) async fn region_status(c: &mut Ctx) {
    let region = c.cfg.region.clone();
    c.step("region.status", READ_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let admin_jwt = c.admin_jwt();
        let list = api(c, "/v1/regions");
        let detail = admin(c, &format!("/admin/clusters/{region}"));
        async move {
            let rows = get(c, &list, &jwt).await.context("could not list the regions")?;
            let there = rows.as_array().is_some_and(|rows| {
                rows.iter().any(|r| {
                    [r.get("id"), r.get("name")]
                        .into_iter()
                        .flatten()
                        .any(|v| v.as_str() == Some(region.as_str()))
                })
            });
            if !there {
                return Err(anyhow!("`{region}` — the region this run is in — is not listed"));
            }
            get(c, &detail, &admin_jwt).await.context("the cluster's own status").map(|_| ())
        }
        .boxed()
    })
    .await;
}

// ── shared ──────────────────────────────────────────────────────────────────
