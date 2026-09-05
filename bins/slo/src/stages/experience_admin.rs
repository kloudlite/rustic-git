//! Stage 14's four admin-side verbs: the quota request a person opens and a superadmin approves,
//! the console's own stop, the superadmin roster, and the activity feed.
//!
//! A sibling of `experience.rs` rather than more of it: the stage is filled in by several hands at
//! once, and one file per group of ids is what keeps them out of each other's way. Every function
//! here is one step and records exactly one id, whatever happens inside it.

use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;
use serde_json::{json, Value};

use super::{admin, api, call, get, poll_json, post, raw};
use crate::ctx::{Ctx, OTHER_EMAIL, PROBE_USER};

/// Per-step ceilings, each looser than the catalogue target it measures (10 s for the create after
/// an approve, 30 s for the stop and the feed) so a slow fleet is a breach with a number rather
/// than a step the probe cut off. `admin.stop.workspace` carries a workspace CREATE inside it,
/// which is why it is the large one.
const APPROVE_CEILING: Duration = Duration::from_secs(90);
const STOP_CEILING: Duration = Duration::from_secs(150);
const GRANT_CEILING: Duration = Duration::from_secs(20);
const FEED_CEILING: Duration = Duration::from_secs(45);

/// How long the once-refused create is retried after the approve — the catalogue's target for the
/// grant taking effect.
const AFTER_APPROVE: Duration = Duration::from_secs(10);

/// The gigabytes the request asks for on top of the current limit, and the amount by which the
/// first create overshoots it. The same number on purpose: after the raise the create fits
/// EXACTLY (`quota::check` refuses on `>`, not `>=`), so a probe that passes proves the grant
/// landed rather than that some other headroom appeared.
const HEADROOM: u64 = 5;

/// Every admin write on this platform carries a note onto its audit row, and an empty one is a 422.
const NOTE: &str = "slo probe";

