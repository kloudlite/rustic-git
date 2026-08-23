//! Pull requests, served by the node that owns the repo.
//!
//! A change belongs to its repo, so it lives in that repo's own database — which means exactly
//! one node may read or write it, and that node is this one. Everything here goes through
//! `crate::pulls` against `db_for(owner, name)`; nothing reaches a central directory.
//!
//! Two authorization shapes, deliberately, and they are the ones already in use beside this file:
//!
//! - **Reads** (`list`, `get`) use `open_ro`, exactly as `repo.rs` does. The peer secret alone is
//!   not enough for a read: the api tier forwards on behalf of a person and names them in
//!   `OWNER_HEADER`, so a private repo's changes must be as invisible here as its refs are.
//! - **Writes** mirror `admin.rs`'s `api_visibility`/`api_description`: the peer secret alone.
//!   Whether the human may open, comment on, merge or close is the api tier's question — only it
//!   knows about users and teams — and it answers it (`settings_caller`/`may_act_under`) before
//!   forwarding. This router is unreachable from the public listener.
//!
//! Every handler calls `ensure_migrated` FIRST, before it reads or writes anything: a repo whose
//! changes predate this move must show them on its first touch, not silently appear empty.
//!
//! Every write publishes its event AFTER the database write, never before and never with `?` —
//! `events::publish` is fire-and-forget, and a Redis outage must cost a consumer one fallback
//! poll, never a user's operation.
use super::super::{internal, Trusted};
use super::{hidden, open_ro};
use crate::pulls::{self, Comment, MergeJob, PullRequest, PullState};
use crate::App;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use std::collections::HashMap;
use std::sync::Arc;

fn now_ms() -> i64 {
    crate::ownership::now_ms() as i64
}

/// The repo's own database, migrated. `Err` is the response to return as-is.
///
/// The directory state is whatever this node was started with. Configured-but-unreachable fails
/// here rather than migrating an empty repo — see `pulls::Source`.
async fn ready(app: &App, owner: &str, name: &str) -> Result<Arc<slatedb::Db>, Response> {
    if let Err(e) = pulls::ensure_migrated(&app.store, &app.dir, owner, name).await {
        eprintln!("migrate pulls {owner}/{name}: {e}"); // ponytail: eprintln
        return Err(internal(e));
    }
    app.store.db_for(owner, name).await.map_err(internal)
}

/// A write route's repo, validated and confirmed to exist.
///
/// Asked of the object store, not the pool, for the same reason `api_description` asks: `db_for`
/// CREATES a database for whatever name it is handed, so an unguarded write would conjure one per
/// mistyped path.
async fn writable(app: &App, owner: &str, name: &str) -> Result<(String, String), Response> {
    let Some((owner, name)) = crate::protocol::parse_repo_path(&format!("{owner}/{name}")) else {
        return Err((StatusCode::BAD_REQUEST, "invalid repository path").into_response());
    };
    if !app.store.repo_exists(&owner, &name).await.unwrap_or(false) {
        return Err(hidden());
    }
    Ok((owner, name))
}

async fn emit(app: &App, kind: crate::events::Kind, pr: &PullRequest, actor: &str) {
    crate::events::publish(
        &app.store.cache,
        &crate::events::Event {
            kind,
            repo: pr.repo.clone(),
            number: pr.number,
            actor: actor.to_string(),
            at_ms: now_ms(),
            title: pr.title.clone(),
            base: pr.base.clone(),
            head: pr.head.clone(),
        },
    )
    .await;
}

