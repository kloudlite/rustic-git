//! Branch comparison, fast-forward/squash merges, and the single-commit patch API.
use super::{hidden, odb_json, open_ro};
use crate::router::internal;
use kloudlite_git_core::httpx::Trusted;
use crate::App;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// What a merge answers with: the commit the base now points at.
#[derive(Serialize)]
pub(super) struct Merged {
    merged: String,
}

/// What a branch would bring to another, and whether it can be applied without a
/// merge commit. Both refs are SHORT branch names — a review is about branches,
/// and resolving them here means the answer follows a push rather than pinning to
/// whatever oid a client last saw.
pub(super) async fn api_compare(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let repo = match open_ro(&app, &trusted, &headers, &owner, &name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let (Some(base), Some(head)) = (q.get("base"), q.get("head")) else {
        return (StatusCode::BAD_REQUEST, "base and head are required").into_response();
    };
    let (base_ref, head_ref) = (format!("refs/heads/{base}"), format!("refs/heads/{head}"));
    let (base_oid, head_oid) = match tokio::join!(
        app.store.get_ref(&repo, &base_ref),
        app.store.get_ref(&repo, &head_ref),
    ) {
        (Ok(Some(b)), Ok(Some(h))) => (b, h),
        (Err(e), _) | (_, Err(e)) => return internal(e),
        // A branch that is not there is the caller's mistake to see, not a 500.
        _ => return (StatusCode::NOT_FOUND, "no such branch").into_response(),
    };
    let n = q.get("n").and_then(|v| v.parse::<usize>().ok()).unwrap_or(250).clamp(1, 1000);
    odb_json(repo, move |odb| crate::browse::compare(odb, base_oid, head_oid, n)).await
}

/// Apply a change by moving `base` to `head`.
///
/// Fast-forward only. A true merge means writing a new commit, and a new commit
/// means a three-way merge of two trees — real work that can conflict, which this
/// server cannot yet do. Moving a ref cannot conflict and cannot lose anything, so
/// it is the honest subset to ship first. Anything else is refused with the reason,
/// and the branch owner rebases.
///
/// It goes through `update_refs`, so BRANCH PROTECTION applies to a merge exactly
/// as it applies to a push — a protected base is not a back door.
pub(super) async fn api_merge(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let (Some(base), Some(head)) = (q.get("base"), q.get("head")) else {
        return (StatusCode::BAD_REQUEST, "base and head are required").into_response();
    };
    let strategy = q.get("strategy").map(String::as_str).unwrap_or("fast-forward");
    match perform(&app, &owner, &name, base, head, strategy, q.get("message").cloned()).await {
        Ok(oid) => Json(Merged { merged: oid }).into_response(),
        Err((code, why)) => (code, why).into_response(),
    }
}

/// The merge itself, without HTTP.
///
/// Two callers, and both are on the node that owns the repo: `api_merge` above, and the owner's
/// own merge lane (`announce_stranded_merges` in `lanes.rs`), which claims a queued job from the repo's database
/// and lands it by calling straight in here. The lane deliberately does NOT go back out through
/// the router to reach code in the same process.
///
/// The refusal is a status and a sentence, because both callers pass it on to a person: the
/// HTTP one as a response, the lane by writing it onto the job as `detail`.
pub(crate) async fn perform(
    app: &App,
    owner: &str,
    name: &str,
    base: &str,
    head: &str,
    strategy: &str,
    message: Option<String>,
) -> std::result::Result<String, (StatusCode, String)> {
    let bad = |c: StatusCode, m: &str| (c, m.to_string());
    let Some((owner, name)) = crate::protocol::parse_repo_pair(owner, name) else {
        return Err(bad(StatusCode::BAD_REQUEST, "invalid repository path"));
    };
    // Defined after the parse so it can name the repo. The backend's own words go to the log
    // only: a `boom` forwarded verbatim surfaced SlateDB text in the PR UI.
    let boom = |e: crate::Error| {
        tracing::error!(owner = %owner, repo = %name, error = %e, "merge.record.failed");
        (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
    };
    let repo = match app.store.open_repo(&owner, &name).await {
        Ok(Some(r)) => r,
        Ok(None) => return Err(bad(StatusCode::NOT_FOUND, "not found")),
        Err(e) => return Err(boom(e)),
    };
    let (base_ref, head_ref) = (format!("refs/heads/{base}"), format!("refs/heads/{head}"));
    let (base_oid, head_oid) = match tokio::join!(
        app.store.get_ref(&repo, &base_ref),
        app.store.get_ref(&repo, &head_ref),
    ) {
        (Ok(Some(b)), Ok(Some(h))) => (b, h),
        (Err(e), _) | (_, Err(e)) => return Err(boom(e)),
        _ => return Err(bad(StatusCode::NOT_FOUND, "no such branch")),
    };

    // Everything that touches the odb — the ancestry walk and the head commit's fields — runs
    // on a blocking thread: `merge_base` is a 50k-commit walk, and doing it on the runtime
    // starves every other request on that worker. `odb_json` makes the same move for reads.
    struct HeadInfo {
        tree: gix_hash::ObjectId,
        who: String,
        mail: String,
        time: i64,
    }
    let need_head = matches!(strategy, "squash" | "merge");
    let r = repo.clone();
    let walked = tokio::task::spawn_blocking(move || -> crate::Result<(bool, Option<HeadInfo>)> {
        let odb = r.odb()?;
        // Re-checked HERE rather than trusted from whatever the caller last read: the branch may
        // have moved since the page was rendered.
        if crate::browse::merge_base(&odb, base_oid, head_oid, 50_000) != crate::browse::MergeBase::Found(base_oid) {
            return Ok((true, None));
        }
        if !need_head {
            return Ok((false, None));
        }
        let mut buf = Vec::new();
        let c = gix_object::FindExt::find_commit(&odb, &head_oid, &mut buf)
            .map_err(|e| crate::err(e.to_string()))?;
        let author = c.author().ok();
        let (who, mail) = match &author {
            Some(a) => (a.name.to_string(), a.email.to_string()),
            None => ("kloudlite".to_string(), "noreply@kloudlite.io".to_string()),
        };
        // The commit time comes from the head commit, not the clock, so merging the same branch
        // twice produces the same id — which is what makes a retried merge idempotent.
        let time = author.as_ref().and_then(|a| a.time().ok()).map(|t| t.seconds).unwrap_or(0);
        Ok((false, Some(HeadInfo { tree: c.tree(), who, mail, time })))
    })
    .await;
    // `head_info`, not `head`: `head` is the branch name and is still needed for the message.
    let (behind, head_info) = match walked {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(boom(e)),
        Err(e) => return Err(boom(crate::err(format!("merge task: {e}")))),
    };
    if behind {
        return Err(bad(StatusCode::CONFLICT, "this branch is behind its base — rebase it and push again"));
    }

    // Which shape to land it in. All three are safe HERE and only here: the base is an ancestor
    // of the head, so the content being landed is exactly the head's tree and no three-way merge
    // is possible or needed. On a diverged branch these would each need a real merge, which is
    // why that case is refused above rather than guessed at.
    let new_tip = match strategy {
        // The ref simply moves; no new object.
        "fast-forward" | "rebase" => head_oid,
        "squash" | "merge" => {
            let Some(HeadInfo { tree, who, mail, time }) = head_info else {
                return Err(boom(crate::err("head commit not read")));
            };
            let parents = if strategy == "squash" { vec![base_oid] } else { vec![base_oid, head_oid] };
            let message = message.unwrap_or_else(|| format!("Merge {head} into {base}\n"));
            match crate::objects::write_commit(
                &app.store,
                &repo,
                crate::objects::NewCommit { tree, parents, message, author_name: who, author_email: mail, time },
            )
            .await
            {
                Ok(oid) => oid,
                Err(e) => return Err(boom(e)),
            }
        }
        _ => {
            return Err(bad(StatusCode::BAD_REQUEST, "strategy must be fast-forward, squash, merge or rebase"))
        }
    };

    let update = vec![crate::refs::RefUpdate {
        name: base_ref,
        old: Some(base_oid),
        new: Some(new_tip),
    }];
    match crate::refs::update_refs(&app.store, &repo, &update).await {
        Ok(r) => match r.into_iter().next().flatten() {
            None => Ok(new_tip.to_hex().to_string()),
            // A protection rule refused it. Its own words, for the person waiting.
            Some(reason) => Err((StatusCode::CONFLICT, reason)),
        },
        Err(e) => Err(boom(e)),
    }
}

/// One file's worth of a patch, as the api tier sends it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FileChange {
    path: String,
    /// Base64: a file is arbitrary bytes and JSON carries text, so the bytes
    /// cannot go over as a string. Absent when `delete` is set.
    content_base64: Option<String>,
    /// `None` keeps the mode the file already has.
    executable: Option<bool>,
    #[serde(default)]
    delete: bool,
}

/// A patch: one commit, any number of files.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Patch {
    /// The branch the editor was reading.
    branch: String,
    /// The tip it was reading, if the caller knows it. The commit is refused if
    /// the branch has moved since — someone pushed while the editor was open,
    /// and landing on top of a tip we never saw would silently drop their work.
    expect: Option<String>,
    message: String,
    author_name: String,
    author_email: String,
    /// Commit onto a NEW branch of this name instead of moving `branch`. This is
    /// what "start a pull request from this edit" is: the base branch does not
    /// move at all, so it can be protected and the edit still lands.
    new_branch: Option<String>,
    changes: Vec<FileChange>,
}

