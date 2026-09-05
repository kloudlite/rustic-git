//! Stage 14's four admin-side verbs: the quota request a person opens and a superadmin approves,
//! the console's own stop, the superadmin roster, and the activity feed.
//!
//! A sibling of `experience.rs` rather than more of it: the stage is filled in by several hands at
//! once, and one file per group of ids is what keeps them out of each other's way. Every function
//! here is one step and records exactly one id, whatever happens inside it.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;
use serde_json::{json, Value};

use super::{admin, api, call, get, poll_json, post, raw};
use crate::ctx::Ctx;
use crate::drill::{undoing, UNDO_SLACK};

/// Per-step ceilings, each looser than the catalogue target it measures (10 s for the create after
/// an approve, 30 s for the stop and the feed) so a slow fleet is a breach with a number rather
/// than a step the probe cut off. `admin.stop.workspace` carries a workspace CREATE inside it,
/// which is why it is the large one.
///
/// Two of them are a BODY cap and a step ceiling: `request.approve` and `superadmin.grant` both
/// compensate — a raised quota, a granted roster seat — and `Ctx::step`'s own timeout drops the
/// whole future, undo included, when it fires. So the body runs under its own ceiling inside
/// `drill::undoing` and the step's is that plus `UNDO_SLACK`, which can therefore never fire first.
const APPROVE_BODY: Duration = Duration::from_secs(90);
const APPROVE_CEILING: Duration = Duration::from_secs(APPROVE_BODY.as_secs() + UNDO_SLACK);
const STOP_CEILING: Duration = Duration::from_secs(150);
const GRANT_BODY: Duration = Duration::from_secs(20);
const GRANT_CEILING: Duration = Duration::from_secs(GRANT_BODY.as_secs() + UNDO_SLACK);
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

/// `deploy/k3s/quotas-slo.yaml`'s `slo-probe` spec, verbatim — the ONE place either half of the
/// probe writes the quota back from.
///
/// The step used to write back whatever it had read a moment earlier, which restores nothing when
/// the run it is repairing is the one that raised it. The yaml is what the owner decided; anything
/// else in the object is a leftover, and teardown writes this on every run whatever the step did.
pub(crate) fn probe_quota() -> Value {
    json!({
        "workspaces": 8,
        "environments": 3,
        "snapshots": 20,
        "diskGb": 40,
        "cpu": 40,
        "memoryGb": 80,
    })
}

/// `request.approve`: an over-quota create is refused, a request raises the limit, and the same
/// create then fits.
pub async fn request_approve(c: &mut Ctx) {
    let name = format!("{}-q", c.prefix());
    let reason = format!("{} slo probe quota", c.prefix());
    c.step("request.approve", APPROVE_CEILING, move |c| approve(c, APPROVE_BODY, name, reason).boxed())
        .await;
}