pub async fn request_approve(c: &mut Ctx) {
    let name = format!("{}-q", c.prefix());
    let reason = format!("{} slo probe quota", c.prefix());
    c.step("request.approve", APPROVE_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let admin_jwt = c.admin_jwt.clone();
        let region = c.cfg.region.clone();
        let quota_url = api(c, "/v1/quota");
        let ws_url = api(c, "/v1/workspaces");
        let req_url = api(c, "/v1/requests");
        let write_back = admin(c, &format!("/admin/quota/{PROBE_USER}"));
        async move {
            let q = get(c, &quota_url, &jwt).await.context("could not read the quota")?;
            // Kept whole rather than field by field: it is written back verbatim below, and a
            // restore that rebuilt the spec from the dimensions this step cares about would erase
            // every other limit somebody granted.
            let limit =
                q.get("limit").cloned().ok_or_else(|| anyhow!("the quota answer carried no limit"))?;
            let cap = disk_gb(&limit).ok_or_else(|| anyhow!("the quota limit carries no diskGb"))?;
            let used = q.get("used").and_then(disk_gb).unwrap_or(0);
            let create = json!({
                "name": name,
                "region": region,
                "quota_gb": cap.saturating_sub(used) + HEADROOM,
                "packages": [],
            });
            let attempt = async {
                let (status, text) =
                    raw(c, reqwest::Method::POST, &ws_url, &jwt, Some(create.clone()), &[]).await?;
                if status != reqwest::StatusCode::CONFLICT {
                    return Err(anyhow!("an over-quota create answered {status}: {}", clip(&text)));
                }
                let body = json!({
                    "kind": "quota",
                    "reason": reason,
                    "quota": { "diskGb": cap + HEADROOM },
                });
                let made =
                    post(c, &req_url, &jwt, body).await.context("could not open the quota request")?;
                let id = made
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("the answer carried no request id"))?
                    .to_string();
                let approve = admin(c, &format!("/admin/requests/{id}/approve"));
                post(c, &approve, &admin_jwt, json!({ "note": NOTE }))
                    .await
                    .context("could not approve the quota request")?;
                created_now(c, &ws_url, &jwt, &create).await
            };
            let out = attempt.await;
            // The finally path, in this order: the workspace holds the disk the ORIGINAL limit
            // does not allow, so it goes before the limit it would otherwise contradict. Both run
            // whatever happened above — a raised probe quota is the one leftover teardown's name
            // sweep cannot see.
            if let Ok(Some(id)) = &out {
                let url = api(c, &format!("/v1/workspaces/{id}"));
                if let Err(e) = call(c, reqwest::Method::DELETE, &url, &jwt, None).await {
                    tracing::warn!(kind = "workspace", op = "delete", error = %format!("{e:#}"), "slo.experience.failed");
                }
            }
            let body = json!({ "spec": limit, "note": "slo probe quota restore" });
            let back = call(c, reqwest::Method::PUT, &write_back, &admin_jwt, Some(body)).await;
            out?;
            back.context("the probe's quota was left RAISED")?;
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// The create the quota refused, retried until the approved grant reaches it. Answers the new
/// workspace's id when one was made, so the caller can take it back out.
async fn created_now(c: &Ctx, url: &str, jwt: &str, body: &Value) -> Result<Option<String>> {
    let start = Instant::now();
    loop {
        let (status, text) =
            raw(c, reqwest::Method::POST, url, jwt, Some(body.clone()), &[]).await?;
        if status.is_success() {
            return Ok(serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("id").and_then(Value::as_str).map(str::to_string)));
        }
        if start.elapsed() >= AFTER_APPROVE {
            return Err(anyhow!(
                "the create still answered {status} {} ms after the approve: {}",
                AFTER_APPROVE.as_millis(),
                clip(&text)
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// `admin.stop.workspace`: the console stops somebody else's workspace, and the OWNER's own read
/// is what says it happened — an admin route that only satisfies itself proves nothing.
pub async fn admin_stop(c: &mut Ctx) {
    let name = format!("{}-a", c.prefix());
    c.step("admin.stop.workspace", STOP_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let admin_jwt = c.admin_jwt.clone();
        let region = c.cfg.region.clone();
        let ws_url = api(c, "/v1/workspaces");
        async move {
            let body = json!({ "name": name, "region": region, "quota_gb": 1, "packages": [] });
            let made = post(c, &ws_url, &jwt, body).await.context("could not create a workspace to stop")?;
            let id = made
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("the answer carried no workspace id"))?
                .to_string();
            // Left for teardown's prefix sweep rather than deleted here: the step is about the
            // stop, and a delete inside it would turn a slow delete into a stop breach.
            let ws = api(c, &format!("/v1/workspaces/{id}"));
            poll_json(c, &ws, &jwt, Duration::from_secs(90), |v| state_is(v, "ready"))
                .await
                .context("the workspace never became ready")?;
            let stop = admin(c, &format!("/admin/workspaces/{id}/stop"));
            post(c, &stop, &admin_jwt, json!({ "note": NOTE }))
                .await
                .context("the admin stop was refused")?;
            poll_json(c, &ws, &jwt, Duration::from_secs(30), |v| state_is(v, "stopped"))
                .await
                .context("the owner's own read never showed it stopped")
        }
        .boxed()
    })
    .await;
}

/// `superadmin.grant`: the roster the directory keeps, added to and taken back.
///
/// The LISTING is the assertion, not a minted claim: a JWT with `superadmin: true` is minted from
/// a secret this probe holds, so it would pass before the grant as well as after and prove nothing
/// about the directory. `refuse_without_claim` is covered by `sec.admin.claim` instead.
pub async fn superadmin_grant(c: &mut Ctx) {
    c.step("superadmin.grant", GRANT_CEILING, |c| {
        let jwt = c.admin_jwt.clone();
        let one = admin(c, &format!("/api/admin/superadmins/{OTHER_EMAIL}"));
        let all = admin(c, "/api/admin/superadmins");
        async move {
            let body = json!({ "note": NOTE });
            post(c, &one, &jwt, body.clone()).await.context("the grant was refused")?;
            let added = listed(c, &all, &jwt).await;
            // The revoke runs whatever the read said: the probe must not leave a second
            // superadmin standing in the directory.
            let removed = call(c, reqwest::Method::DELETE, &one, &jwt, Some(body)).await;
            if !added.context("could not read the roster after the grant")? {
                return Err(anyhow!("the roster does not list the account the grant added"));
            }
            removed.context("the account was left a SUPERADMIN")?;
            match listed(c, &all, &jwt).await.context("could not read the roster after the revoke")? {
                true => Err(anyhow!("the roster still lists the account after the revoke")),
                false => Ok(()),
            }
        }
        .boxed()
    })
    .await;
}

/// Whether the second tenant is on the roster. Case-insensitive: the roster's `_id` is an email
/// address, and nothing normalises the casing on the way in.
async fn listed(c: &Ctx, url: &str, jwt: &str) -> Result<bool> {
    let v = get(c, url, jwt).await?;
    Ok(v.as_array().unwrap_or(&vec![]).iter().any(|r| {
        r.get("_id").and_then(Value::as_str).is_some_and(|u| u.eq_ignore_ascii_case(OTHER_EMAIL))
    }))
}

/// `feed.experience`: something that happened reaches the activity feed.
///
/// Its own throwaway repo rather than one an earlier stage made: the feed's `repo_created` half is
/// read from the listing markers, so any repo of this run's proves the same path, and a step that
/// depended on another stage's object would skip on every run that reordered them.
pub async fn feed(c: &mut Ctx) {
    let name = format!("{}-f", c.prefix());
    c.step("feed.experience", FEED_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let repos = api(c, "/v1/repos");
        let feed = api(c, &format!("/v1/activity?owner={PROBE_USER}&limit=100"));
        async move {
            let body = json!({ "owner": PROBE_USER, "name": name, "visibility": "private" });
            post(c, &repos, &jwt, body).await.context("could not create the repo")?;
            poll_json(c, &feed, &jwt, FEED_CEILING, |v| {
                v.as_array().unwrap_or(&vec![]).iter().any(|e| {
                    e.get("kind").and_then(Value::as_str) == Some("repo_created")
                        && e.get("repo").and_then(Value::as_str) == Some(name.as_str())
                })
            })
            .await
            .context("the new repo never reached the activity feed")
        }
        .boxed()
    })
    .await;
}

fn disk_gb(v: &Value) -> Option<u64> {
    v.get("diskGb").and_then(Value::as_u64)
}

fn state_is(v: &Value, want: &str) -> bool {
    v.get("state").and_then(Value::as_str) == Some(want)
}

/// A response body carried into a step detail. Long enough to name the refusal, short enough that
/// an HTML error page does not become the report.
fn clip(text: &str) -> String {
    text.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    use axum::extract::Path;
    use axum::routing::{delete, get, post, put};
    use axum::{Json, Router};

    use super::*;

    /// Everything the four steps touch, in the order they touch it. One router for both processes:
    /// the paths do not collide, and a stub per tier would only prove the test can wire two.
    #[derive(Default)]
    struct Fleet {
        creates: AtomicUsize,
        stopped: AtomicBool,
        granted: AtomicBool,
    }

    fn app(state: Arc<Fleet>, repo: String) -> Router {
        let s = state.clone();
        let creates = move || {
            let s = s.clone();
            async move {
                // The first create is the over-quota one the raise is asked for; every later one
                // is a workspace that fits.
                if s.creates.fetch_add(1, Ordering::SeqCst) == 0 {
                    return (axum::http::StatusCode::CONFLICT, "diskGb: 1 of 1 in use").into_response();
                }
                Json(json!({ "id": "ws-probe" })).into_response()
            }
        };
        let s = state.clone();
        let ws = move || {
            let s = s.clone();
            async move {
                let state = if s.stopped.load(Ordering::SeqCst) { "stopped" } else { "ready" };
                Json(json!({ "id": "ws-probe", "state": state }))
            }
        };
        let s = state.clone();
        let stop = move || {
            let s = s.clone();
            async move {
                s.stopped.store(true, Ordering::SeqCst);
                Json(json!({}))
            }
        };
        let s = state.clone();
        let roster = move || {
            let s = s.clone();
            async move {
                let rows = if s.granted.load(Ordering::SeqCst) {
                    vec![json!({ "_id": OTHER_EMAIL })]
                } else {
                    vec![]
                };
                Json(rows)
            }
        };
        let s = state.clone();
        let grant = move |_: Path<String>| {
            let s = s.clone();
            async move {
                s.granted.store(true, Ordering::SeqCst);
                Json(json!({}))
            }
        };
        let s = state;
        let revoke = move |_: Path<String>| {
            let s = s.clone();
            async move {
                s.granted.store(false, Ordering::SeqCst);
                Json(json!({}))
            }
        };
        use axum::response::IntoResponse;
        Router::new()
            .route("/v1/quota", get(|| async { Json(json!({"limit": {"diskGb": 100}, "used": {"diskGb": 10}})) }))
            .route("/v1/workspaces", post(creates))
            .route("/v1/workspaces/{id}", get(ws).delete(|_: Path<String>| async { Json(json!({})) }))
            .route("/v1/requests", post(|| async { Json(json!({ "id": "req-1" })) }))
            .route("/admin/requests/{id}/approve", post(|_: Path<String>| async { Json(json!({})) }))
            .route("/admin/quota/{owner}", put(|_: Path<String>| async { Json(json!({})) }))
            .route("/admin/workspaces/{id}/stop", post(stop))
            .route("/api/admin/superadmins", get(roster))
            .route("/api/admin/superadmins/{user}", post(grant).delete(revoke))
            .route("/v1/repos", post(|| async { Json(json!({})) }))
            .route(
                "/v1/activity",
                get(move || {
                    let repo = repo.clone();
                    async move { Json(json!([{ "kind": "repo_created", "repo": repo }])) }
                }),
            )
            .route("/{*rest}", delete(|| async { Json(json!({})) }))
    }

    async fn run(c: &mut Ctx) {
        request_approve(c).await;
        admin_stop(c).await;
        superadmin_grant(c).await;
        feed(c).await;
    }

    const IDS: [&str; 4] =
        ["request.approve", "admin.stop.workspace", "superadmin.grant", "feed.experience"];

    /// A fleet that answers: every id is recorded once, and passes.
    #[tokio::test]
    async fn every_id_is_emitted_once_against_a_fleet_that_answers() {
        let mut c = crate::testkit::ctx().await;
        let url = crate::testkit::serve(app(Arc::default(), format!("{}-f", c.prefix()))).await;
        c.cfg.api_url = url.clone();
        c.cfg.admin_url = url;
        run(&mut c).await;
        let ids: Vec<&str> = c.steps.iter().map(|s| s.slo_id.as_str()).collect();
        assert_eq!(ids, IDS);
        for s in &c.steps {
            assert!(s.ok, "{}: {}", s.slo_id, s.detail);
        }
    }

    /// Nothing reachable: still exactly one sample per id, each a failure with a reason rather
    /// than a skip — a precondition that never held is what this stage measures.
    #[tokio::test]
    async fn every_id_is_emitted_once_with_nothing_reachable() {
        let mut c = crate::testkit::ctx().await;
        run(&mut c).await;
        let ids: Vec<&str> = c.steps.iter().map(|s| s.slo_id.as_str()).collect();
        assert_eq!(ids, IDS);
        for s in &c.steps {
            assert!(!s.ok && !s.skipped, "{}: {}", s.slo_id, s.detail);
        }
    }
}