#[derive(Serialize)]
pub(super) struct Committed {
    commit: String,
    branch: String,
}

/// Apply a patch as one commit.
///
/// The whole patch lands or none of it does: the blobs and trees are staged and
/// written together, and the ref moves only once the commit is stored. And the
/// ref moves by compare-and-swap, so a push that arrives mid-edit loses the race
/// rather than being overwritten.
pub(super) async fn api_patch(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    Json(patch): Json<Patch>,
) -> Response {
    let Some((owner, name)) = crate::protocol::parse_repo_pair(&owner, &name) else {
        return (StatusCode::BAD_REQUEST, "invalid repository path").into_response();
    };
    let repo = match app.store.open_repo(&owner, &name).await {
        Ok(Some(r)) => r,
        Ok(None) => return hidden(),
        Err(e) => return internal(e),
    };
    if patch.changes.is_empty() {
        return (StatusCode::BAD_REQUEST, "a commit needs at least one change").into_response();
    }
    if patch.message.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "a commit needs a message").into_response();
    }

    let branch_ref = format!("refs/heads/{}", patch.branch);
    let tip = match app.store.get_ref(&repo, &branch_ref).await {
        Ok(t) => t,
        Err(e) => return internal(e),
    };
    // Read HERE rather than trusted from whatever the editor last saw.
    if let Some(expected) = &patch.expect {
        if tip.map(|t| t.to_hex().to_string()).as_deref() != Some(expected.as_str()) {
            return (
                StatusCode::CONFLICT,
                "this branch has moved since you started editing",
            )
                .into_response();
        }
    }
    let Some(tip) = tip else {
        return (StatusCode::NOT_FOUND, "no such branch").into_response();
    };

    let mut changes = std::collections::BTreeMap::new();
    for c in patch.changes {
        let change = if c.delete {
            crate::objects::Change::Delete
        } else {
            use base64::Engine;
            let Some(b64) = c.content_base64.as_deref() else {
                return (StatusCode::BAD_REQUEST, format!("{}: no content", c.path)).into_response();
            };
            match base64::engine::general_purpose::STANDARD.decode(b64) {
                Ok(content) => crate::objects::Change::Upsert { content, executable: c.executable },
                Err(_) => {
                    return (StatusCode::BAD_REQUEST, format!("{}: content is not base64", c.path))
                        .into_response()
                }
            }
        };
        // Two changes to one path have no defined order, so the patch is refused
        // rather than one of them silently winning.
        if changes.insert(c.path.clone(), change).is_some() {
            return (StatusCode::BAD_REQUEST, format!("{} appears twice", c.path)).into_response();
        }
    }

    // The base tree, the staged blobs/trees and the "did anything change" answer all need the
    // odb; one blocking task does the three together. `apply_changes`' refusals are the
    // caller's to see (a path that is a directory, a missing parent), so they come back as a
    // message for a 400 rather than as a fault.
    let r = repo.clone();
    // The staged result, or the caller's own words for a 400.
    type Applied = std::result::Result<(gix_hash::ObjectId, crate::objects::Staging), String>;
    let staged = tokio::task::spawn_blocking(move || -> crate::Result<(gix_hash::ObjectId, Applied)> {
        let odb = r.odb()?;
        let mut buf = Vec::new();
        let base_tree = gix_object::FindExt::find_commit(&odb, &tip, &mut buf)
            .map_err(|e| crate::err(e.to_string()))?
            .tree();
        let mut staging = crate::objects::Staging::default();
        let applied = crate::objects::apply_changes(&odb, Some(base_tree), changes, &mut staging)
            .map(|t| (t, staging))
            .map_err(|e| e.to_string());
        Ok((base_tree, applied))
    })
    .await;
    let (base_tree, tree, staging) = match staged {
        Ok(Ok((base_tree, Ok((tree, staging))))) => (base_tree, tree, staging),
        Ok(Ok((_, Err(why)))) => return (StatusCode::BAD_REQUEST, why).into_response(),
        Ok(Err(e)) => return internal(e),
        Err(e) => return internal(crate::err(format!("patch task: {e}"))),
    };
    // Nothing actually changed: the same bytes were sent back. A commit here would be an empty
    // one, which is noise in the history rather than a record.
    if tree == base_tree {
        return (StatusCode::BAD_REQUEST, "this changes nothing").into_response();
    }

    // Blobs and trees FIRST: a commit is validated against what is stored, so it
    // cannot be written before the tree it points at.
    if let Err(e) = staging.write(&app.store, &repo).await {
        return internal(e);
    }
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let commit = match crate::objects::write_commit(
        &app.store,
        &repo,
        crate::objects::NewCommit {
            tree,
            parents: vec![tip],
            message: patch.message,
            author_name: patch.author_name,
            author_email: patch.author_email,
            time,
        },
    )
    .await
    {
        Ok(c) => c,
        Err(e) => return internal(e),
    };

    // Onto a new branch, the update is a CREATE (`old: None`), so it cannot
    // overwrite a branch of that name that already exists.
    let (target, old) = match &patch.new_branch {
        Some(b) => (format!("refs/heads/{b}"), None),
        None => (branch_ref, Some(tip)),
    };
    let landed_on = patch.new_branch.clone().unwrap_or(patch.branch);
    match crate::refs::update_refs(
        &app.store,
        &repo,
        &[crate::refs::RefUpdate { name: target, old, new: Some(commit) }],
    )
    .await
    {
        Ok(r) => match r.into_iter().next().flatten() {
            None => Json(Committed { commit: commit.to_hex().to_string(), branch: landed_on })
                .into_response(),
            Some(reason) => (StatusCode::CONFLICT, reason).into_response(),
        },
        Err(e) => internal(e),
    }
}