/// Every change in the repo, newest first — the shape the page already renders, and the order
/// Mongo's `sort({createdAt: -1})` gave it. `pulls::list` is oldest-number-first because the
/// padded key sorts that way, so the reversal happens here rather than in the store.
pub(super) async fn api_pulls(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    // The PARSED repo, not the raw path: `open_ro` strips `.git`, and `ready` opens whatever name
    // it is handed — the raw one would conjure `repo/alice/web.git`, a database under a key no
    // routing ever names.
    let repo = match open_ro(&app, &trusted, &headers, &owner, &name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let db = match ready(&app, &repo.owner, &repo.name).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    match pulls::list(&db).await {
        Ok(mut v) => {
            v.reverse();
            // The page renders a count, so the list carries a count: a 25-PR page
            // was shipping every comment body ever written just to say "3 comments".
            // Filtering happens on the serialized value so the state names here are
            // exactly the ones the wire already speaks.
            let mut out: Vec<serde_json::Value> = v
                .into_iter()
                .map(|p| {
                    let n = p.comments.len();
                    let mut j = serde_json::to_value(&p).unwrap_or_default();
                    if let Some(o) = j.as_object_mut() {
                        o.remove("comments");
                        o.insert("commentCount".into(), n.into());
                    }
                    j
                })
                .collect();
            if let Some(want) = q.get("state") {
                out.retain(|j| j["state"] == serde_json::Value::String(want.clone()));
            }
            if let Some(n) = q.get("limit").and_then(|s| s.parse::<usize>().ok()) {
                out.truncate(n);
            }
            Json(out).into_response()
        }
        Err(e) => internal(e),
    }
}

pub(super) async fn api_pull(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Response {
    // Parsed, not raw: see `api_pulls`.
    let repo = match open_ro(&app, &trusted, &headers, &owner, &name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let db = match ready(&app, &repo.owner, &repo.name).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    match pulls::get(&db, number).await {
        Ok(Some(pr)) => Json(pr).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such change").into_response(),
        Err(e) => internal(e),
    }
}

/// The one write shape the api tier already speaks — same field names as its own `NewPull`, so
/// forwarding is a pass-through of the body it was handed.
#[derive(serde::Deserialize)]
pub(super) struct NewPull {
    title: String,
    #[serde(default)]
    body: String,
    base: String,
    head: String,
    /// Who is opening it. The node has no idea who the caller is — the api tier does, and says so.
    #[serde(default)]
    author: String,
}

pub(super) async fn api_pull_open(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    Json(new): Json<NewPull>,
) -> Response {
    let (owner, name) = match writable(&app, &owner, &name).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let title: String = new.title.trim().chars().take(200).collect();
    if title.is_empty() {
        return (StatusCode::BAD_REQUEST, "a title is required").into_response();
    }
    let (base, head) = (new.base.trim(), new.head.trim());
    if base == head {
        return (StatusCode::BAD_REQUEST, "a change has to come from a different branch")
            .into_response();
    }
    let db = match ready(&app, &owner, &name).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    // Migration first, then the number: `ensure_migrated` raises `meta/next_pull` past every
    // number Mongo already handed out, so a migrated repo cannot reissue one.
    let number = match pulls::next_number(&app.store, &owner, &name).await {
        Ok(n) => n,
        Err(e) => return internal(e),
    };
    let repo = format!("{owner}/{name}");
    let pr = PullRequest {
        id: format!("{repo}#{number}"),
        repo,
        number,
        title,
        body: new.body.trim().to_string(),
        base: base.to_string(),
        head: head.to_string(),
        state: PullState::Open,
        author: new.author.clone(),
        created_at_ms: now_ms(),
        merged_at_ms: None,
        comments: Vec::new(),
        merge: None,
        mergeability: None,
        check_at_ms: None,
    };
    if let Err(e) = pulls::put(&db, &pr).await {
        return internal(e);
    }
    emit(&app, crate::events::Kind::PullOpened, &pr, &new.author).await;
    (StatusCode::CREATED, Json(pr)).into_response()
}

#[derive(serde::Deserialize)]
pub(super) struct NewComment {
    body: String,
    #[serde(default)]
    author: String,
}

pub(super) async fn api_pull_comment(
    State(app): State<Arc<App>>,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Json(new): Json<NewComment>,
) -> Response {
    let (owner, name) = match writable(&app, &owner, &name).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let body: String = new.body.trim().chars().take(10_000).collect();
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "say something").into_response();
    }
    let pr = match update(&app, &owner, &name, number, |pr| {
        pr.comments.push(Comment { author: new.author.clone(), body, at_ms: now_ms() });
        None
    })
    .await
    {
        Ok(pr) => pr,
        Err(r) => return r,
    };
    emit(&app, crate::events::Kind::PullCommented, &pr, &new.author).await;
    StatusCode::NO_CONTENT.into_response()
}

/// Ask for a merge. 409 rather than a second 202 when one is already queued or running: asking
/// twice must not queue it twice, and saying so is more use than a repeated "accepted".
pub(super) async fn api_pull_merge(
    State(app): State<Arc<App>>,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let (owner, name) = match writable(&app, &owner, &name).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let strategy = match q.get("strategy").map(String::as_str).unwrap_or("fast-forward") {
        s @ ("fast-forward" | "squash" | "merge" | "rebase") => s.to_string(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "strategy must be fast-forward, squash, merge or rebase",
            )
                .into_response()
        }
    };
    let who = q.get("by").cloned().unwrap_or_default();
    let pr = match update(&app, &owner, &name, number, |pr| {
        // A finished-but-failed job may be retried, which is why only these two block a new one —
        // the same condition Mongo's `request_merge` matched on. Checked inside the lock, where
        // the answer is still true by the time the write lands.
        let in_flight = matches!(
            pr.merge.as_ref().map(|m| m.state),
            Some(crate::pulls::MergeState::Queued | crate::pulls::MergeState::Running)
        );
        if pr.state != PullState::Open || in_flight {
            return Some(
                (StatusCode::CONFLICT, "this change is not open, or a merge is already under way")
                    .into_response(),
            );
        }
        pr.merge = Some(MergeJob {
            state: crate::pulls::MergeState::Queued,
            strategy,
            requested_by: who.clone(),
            requested_at_ms: now_ms(),
            claimed_at_ms: None,
            claimed_by: None,
            detail: None,
            announced_at_ms: None,
        });
        None
    })
    .await
    {
        Ok(pr) => pr,
        Err(r) => return r,
    };
    // The event IS the kick. This node does not merge — it records the job and serves the git
    // protocol the worker performs the merge over — so there is nothing to spawn here, and the
    // floor if the event is lost is `App::announce_stranded_merges` re-announcing it.
    emit(&app, crate::events::Kind::MergeRequested, &pr, &who).await;
    (StatusCode::ACCEPTED, "merging").into_response()
}