/// The step's whole body, with the body's ceiling as an argument so a test can watch the
/// compensation run when the body times out.
async fn approve(c: &Ctx, cap: Duration, name: String, reason: String) -> Result<()> {
    let probe = c.probe_user.clone();
    let jwt = c.probe_jwt.clone();
    let admin_jwt = c.admin_jwt.clone();
    let region = c.cfg.region.clone();
    let quota_url = api(c, "/v1/quota");
    let ws_url = api(c, "/v1/workspaces");
    let req_url = api(c, "/v1/requests");
    let write_back = admin(c, &format!("/admin/quota/{probe}"));
    // What the body made, for the undo — which cannot be handed the body's return value, because
    // it must also run on the path where the body was cut off and returned nothing at all.
    let made: Arc<Mutex<Option<String>>> = Arc::default();
    let created = made.clone();
    let body = async {
        let q = get(c, &quota_url, &jwt).await.context("could not read the quota")?;
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
        let (status, text) =
            raw(c, reqwest::Method::POST, &ws_url, &jwt, Some(create.clone()), &[]).await?;
        if status != reqwest::StatusCode::CONFLICT {
            // Recorded even here: a create that SUCCEEDED is a workspace the undo must take back.
            *created.lock().expect("lock") = id_of(&text);
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
        created_now(c, &ws_url, &jwt, &create, &created).await
    };
    // The compensation, OUTSIDE the cancellable region, in this order: the workspace holds the
    // disk the yaml limit does not allow, so it goes before the limit it would otherwise
    // contradict. A raised probe quota is the one leftover teardown's name sweep cannot see.
    let undo = || async {
        // Cloned out of the guard before the await: a `MutexGuard` held across one is a future
        // `Ctx::step` cannot box.
        let held = made.lock().expect("lock").clone();
        let dropped = match held {
            Some(id) => call(c, reqwest::Method::DELETE, &api(c, &format!("/v1/workspaces/{id}")), &jwt, None)
                .await
                .map(|_| ())
                .with_context(|| format!("the over-quota workspace {id} was left RUNNING")),
            None => Ok(()),
        };
        let body = json!({ "spec": probe_quota(), "note": "slo probe quota restore" });
        // The quota goes back whatever the delete did — a raised limit is allocation nobody
        // decided on, and it outlives the workspace by definition.
        call(c, reqwest::Method::PUT, &write_back, &admin_jwt, Some(body))
            .await
            .context("the probe's quota was left RAISED")?;
        dropped
    };
    undoing(cap, body, undo).await
}

/// The create the quota refused, retried until the approved grant reaches it. The id of whatever
/// it made lands in `made`, which is what the compensation takes back out.
async fn created_now(
    c: &Ctx,
    url: &str,
    jwt: &str,
    body: &Value,
    made: &Mutex<Option<String>>,
) -> Result<()> {
    let start = Instant::now();
    loop {
        let (status, text) =
            raw(c, reqwest::Method::POST, url, jwt, Some(body.clone()), &[]).await?;
        if status.is_success() {
            // Written down BEFORE the step can return: the undo takes it back, and a create the
            // probe forgot is one workspace of the owner's allocation held for good.
            *made.lock().expect("lock") = id_of(&text);
            return Ok(());
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
    let other_email = c.other_email.clone();
    c.step("superadmin.grant", GRANT_CEILING, |c| {
        let jwt = c.admin_jwt.clone();
        let one = admin(c, &format!("/api/admin/superadmins/{other_email}"));
        let all = admin(c, "/api/admin/superadmins");
        async move {
            let body = json!({ "note": NOTE });
            let granted = async {
                post(c, &one, &jwt, body.clone()).await.context("the grant was refused")?;
                match listed(c, &all, &jwt).await.context("could not read the roster after the grant")? {
                    true => Ok(()),
                    false => Err(anyhow!("the roster does not list the account the grant added")),
                }
            };
            // Outside the cancellable region: the probe must not leave a second superadmin
            // standing in the directory because its own step ran out of time.
            let revoke = || async {
                call(c, reqwest::Method::DELETE, &one, &jwt, Some(body.clone()))
                    .await
                    .map(|_| ())
                    .context("the account was left a SUPERADMIN")
            };
            undoing(GRANT_BODY, granted, revoke).await?;
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
    let other_email = c.other_email.clone();
    let v = get(c, url, jwt).await?;
    Ok(v.as_array().unwrap_or(&vec![]).iter().any(|r| {
        r.get("_id").and_then(Value::as_str).is_some_and(|u| u.eq_ignore_ascii_case(&other_email))
    }))
}

/// `feed.experience`: something that happened reaches the activity feed.
///
/// Its own throwaway repo rather than one an earlier stage made: the feed's `repo_created` half is
/// read from the listing markers, so any repo of this run's proves the same path, and a step that
/// depended on another stage's object would skip on every run that reordered them.
pub async fn feed(c: &mut Ctx) {
    let probe = c.probe_user.clone();
    let name = format!("{}-f", c.prefix());
    c.step("feed.experience", FEED_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let repos = api(c, "/v1/repos");
        let feed = api(c, &format!("/v1/activity?owner={probe}&limit=100"));
        async move {
            let body = json!({ "owner": probe, "name": name, "visibility": "private" });
            post(c, &repos, &jwt, body).await.context("could not create the repo")?;
            // Two seconds inside the step's own ceiling, so a feed that never carries the repo
            // reports what it last saw rather than the step's bare "timed out".
            poll_json(c, &feed, &jwt, FEED_CEILING - Duration::from_secs(2), |v| {
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

/// The `id` off a create's body, whatever the body is. `None` for anything that is not a JSON
/// object with one — a refusal, an HTML error page, an empty 204.
fn id_of(text: &str) -> Option<String> {
    serde_json::from_str::<Value>(text).ok()?.get("id").and_then(Value::as_str).map(str::to_string)
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
                    vec![json!({ "_id": crate::ctx::email_of(crate::ctx::OTHER_USER) })]
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

    /// The compensation runs when the BODY is cut off — the rule `drill::undoing` exists for, and
    /// the reason the quota write-back is not simply the last line of the step. A create that never
    /// answers used to mean a probe quota left RAISED for good: `Ctx::step`'s timeout drops the
    /// whole future, undo included.
    #[tokio::test]
    async fn the_quota_is_written_back_when_the_body_times_out() {
        let puts = Arc::new(AtomicUsize::new(0));
        let p = puts.clone();
        let app = Router::new()
            .route(
                "/v1/quota",
                get(|| async { Json(json!({"limit": {"diskGb": 100}, "used": {"diskGb": 10}})) }),
            )
            // Never answers: the body can only end at its ceiling.
            .route(
                "/v1/workspaces",
                post(|| async {
                    tokio::time::sleep(Duration::from_secs(600)).await;
                    Json(json!({}))
                }),
            )
            .route(
                "/admin/quota/{owner}",
                put(move |Json(b): Json<Value>| {
                    let p = p.clone();
                    async move {
                        // The yaml's values, not whatever the step happened to read.
                        assert_eq!(b["spec"], probe_quota());
                        p.fetch_add(1, Ordering::SeqCst);
                        Json(json!({}))
                    }
                }),
            );
        let mut c = crate::testkit::ctx().await;
        let url = crate::testkit::serve(app).await;
        c.cfg.api_url = url.clone();
        c.cfg.admin_url = url;

        let out = approve(&c, Duration::from_millis(50), "ws".into(), "why".into()).await;
        let detail = format!("{:#}", out.expect_err("the body cannot finish"));
        assert!(detail.contains("timed out"), "{detail}");
        assert_eq!(puts.load(Ordering::SeqCst), 1, "the quota was left RAISED");
    }

    /// And a workspace the undo cannot take back FAILS the step — after the write-back, which is
    /// the leftover that outlives it. A warning here would leave a probe workspace holding disk
    /// the yaml limit does not allow, with a green SLO on top of it.
    #[tokio::test]
    async fn a_workspace_the_undo_cannot_delete_fails_the_step() {
        let puts = Arc::new(AtomicUsize::new(0));
        let p = puts.clone();
        let app = Router::new()
            .route(
                "/v1/quota",
                get(|| async { Json(json!({"limit": {"diskGb": 100}, "used": {"diskGb": 10}})) }),
            )
            // A create that is NOT refused: the quota did not do its job, and the workspace it
            // made is the undo's problem.
            .route("/v1/workspaces", post(|| async { Json(json!({ "id": "ws-probe" })) }))
            .route(
                "/v1/workspaces/{id}",
                delete(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
            )
            .route(
                "/admin/quota/{owner}",
                put(move || {
                    let p = p.clone();
                    async move {
                        p.fetch_add(1, Ordering::SeqCst);
                        Json(json!({}))
                    }
                }),
            );
        let mut c = crate::testkit::ctx().await;
        let url = crate::testkit::serve(app).await;
        c.cfg.api_url = url.clone();
        c.cfg.admin_url = url;

        let out = approve(&c, Duration::from_secs(10), "ws".into(), "why".into()).await;
        let detail = format!("{:#}", out.expect_err("the create was not refused"));
        assert!(detail.contains("an over-quota create answered 200"), "{detail}");
        assert_eq!(puts.load(Ordering::SeqCst), 1, "the quota was left RAISED");
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

#[cfg(test)]
mod quota_yaml {
    /// `probe_quota` is a hand copy of the PRIMARY tenant's object in deploy/k3s/quotas-slo.yaml;
    /// an edit to one without the other makes teardown quietly restore the wrong limits. Checked
    /// for all three primary users — one pair per suite (ctx::SUITE_TENANTS), and teardown restores
    /// whichever one the run owns, so a pair added with different limits is the same bug.
    #[test]
    fn every_primary_quota_matches_the_yaml_applied_on_the_region() {
        let yaml = include_str!("../../../../deploy/k3s/quotas-slo.yaml");
        let want = super::probe_quota();
        let mut seen = 0;
        for doc in yaml.split("\n---\n").map(|d| format!("{}\n", d.trim_end())) {
            if !crate::ctx::SUITE_TENANTS.iter().any(|(p, _)| doc.contains(&format!("name: {p}\n"))) {
                continue;
            }
            seen += 1;
            for (k, v) in want.as_object().unwrap() {
                let line = format!("\n  {k}: {v}\n");
                assert!(doc.contains(&line), "a quotas-slo.yaml primary object lacks `{}`", line.trim());
            }
        }
        assert_eq!(seen, crate::ctx::SUITE_TENANTS.len(), "one primary Quota object per suite");
    }
}