pub(super) async fn api_pull_close(
    State(app): State<Arc<App>>,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let (owner, name) = match writable(&app, &owner, &name).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let who = q.get("by").cloned().unwrap_or_default();
    let pr = match update(&app, &owner, &name, number, |pr| {
        if pr.state != PullState::Open {
            return Some((StatusCode::CONFLICT, "this change is not open").into_response());
        }
        pr.state = PullState::Closed;
        None
    })
    .await
    {
        Ok(pr) => pr,
        Err(r) => return r,
    };
    emit(&app, crate::events::Kind::PullClosed, &pr, &who).await;
    StatusCode::NO_CONTENT.into_response()
}

/// Recompute this change's mergeability, here, on the node that owns the repo.
///
/// The caller says only "go look" — no state, no oids. It cannot know the answer: the refs and
/// the objects are here, and reading this repo's database anywhere else fences this node. So the
/// merge worker's stream nudge becomes one POST, and the computation stays where the truth is.
/// The owner's own periodic lane calls the same `pulls::check`, which is what makes a lost nudge
/// cost latency rather than a check.
///
/// Number `0` means the whole repo, matching the `HeadMoved` event that is about a branch and
/// names no single change.
///
/// Always 204, even when nothing needed doing: the caller acks its stream entry either way, and
/// "already up to date" is not a failure it could act on.
///
/// No event: this is an answer, not a change anyone asked to be told about.
pub(super) async fn api_pull_check(
    State(app): State<Arc<App>>,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Response {
    let (owner, name) = match writable(&app, &owner, &name).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    if let Err(r) = ready(&app, &owner, &name).await {
        return r;
    }
    let done = if number == 0 {
        pulls::check_repo(&app.store, &owner, &name).await
    } else {
        pulls::check(&app.store, &owner, &name, number).await.map(|c| match c {
            pulls::Checked::Deep(d) => vec![d],
            _ => Vec::new(),
        })
    };
    match done {
        Ok(deep) => Json(deep).into_response(),
        Err(e) => internal(e),
    }
}

// ---------------------------------------------------------------------------
// The worker's three endpoints: claim a merge, report how it went, report a trial merge.
//
// Peer-only in the strong sense — `Trusted(Some(_))`, so the shared secret AND an asserted
// identity, never a Bearer token. These are not a person's routes: they hand out work and write
// outcomes, and the only caller that may is the fleet's own worker.
// ---------------------------------------------------------------------------

/// The peer secret alone is not enough; the caller must also say who it acts as, and that must be
/// the repo's owner. `trust_peer` has already checked the secret for everything on this listener.
fn as_owner(trusted: &Trusted, owner: &str) -> Result<(), Response> {
    match trusted.0.as_deref() {
        Some(o) if o == owner => Ok(()),
        Some(_) => Err((StatusCode::FORBIDDEN, "not this repository's owner").into_response()),
        None => Err((StatusCode::BAD_REQUEST, "caller identity required").into_response()),
    }
}

/// Take THIS change's queued merge, if it is still there to take.
///
/// 409, not 404, when it is not: the worker acted on a nudge, and "someone already has it" is the
/// normal answer to a duplicate delivery, not a fault. The lease is `App::MERGE_LEASE`, so a
/// worker that dies mid-merge strands the change for that long and no longer.
pub(super) async fn api_pull_claim(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let (owner, name) = match writable(&app, &owner, &name).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    if let Err(r) = as_owner(&trusted, &owner) {
        return r;
    }
    if let Err(r) = ready(&app, &owner, &name).await {
        return r;
    }
    let me = q.get("by").cloned().unwrap_or_else(|| "worker".to_string());
    match pulls::claim_merge_number(&app.store, &owner, &name, number, App::MERGE_LEASE, &me).await
    {
        Ok(Some(pr)) => {
            let job = pr.merge.as_ref();
            Json(crate::merge_worker::Job {
                owner,
                name,
                number: pr.number,
                strategy: job.map(|j| j.strategy.clone()).unwrap_or_default(),
                base: pr.base.clone(),
                head: pr.head.clone(),
                title: pr.title.clone(),
                requested_by: job.map(|j| j.requested_by.clone()).unwrap_or_default(),
            })
            .into_response()
        }
        Ok(None) => (StatusCode::CONFLICT, "no merge to claim").into_response(),
        Err(e) => internal(e),
    }
}

/// Record how a merge ended. The one place a change becomes `Merged`.
///
/// The worker has already pushed by the time this arrives — the ref moved through `receive-pack`
/// like any other push, protection rules and all — so this writes what happened, it does not
/// decide it.
///
/// `?by=` must be the token that WON the claim. A worker whose lease lapsed mid-merge may still be
/// running, and its late report would otherwise overwrite the state of the worker that has the job
/// now: "merged" on a change the new claimant is still merging, or "failed" on one that just
/// landed. Checked inside the lock, against `claimed_by`, where the answer is still true when the
/// write lands. 409 and no write when it does not match — the same answer a lost claim gets.
///
/// One read-modify-write for the whole outcome, rather than a state change and then a job change:
/// a crash between two writes would leave a merged change carrying a live job, or a cleared job on
/// a change that never merged.
pub(super) async fn api_pull_outcome(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Query(q): Query<HashMap<String, String>>,
    // Raw bytes, not `Json<Outcome>`: an extractor runs BEFORE the handler, so a malformed body
    // would be answered 422 by a caller who was never authorized to reach this route at all.
    // Authorization first, parsing second.
    body: axum::body::Bytes,
) -> Response {
    use crate::merge_worker::OutcomeState;
    let (owner, name) = match writable(&app, &owner, &name).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    if let Err(r) = as_owner(&trusted, &owner) {
        return r;
    }
    let out: crate::merge_worker::Outcome = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    let by = q.get("by").cloned().unwrap_or_default();
    let detail = out.detail.as_deref().map(str::trim).filter(|d| !d.is_empty()).map(str::to_string);
    let merged = out.state == OutcomeState::Merged;
    let pr = match update(&app, &owner, &name, number, |pr| {
        // An empty `by` never matches: a claim always records a token, so a report without one is
        // from something that never claimed.
        if pr.merge.as_ref().and_then(|j| j.claimed_by.as_deref()) != Some(by.as_str()) {
            return Some(
                (StatusCode::CONFLICT, "this merge is claimed by someone else").into_response(),
            );
        }
        match out.state {
            OutcomeState::Merged => {
                pr.state = PullState::Merged;
                if pr.merged_at_ms.is_none() {
                    pr.merged_at_ms = Some(now_ms());
                }
                // A merged change already records that it merged, in its own state; `Queued` is
                // not a state a finished job stays in, so clearing is the honest end.
                pr.merge = None;
            }
            // The job stays, carrying the reason: a failed merge is retryable from the page, and
            // the person waiting is owed git's own words for why it stopped.
            OutcomeState::Conflicts | OutcomeState::Refused => {
                if let Some(job) = pr.merge.as_mut() {
                    job.state = if out.state == OutcomeState::Conflicts {
                        crate::pulls::MergeState::Conflicts
                    } else {
                        crate::pulls::MergeState::Failed
                    };
                    job.detail = detail.clone();
                }
            }
        }
        None
    })
    .await
    {
        Ok(pr) => pr,
        Err(r) => return r,
    };
    if merged {
        // Two events, not one: `PullMerged` is this change's own timeline entry, while `HeadMoved`
        // says the base branch tip moved — which is what makes every OTHER open change against
        // that base worth re-checking.
        emit(&app, crate::events::Kind::PullMerged, &pr, &pr.author).await;
        let moved = PullRequest {
            number: 0,
            title: String::new(),
            base: String::new(),
            head: String::new(),
            ..pr
        };
        emit(&app, crate::events::Kind::HeadMoved, &moved, "").await;
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Record a trial merge's verdict on a change the cheap check could not answer for.
///
/// Only the state, the sentence and the fast-forward flag: the two tips the answer belongs to were
/// stamped by the check that asked for this, and keeping them is what lets the NEXT check see that
/// a branch has moved since. A change with no recorded check at all is left alone — the verdict
/// would have no tips to be true of.
pub(super) async fn api_pull_mergeability(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    Path((owner, name, number)): Path<(String, String, i64)>,
    // Bytes, not `Json<Verdict>`, for the same reason as `api_pull_outcome`: authorize first.
    body: axum::body::Bytes,
) -> Response {
    let (owner, name) = match writable(&app, &owner, &name).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    if let Err(r) = as_owner(&trusted, &owner) {
        return r;
    }
    let v: crate::merge_worker::Verdict = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    match update(&app, &owner, &name, number, |pr| {
        let Some(m) = pr.mergeability.as_mut() else {
            return Some(StatusCode::NO_CONTENT.into_response());
        };
        m.state = v.state;
        m.detail = v.detail.clone();
        m.fast_forward = v.fast_forward;
        None
    })
    .await
    {
        // `update`'s refusal shape carries the 204 for "nothing recorded yet" as well.
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(r) => r,
    }
}

/// The HTTP face of `pulls::modify`: same lock, same read-modify-write, with a refusal that is a
/// `Response`. `f` returns `Some(response)` to refuse — which keeps the state checks (is it still
/// open? is a merge already in flight?) INSIDE the lock, where they are actually decisive.
async fn update(
    app: &App,
    owner: &str,
    name: &str,
    number: i64,
    f: impl FnOnce(&mut PullRequest) -> Option<Response>,
) -> Result<PullRequest, Response> {
    ready(app, owner, name).await?;
    let mut refusal = None;
    let done = pulls::modify(&app.store, owner, name, number, |pr| match f(pr) {
        Some(r) => {
            refusal = Some(r);
            false
        }
        None => true,
    })
    .await
    .map_err(internal)?;
    match done {
        Some(pr) => Ok(pr),
        // No refusal recorded and nothing written means the change is not there.
        None => Err(refusal
            .unwrap_or_else(|| (StatusCode::NOT_FOUND, "no such change").into_response())),
    }
}
