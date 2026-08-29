mod common;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

async fn get_as(
    router: &axum::Router,
    as_owner: &str,
    path: &str,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .uri(path)
        .header(rustic_git_core::peer::PEER_HEADER, "test-peer-secret")
        .header(rustic_git_core::peer::OWNER_HEADER, as_owner)
        .body(axum::body::Body::empty())
        .unwrap();
    let r = router.clone().oneshot(req).await.unwrap();
    let status = r.status();
    let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (
        status,
        serde_json::from_slice::<serde_json::Value>(&body).unwrap_or_default(),
    )
}

async fn post_as(router: &axum::Router, as_owner: &str, path: &str) -> StatusCode {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header(rustic_git_core::peer::PEER_HEADER, "test-peer-secret")
        .header(rustic_git_core::peer::OWNER_HEADER, as_owner)
        .body(axum::body::Body::empty())
        .unwrap();
    router.clone().oneshot(req).await.unwrap().status()
}

async fn post_full_as(router: &axum::Router, as_owner: &str, path: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header(rustic_git_core::peer::PEER_HEADER, "test-peer-secret")
        .header(rustic_git_core::peer::OWNER_HEADER, as_owner)
        .body(axum::body::Body::empty())
        .unwrap();
    let r = router.clone().oneshot(req).await.unwrap();
    let status = r.status();
    let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// Name uniqueness is the owning node's answer now, not a Mongo unique index. A repeat create
/// must be the distinct 409 the api tier maps to "a repository of that name already exists" —
/// a 500 there would tell the person a name they cannot have is a service fault.
#[tokio::test(flavor = "multi_thread")]
async fn a_repeat_create_is_a_conflict_not_a_fault() {
    let e = common::env().await;
    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());
    assert_eq!(post_as(&router, "alice", "/api/alice/widget/create").await, StatusCode::CREATED);
    let (status, body) = post_full_as(&router, "alice", "/api/alice/widget/create").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body, "repository already exists");
}

/// THE uniqueness guarantee, moved off Mongo's unique `_id`. Both creates route to the same
/// node by repo key, so check-then-create there must be serialized: exactly one winner, and the
/// loser hears 409 rather than silently overwriting the winner's repo.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_creates_of_one_name_leave_exactly_one_winner() {
    let e = common::env().await;
    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let r = router.clone();
        tasks.push(tokio::spawn(async move {
            post_as(&r, "alice", "/api/alice/widget/create").await
        }));
    }
    let mut created = 0;
    for t in tasks {
        let s = t.await.unwrap();
        match s {
            StatusCode::CREATED => created += 1,
            StatusCode::CONFLICT => {}
            other => panic!("a racing create answered {other}"),
        }
    }
    assert_eq!(created, 1, "exactly one create may win the name");
}

/// The whole point of the move: a create with no directory anywhere still lands everything a
/// listing and the feed's `repo_created` row need, in the repo's own database and its marker.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_alone_furnishes_the_listing_and_the_feed_row() {
    use rustic_git_storage::index::{self, Kind};

    let e = common::env().await;
    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());
    let s = post_as(
        &router,
        "alice",
        "/api/alice/widget/create?visibility=public&description=the%20widget&created_by=alice&created_at_ms=1700000000000",
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);

    let markers = index::list(&e.store, Kind::Repo, "alice", false).await.unwrap();
    let m = markers.iter().find(|m| m.name == "widget").expect("no marker for the new repo");
    assert!(m.public);
    assert_eq!(m.description, "the widget");
    assert_eq!(m.created_by, "alice");
    assert_eq!(m.created_ms, 1_700_000_000_000);
}

/// The create-rollback path: when the fleet create fails the api tier releases the name by
/// deleting it on the owner, and the name has to be genuinely free afterwards — otherwise the
/// person who could not create a repo can never try again.
#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_name_can_be_claimed_again() {
    let e = common::env().await;
    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());
    assert_eq!(post_as(&router, "alice", "/api/alice/widget/create").await, StatusCode::CREATED);
    assert_eq!(post_as(&router, "alice", "/api/alice/widget/delete").await, StatusCode::NO_CONTENT);
    assert_eq!(
        post_as(&router, "alice", "/api/alice/widget/create").await,
        StatusCode::CREATED,
        "a released name must be claimable again"
    );
}

/// Create/flip/delete must keep the listing-index markers in sync with the real state: a
/// listing answers from these markers without opening the repo's own database, so a marker left
/// stale after an admin op would show a name that no longer exists, or hide one that does.
#[tokio::test(flavor = "multi_thread")]
async fn repo_lifecycle_maintains_markers() {
    use rustic_git_storage::index::{self, Kind};
    use slatedb::object_store::ObjectStoreExt;

    let e = common::env().await;
    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());

    let priv_path = index::path(false, Kind::Repo, "alice", "widget");
    let pub_path = index::path(true, Kind::Repo, "alice", "widget");

    // create private -> private marker exists, public absent
    let s = post_as(&router, "alice", "/api/alice/widget/create").await;
    assert_eq!(s, StatusCode::CREATED);
    assert!(e.store.os.get(&priv_path).await.is_ok(), "private marker missing after create");
    assert!(e.store.os.get(&pub_path).await.is_err(), "public marker present after private create");

    // flip public -> public marker exists, private absent
    let s = post_as(&router, "alice", "/api/alice/widget/visibility?visibility=public").await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(e.store.os.get(&pub_path).await.is_ok(), "public marker missing after flip");
    assert!(e.store.os.get(&priv_path).await.is_err(), "private marker left behind after flip");

    // delete -> both absent
    let s = post_as(&router, "alice", "/api/alice/widget/delete").await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(e.store.os.get(&pub_path).await.is_err(), "public marker survived delete");
    assert!(e.store.os.get(&priv_path).await.is_err(), "private marker survived delete");

    // create with visibility=public -> public marker exists right away, private absent
    let pub_path2 = index::path(true, Kind::Repo, "alice", "widget2");
    let priv_path2 = index::path(false, Kind::Repo, "alice", "widget2");
    let s = post_as(&router, "alice", "/api/alice/widget2/create?visibility=public").await;
    assert_eq!(s, StatusCode::CREATED);
    assert!(e.store.os.get(&pub_path2).await.is_ok(), "public marker missing after public create");
    assert!(e.store.os.get(&priv_path2).await.is_err(), "private marker present after public create");
}

/// A delete and a visibility flip that overlap must not leave a marker for a repo that is gone:
/// the flip writes a marker, and if the delete's marker removal is not serialized against it, the
/// write lands after the removal and the listing keeps naming a deleted repo forever.
///
/// Ordered deterministically rather than raced: the test holds the very lock both handlers take,
/// queues the flip first and the delete second (tokio's mutex is FIFO-fair), then releases — so
/// the flip's marker write is guaranteed to be the one the delete has to clean up after.
#[tokio::test(flavor = "multi_thread")]
async fn a_delete_overlapping_a_flip_leaves_no_orphan_marker() {
    use rustic_git_storage::index::{self, Kind};
    use slatedb::object_store::ObjectStoreExt;

    let e = common::env().await;
    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());
    assert_eq!(post_as(&router, "alice", "/api/alice/widget/create").await, StatusCode::CREATED);

    let lock = e.store.keyed_lock("index/repo/alice/widget");
    let guard = lock.lock().await;

    let (r1, r2) = (router.clone(), router.clone());
    let flip = tokio::spawn(async move {
        post_as(&r1, "alice", "/api/alice/widget/visibility?visibility=public").await
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let del = tokio::spawn(async move { post_as(&r2, "alice", "/api/alice/widget/delete").await });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    drop(guard);
    flip.await.unwrap();
    assert_eq!(del.await.unwrap(), StatusCode::NO_CONTENT);

    for p in [index::path(true, Kind::Repo, "alice", "widget"), index::path(false, Kind::Repo, "alice", "widget")] {
        assert!(e.store.os.get(&p).await.is_err(), "orphan marker {p} survived the delete");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn refs_then_tree_then_blob_then_log_and_commit() {
    if !common::have_git() {
        eprintln!("skipping: no git"); // ponytail: eprintln
        return;
    }
    let e = common::env().await;
    common::push_fixture(&e, "alice", "web").await;
    let app = common::app(e.store.clone()).await;
    let router = rustic_git_server::router::peer_router(app, common::no_jobs_state());

    let (s, refs) = get_as(&router, "alice", "/api/alice/web/refs").await;
    assert_eq!(s, StatusCode::OK);
    let oid = refs[0]["oid"].as_str().unwrap().to_string();
    assert_eq!(refs[0]["kind"], "branch");

    let (s, tree) = get_as(&router, "alice", &format!("/api/alice/web/tree/{oid}")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(tree.as_array().unwrap().iter().any(|e| e["name"] == "src"));

    let (s, sub) = get_as(&router, "alice", &format!("/api/alice/web/tree/{oid}/src")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(sub.as_array().unwrap().iter().any(|e| e["name"] == "main.rs"));

    let (s, blob) = get_as(&router, "alice", &format!("/api/alice/web/blob/{oid}/src/main.rs")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(blob["truncated"], false);
    assert!(!blob["bytes_base64"].as_str().unwrap().is_empty());

    // Two commits from the fixture, and `n` clamps rather than errors.
    let (s, log) = get_as(&router, "alice", &format!("/api/alice/web/log/{oid}?n=999")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(log.as_array().unwrap().len(), 2);

    let (s, c) = get_as(&router, "alice", &format!("/api/alice/web/commit/{oid}")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(c["oid"], oid);
    assert!(c["diff"].as_str().unwrap().contains("main.rs"));

    let (s, _) = get_as(&router, "alice", &format!("/api/alice/web/tree/{oid}/nope")).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "unknown path is 404, never 500");

    let (s, _) = get_as(&router, "alice", "/api/alice/web/tree/not-an-oid").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "a malformed oid is 404, never 400/500");

    let (s, _) = get_as(&router, "alice", "/api/alice/ghost/refs").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn private_repo_is_404_to_a_stranger() {
    if !common::have_git() {
        eprintln!("skipping: no git"); // ponytail: eprintln
        return;
    }
    let e = common::env().await;
    common::push_fixture(&e, "alice", "web").await;
    let app = common::app(e.store.clone()).await;
    let (s, _) = get_as(&rustic_git_server::router::peer_router(app, common::no_jobs_state()), "bob", "/api/alice/web/refs").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "existence must not leak");
}

/// Catches the production behaviour the unit test alone would have missed: on a PUBLIC repo an
/// authenticated non-owner got 404 from the browse routes while an anonymous caller got 200.
#[tokio::test(flavor = "multi_thread")]
async fn public_repo_browses_for_a_non_owner_token() {
    if !common::have_git() {
        eprintln!("skipping: no git"); // ponytail: eprintln
        return;
    }
    let e = common::env().await;
    common::push_fixture(&e, "alice", "web").await;
    e.store.set_public("alice", "web", true).await.unwrap();
    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());
    let (s, refs) = get_as(&router, "bob", "/api/alice/web/refs").await;
    assert_eq!(s, StatusCode::OK, "public grants read to any authenticated caller");
    assert!(!refs.as_array().unwrap().is_empty());
    // and the owner is unaffected
    let (s, _) = get_as(&router, "alice", "/api/alice/web/refs").await;
    assert_eq!(s, StatusCode::OK);
}

/// The browse API is peer-only. On the public listener `/api/...` must 404 here, never be forwarded
/// to the owner's peer port with the shared secret.
#[tokio::test(flavor = "multi_thread")]
async fn public_router_does_not_serve_the_browse_api() {
    if !common::have_git() {
        eprintln!("skipping: no git"); // ponytail: eprintln
        return;
    }
    let e = common::env().await;
    common::push_fixture(&e, "alice", "web").await;
    let router = rustic_git_server::router::router(common::app(e.store.clone()).await, common::no_jobs_state());
    for path in ["/api/alice/web/refs", "/api/alice/web/tree/HEAD", "/api/alice/web/log/x"] {
        let req = Request::builder()
            .uri(path)
            .body(axum::body::Body::empty())
            .unwrap();
        let r = router.clone().oneshot(req).await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND, "{path} must not be public");
    }
}

/// The path both routers used to disagree about. `alice/info` is a real repo, so
/// `/api/alice/info/refs` is its browse route on the peer listener — and the handler that actually
/// runs must operate on `alice/info`, not on a repo `api/alice` the middleware picked instead.
#[tokio::test(flavor = "multi_thread")]
async fn a_repo_named_info_browses_as_itself() {
    if !common::have_git() {
        eprintln!("skipping: no git"); // ponytail: eprintln
        return;
    }
    let e = common::env().await;
    common::push_fixture(&e, "alice", "info").await;
    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());

    // 200 with real refs proves the handler opened `alice/info`: no other repo exists here, and a
    // handler that had been given owner=api name=alice would 404.
    let (s, refs) = get_as(&router, "alice", "/api/alice/info/refs").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(refs[0]["kind"], "branch");
    let oid = refs[0]["oid"].as_str().unwrap().to_string();
    let (s, tree) = get_as(&router, "alice", &format!("/api/alice/info/tree/{oid}")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(tree.as_array().unwrap().iter().any(|e| e["name"] == "src"));

    // The same path is alice's repo, so bob may not see it.
    let (s, _) = get_as(&router, "bob", "/api/alice/info/refs").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "existence must not leak");
}

/// ...and on the public listener that same path is a flat 404. Forwarding is what must not happen:
/// this node's peer address is 127.0.0.1:1, so a forward would surface as 502/503, not 404.
#[tokio::test(flavor = "multi_thread")]
async fn public_router_404s_a_repo_named_info() {
    if !common::have_git() {
        eprintln!("skipping: no git"); // ponytail: eprintln
        return;
    }
    let e = common::env().await;
    common::push_fixture(&e, "alice", "info").await;
    let router = rustic_git_server::router::router(common::app(e.store.clone()).await, common::no_jobs_state());
    for path in ["/api/alice/info/refs", "/api/alice/info", "/api/alice/git-upload-pack"] {
        let req = Request::builder()
            .uri(path)
            .body(axum::body::Body::empty())
            .unwrap();
        let r = router.clone().oneshot(req).await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND, "{path} must not be public");
    }
    // The public git route of the same repo is untouched: `/alice/info/info/refs`.
    let req = Request::builder()
        .uri("/alice/info/info/refs?service=git-upload-pack")
        .body(axum::body::Body::empty())
        .unwrap();
    let r = router.clone().oneshot(req).await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "private repo, but routed and reached");
}

/// `GET .../protect` used to skip the visibility gate entirely: any caller, owner or
/// stranger, got the branch-protection list. It must now behave exactly like every
/// other browse route.
#[tokio::test(flavor = "multi_thread")]
async fn protections_require_visibility() {
    if !common::have_git() {
        eprintln!("skipping: no git"); // ponytail: eprintln
        return;
    }
    let e = common::env().await;
    common::push_fixture(&e, "alice", "web").await;
    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());

    let (s, _) = get_as(&router, "bob", "/api/alice/web/protect").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "existence must not leak");

    let (s, list) = get_as(&router, "alice", "/api/alice/web/protect").await;
    assert_eq!(s, StatusCode::OK);
    assert!(list.as_array().is_some());
}

/// A crash between the DB visibility write and the marker swap leaves the two disagreeing —
/// the structural sweep can't see DB truth, only the owning node can. `reconcile_marker` must
/// move the marker to match the DB, in both directions, preserving the other body fields.
#[tokio::test(flavor = "multi_thread")]
async fn reconcile_marker_heals_crashed_flip() {
    use rustic_git_storage::index::{self, Kind, Marker};

    let e = common::env().await;
    e.store.create_repo("alice", "widget").await.unwrap();

    // DB says public, but a crashed flip left a PRIVATE marker behind.
    e.store.set_public("alice", "widget", true).await.unwrap();
    index::write(
        &e.store,
        Kind::Repo,
        "alice",
        &Marker {
            name: "widget".into(),
            public: false,
            created_by: "alice@example.com".into(),
            created_ms: 111,
            description: "a widget".into(),
            manifests: 0,
            updated_ms: 0,
        },
    )
    .await
    .unwrap();

    let db_public = e.store.is_public("alice", "widget").await.unwrap();
    let repaired = e.store.reconcile_marker("alice", "widget", Kind::Repo, db_public).await.unwrap();
    assert!(repaired);
    let m = index::read(&e.store.os, Kind::Repo, "alice", "widget").await.unwrap();
    assert!(m.public, "marker should have moved to public to match the DB");
    assert_eq!(m.created_by, "alice@example.com", "body fields must survive the repair");
    assert_eq!(m.created_ms, 111);

    // A second call is a no-op: already agrees.
    let db_public = e.store.is_public("alice", "widget").await.unwrap();
    let repaired_again = e.store.reconcile_marker("alice", "widget", Kind::Repo, db_public).await.unwrap();
    assert!(!repaired_again);

    // Inverse: DB says private, but a crashed flip left a PUBLIC marker behind.
    e.store.set_public("alice", "widget", false).await.unwrap();
    index::write(
        &e.store,
        Kind::Repo,
        "alice",
        &Marker {
            name: "widget".into(),
            public: true,
            created_by: "alice@example.com".into(),
            created_ms: 111,
            description: "a widget".into(),
            manifests: 0,
            updated_ms: 0,
        },
    )
    .await
    .unwrap();

    let db_public = e.store.is_public("alice", "widget").await.unwrap();
    let repaired = e.store.reconcile_marker("alice", "widget", Kind::Repo, db_public).await.unwrap();
    assert!(repaired);
    let m = index::read(&e.store.os, Kind::Repo, "alice", "widget").await.unwrap();
    assert!(!m.public, "marker should have moved to private to match the DB");
}

/// The periodic lane: a repo nobody touches never reaches `open_repo`'s lazy repair, so the
/// renewal loop must walk what this node owns and repair it anyway — in both directions, and
/// without ever touching a repo this node does not hold (opening one elsewhere fences its owner).
#[tokio::test(flavor = "multi_thread")]
async fn owned_marker_lane_repairs_both_directions_and_skips_unowned() {
    use rustic_git_storage::index::{self, Kind, Marker};

    let marker = |name: &str, public: bool| Marker {
        name: name.into(),
        public,
        created_by: "alice@example.com".into(),
        created_ms: 111,
        description: String::new(),
        manifests: 0,
        updated_ms: 0,
    };

    let e = common::env().await;
    let app = common::app(e.store.clone()).await;

    // DB public, marker private (the "where did my public repos go" case).
    e.store.create_repo("alice", "up").await.unwrap();
    e.store.set_public("alice", "up", true).await.unwrap();
    index::write(&e.store, Kind::Repo, "alice", &marker("up", false)).await.unwrap();

    // DB private, marker public (the fail-closed direction).
    e.store.create_repo("alice", "down").await.unwrap();
    e.store.set_public("alice", "down", false).await.unwrap();
    index::write(&e.store, Kind::Repo, "alice", &marker("down", true)).await.unwrap();

    // Same drift, but this node does not hold the database open — the lane must leave it alone.
    e.store.create_repo("alice", "elsewhere").await.unwrap();
    e.store.set_public("alice", "elsewhere", true).await.unwrap();
    index::write(&e.store, Kind::Repo, "alice", &marker("elsewhere", false)).await.unwrap();
    e.store.pool.evict("alice", "elsewhere").await;

    rustic_git_server::lanes::reconcile_owned_markers(&app).await;

    let m = index::read(&e.store.os, Kind::Repo, "alice", "up").await.unwrap();
    assert!(m.public, "an owned repo whose DB says public must be republished to the listing");
    let m = index::read(&e.store.os, Kind::Repo, "alice", "down").await.unwrap();
    assert!(!m.public, "an owned repo whose DB says private must be pulled from the listing");
    let m = index::read(&e.store.os, Kind::Repo, "alice", "elsewhere").await.unwrap();
    assert!(!m.public, "a repo this node does not own must not be touched by the lane");
}

/// A warm workspace volume (`vol/owner/id` in the same pool) is neither a repo nor an image:
/// the lane must skip it, not publish a listing marker for a git repo owned by `vol`.
#[tokio::test(flavor = "multi_thread")]
async fn owned_marker_lane_skips_warm_volumes() {
    use futures::TryStreamExt;
    let e = common::env().await;
    let app = common::app(e.store.clone()).await;
    e.store.pool.get("vol", "alice/ws-1").await.unwrap();

    rustic_git_server::lanes::reconcile_owned_markers(&app).await;

    let keys: Vec<String> = e
        .store
        .os
        .list(Some(&slatedb::object_store::path::Path::from("index")))
        .map_ok(|m| m.location.to_string())
        .try_collect()
        .await
        .unwrap();
    assert!(
        keys.iter().all(|k| !k.contains("/vol/")),
        "a warm volume must not get a repo listing marker: {keys:?}"
    );
}

/// Create and description edit must land in the repo's OWN database, not just the Mongo index:
/// that database is what Task 4 onward reads, and a create that skipped it would leave a repo
/// whose description exists only in a listing row.
#[tokio::test(flavor = "multi_thread")]
async fn create_and_edit_write_repo_meta() {
    let e = common::env().await;
    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());

    let s = post_as(
        &router,
        "alice",
        "/api/alice/widget/create?description=first%20cut&created_by=alice%40example.com&created_at_ms=1234567890",
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let m = e.store.repo_meta("alice", "widget").await.unwrap().expect("meta after create");
    assert_eq!(m.description, "first cut");
    assert_eq!(m.created_by, "alice@example.com");
    assert_eq!(m.created_at_ms, 1234567890);

    let s = post_as(&router, "alice", "/api/alice/widget/description?description=second%20cut").await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let m = e.store.repo_meta("alice", "widget").await.unwrap().expect("meta after edit");
    assert_eq!(m.description, "second cut");
    // The edit touches ONE key: creator and creation time are not the editor's to rewrite.
    assert_eq!(m.created_by, "alice@example.com");
    assert_eq!(m.created_at_ms, 1234567890);

    // A description edit on a repo that does not exist must not conjure a database for it.
    let s = post_as(&router, "alice", "/api/alice/ghost/description?description=x").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

// ── pull requests on the owning node ────────────────────────────────────────

/// POST with a JSON body, as `as_owner`. The two PR write routes that carry real user text
/// (`open` and `comments`) take one, exactly as the api tier's own endpoints already do.
async fn post_json_as(
    router: &axum::Router,
    as_owner: &str,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header(rustic_git_core::peer::PEER_HEADER, "test-peer-secret")
        .header(rustic_git_core::peer::OWNER_HEADER, as_owner)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let r = router.clone().oneshot(req).await.unwrap();
    let status = r.status();
    let bytes = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

async fn open_pr(router: &axum::Router, path: &str, title: &str, head: &str) -> (StatusCode, serde_json::Value) {
    post_json_as(
        router,
        "alice",
        path,
        serde_json::json!({ "title": title, "body": "why", "base": "main", "head": head }),
    )
    .await
}

/// The whole point of moving pull requests into the repo's own database: a change opened through
/// the routed handler is readable through the routed get, and shows up in the routed list. If any
/// of the three disagreed the repo would not be carrying its own truth.
#[tokio::test(flavor = "multi_thread")]
async fn a_pull_opened_on_the_owner_is_readable_and_listed() {
    let e = common::env().await;
    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());
    assert_eq!(post_as(&router, "alice", "/api/alice/widget/create").await, StatusCode::CREATED);

    let (s, pr) = open_pr(&router, "/api/alice/widget/pulls", "fix the thing", "fix-it").await;
    assert_eq!(s, StatusCode::CREATED, "{pr}");
    assert_eq!(pr["number"], 1);
    assert_eq!(pr["state"], "open");
    assert_eq!(pr["repo"], "alice/widget");

    let (s, got) = get_as(&router, "alice", "/api/alice/widget/pulls/1").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(got["title"], "fix the thing");
    assert_eq!(got["head"], "fix-it");

    let (s, list) = get_as(&router, "alice", "/api/alice/widget/pulls").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(list.as_array().expect("a list").len(), 1);
    assert_eq!(list[0]["number"], 1);

    // A number nobody opened is a plain 404, not an empty body the page would render as a PR.
    let (s, _) = get_as(&router, "alice", "/api/alice/widget/pulls/9").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

/// The number IS the key, so two changes handed the same one would overwrite each other.
#[tokio::test(flavor = "multi_thread")]
async fn opening_twice_allocates_sequential_numbers() {
    let e = common::env().await;
    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());
    assert_eq!(post_as(&router, "alice", "/api/alice/widget/create").await, StatusCode::CREATED);

    let (_, one) = open_pr(&router, "/api/alice/widget/pulls", "first", "a").await;
    let (_, two) = open_pr(&router, "/api/alice/widget/pulls", "second", "b").await;
    assert_eq!(one["number"], 1);
    assert_eq!(two["number"], 2);
    let (_, list) = get_as(&router, "alice", "/api/alice/widget/pulls").await;
    assert_eq!(list.as_array().unwrap().len(), 2);
}

/// A repo whose changes predate this move must show them on the FIRST list — that is what
/// `ensure_migrated` in every handler buys, and without it the cutover makes every existing pull
/// request vanish from the page.
#[tokio::test(flavor = "multi_thread")]
async fn a_pre_existing_pull_appears_on_the_first_list() {
    let e = common::env().await;
    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());
    assert_eq!(post_as(&router, "alice", "/api/alice/widget/create").await, StatusCode::CREATED);

    // Stand in for Mongo through the injectable row source, so this proves the handler's
    // migration step without a live directory.
    let old = rustic_git_pulls::pulls::PullRequest {
        id: "alice/widget#4".into(),
        repo: "alice/widget".into(),
        number: 4,
        title: "from before".into(),
        body: String::new(),
        base: "main".into(),
        head: "old".into(),
        state: rustic_git_pulls::pulls::PullState::Open,
        author: "alice@example.com".into(),
        created_at_ms: 1,
        merged_at_ms: None,
        comments: Vec::new(),
        merge: None,
        mergeability: None,
        check_at_ms: None,
    };
    rustic_git_pulls::pulls::migrate_from(&e.store, "alice", "widget", || async { Ok(vec![old]) })
        .await
        .unwrap();

    let (s, list) = get_as(&router, "alice", "/api/alice/widget/pulls").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["number"], 4);
    // The next number must clear the migrated ones, or opening one would overwrite PR 4.
    let (_, next) = open_pr(&router, "/api/alice/widget/pulls", "after", "new").await;
    assert_eq!(next["number"], 5);
}

/// The leak test. These routes are peer-only, but "peer-only" is not "public": the api tier
/// forwards on behalf of a human and names them in `OWNER_HEADER`, so a private repo's changes
/// must be as invisible here as its refs are.
#[tokio::test(flavor = "multi_thread")]
async fn a_private_repos_pulls_are_invisible_to_a_stranger() {
    let e = common::env().await;
    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());
    assert_eq!(post_as(&router, "alice", "/api/alice/widget/create").await, StatusCode::CREATED);
    let (s, _) = open_pr(&router, "/api/alice/widget/pulls", "secret work", "wip").await;
    assert_eq!(s, StatusCode::CREATED);

    let (s, body) = get_as(&router, "bob", "/api/alice/widget/pulls").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "a stranger listed a private repo's changes: {body}");
    let (s, body) = get_as(&router, "bob", "/api/alice/widget/pulls/1").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "a stranger read a private repo's change: {body}");

    // And once it is public, the same caller may read it — otherwise this test would pass on a
    // handler that simply refuses everyone.
    assert_eq!(
        post_as(&router, "alice", "/api/alice/widget/visibility?visibility=public").await,
        StatusCode::NO_CONTENT
    );
    let (s, list) = get_as(&router, "bob", "/api/alice/widget/pulls").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
}

/// The list is for a page that renders a COUNT — shipping every comment body to
/// draw a number is what this asserts away. The detail route keeps them.
#[tokio::test(flavor = "multi_thread")]
async fn the_pull_list_carries_a_comment_count_not_the_comments() {
    let e = common::env().await;
    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());
    assert_eq!(post_as(&router, "alice", "/api/alice/widget/create").await, StatusCode::CREATED);
    assert_eq!(open_pr(&router, "/api/alice/widget/pulls", "t", "topic").await.0, StatusCode::CREATED);
    let (s, _) = post_json_as(
        &router,
        "alice",
        "/api/alice/widget/pulls/1/comments",
        serde_json::json!({ "body": "looks fine", "author": "b@example.com" }),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    let (status, list) = get_as(&router, "alice", "/api/alice/widget/pulls?state=open&limit=10").await;
    assert_eq!(status, StatusCode::OK);
    let row = &list.as_array().unwrap()[0];
    assert_eq!(row["commentCount"], 1);
    assert!(row.get("comments").is_none(), "the array must not travel with the list");

    let (_, closed) = get_as(&router, "alice", "/api/alice/widget/pulls?state=merged").await;
    assert_eq!(closed.as_array().unwrap().len(), 0, "state filters");

    let (_, detail) = get_as(&router, "alice", "/api/alice/widget/pulls/1").await;
    assert!(detail.get("comments").is_some(), "the detail keeps full comments");
}

/// Catches: `api_pulls`/`api_pull` handing the RAW path name to `ready()`, which opened a ghost
/// database `repo/alice/widget.git` — a key no routing ever names, on whichever node got asked.
#[tokio::test(flavor = "multi_thread")]
async fn a_dot_git_suffix_browse_read_creates_no_ghost_database() {
    let e = common::env().await;
    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());
    assert_eq!(post_as(&router, "alice", "/api/alice/widget/create").await, StatusCode::CREATED);
    let (s, list) = get_as(&router, "alice", "/api/alice/widget.git/pulls").await;
    assert_eq!(s, StatusCode::OK, "{list}");
    let (s, _) = get_as(&router, "alice", "/api/alice/widget.git/pulls/1").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(
        !e.store.repo_db_exists("alice", "widget.git").await.unwrap(),
        "a `.git`-suffixed read conjured a database under an unrouted key"
    );
}

/// A comment is an append to the change's own row; the next read must show it, because the row
/// IS the record now.
#[tokio::test(flavor = "multi_thread")]
async fn a_comment_appends_and_is_visible_on_the_next_read() {
    let e = common::env().await;
    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());
    assert_eq!(post_as(&router, "alice", "/api/alice/widget/create").await, StatusCode::CREATED);
    assert_eq!(open_pr(&router, "/api/alice/widget/pulls", "t", "h").await.0, StatusCode::CREATED);

    let (s, _) = post_json_as(
        &router,
        "alice",
        "/api/alice/widget/pulls/1/comments",
        serde_json::json!({ "body": "looks good", "author": "bob@example.com" }),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    let (_, pr) = get_as(&router, "alice", "/api/alice/widget/pulls/1").await;
    assert_eq!(pr["comments"].as_array().unwrap().len(), 1);
    assert_eq!(pr["comments"][0]["body"], "looks good");
    assert_eq!(pr["comments"][0]["author"], "bob@example.com");

    // An empty comment is the caller's mistake, not a row to store.
    let (s, _) = post_json_as(
        &router,
        "alice",
        "/api/alice/widget/pulls/1/comments",
        serde_json::json!({ "body": "  " }),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

/// Merge and close are state transitions, and asking twice must not queue the work twice or
/// re-close a closed change.
#[tokio::test(flavor = "multi_thread")]
async fn merge_then_close_are_each_answered_once() {
    let e = common::env().await;
    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());
    assert_eq!(post_as(&router, "alice", "/api/alice/widget/create").await, StatusCode::CREATED);
    assert_eq!(open_pr(&router, "/api/alice/widget/pulls", "t", "h").await.0, StatusCode::CREATED);

    let s = post_as(&router, "alice", "/api/alice/widget/pulls/1/merge?strategy=squash").await;
    assert_eq!(s, StatusCode::ACCEPTED);
    // Deterministic again now that the 202 only records and announces: this node performs no
    // merge, so the job is still Queued when the second ask arrives.
    let s = post_as(&router, "alice", "/api/alice/widget/pulls/1/merge?strategy=merge").await;
    assert_eq!(s, StatusCode::CONFLICT, "asking twice must not queue it twice");
    let s = post_as(&router, "alice", "/api/alice/widget/pulls/1/merge?strategy=nonsense").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // `check` is a nudge, and its answer is the list of changes the owner could NOT settle from
    // ancestry — the worker's queue. This repo has no branches at all, so the honest verdict is
    // that one of them is gone, and there is nothing to hand over.
    let s = post_as(&router, "alice", "/api/alice/widget/pulls/1/check").await;
    assert_eq!(s, StatusCode::OK);
    let (_, pr) = get_as(&router, "alice", "/api/alice/widget/pulls/1").await;
    assert_eq!(pr["mergeability"]["state"], "unknown");
    assert_eq!(pr["mergeability"]["detail"], "one of the branches is gone");

    assert_eq!(post_as(&router, "alice", "/api/alice/widget/pulls/1/close").await, StatusCode::NO_CONTENT);
    assert_eq!(
        post_as(&router, "alice", "/api/alice/widget/pulls/1/close").await,
        StatusCode::CONFLICT,
        "a closed change was closed again"
    );
    let (_, pr) = get_as(&router, "alice", "/api/alice/widget/pulls/1").await;
    assert_eq!(pr["state"], "closed");
}

/// The api tier no longer holds pull requests — it forwards, so the bodies and query strings it
/// sends ARE the contract. This drives a whole change through the exact payloads `src/api.rs`
/// now sends (`author` in the body, `by=` on the state transitions) and asserts the identity it
/// names is what gets recorded, because the owning node has no other way to learn who is acting.
///
/// It also pins the field names the web app reads (`web/apps/web/src/lib/api.ts`): the move of
/// storage must not become a rename.
#[tokio::test(flavor = "multi_thread")]
async fn a_forwarded_lifecycle_records_the_caller_the_api_tier_names() {
    let e = common::env().await;
    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());
    assert_eq!(post_as(&router, "alice", "/api/alice/widget/create").await, StatusCode::CREATED);

    let (s, pr) = post_json_as(
        &router,
        "alice",
        "/api/alice/widget/pulls",
        serde_json::json!({"title":"fix it","body":"why","base":"main","head":"fix-it","author":"k@example.com"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    // The shape the web app deserialises, field by field.
    assert_eq!(pr["_id"], "alice/widget#1");
    assert_eq!(pr["repo"], "alice/widget");
    assert_eq!(pr["number"], 1);
    assert_eq!(pr["title"], "fix it");
    assert_eq!(pr["body"], "why");
    assert_eq!(pr["base"], "main");
    assert_eq!(pr["head"], "fix-it");
    assert_eq!(pr["state"], "open");
    assert_eq!(pr["author"], "k@example.com", "the api tier's caller, not the peer secret");
    assert!(pr["createdAt"].is_number());
    assert!(pr["comments"].is_array());

    let (s, _) = post_json_as(
        &router,
        "alice",
        "/api/alice/widget/pulls/1/comments",
        serde_json::json!({"body":"looks good","author":"k@example.com"}),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    let s = post_as(
        &router,
        "alice",
        "/api/alice/widget/pulls/1/merge?strategy=squash&by=k%40example.com",
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED);

    let (_, pr) = get_as(&router, "alice", "/api/alice/widget/pulls/1").await;
    assert_eq!(pr["comments"][0]["author"], "k@example.com");
    assert_eq!(pr["comments"][0]["body"], "looks good");
    assert!(pr["comments"][0]["at"].is_number() || pr["comments"][0]["atMs"].is_number());
    assert_eq!(pr["merge"]["strategy"], "squash");
    assert_eq!(pr["merge"]["requestedBy"], "k@example.com");

    let s = post_as(&router, "alice", "/api/alice/widget/pulls/1/close?by=k%40example.com").await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, list) = get_as(&router, "alice", "/api/alice/widget/pulls").await;
    assert_eq!(list[0]["state"], "closed");
    assert_eq!(list[0]["number"], 1);
}

/// A valid token presented under someone else's username is a 401, not a silent downgrade to an
/// anonymous request that then 401s for a different reason — and not a success. git's own
/// placeholder (`https://x:<token>@host`) still works, because it names nobody.
#[tokio::test(flavor = "multi_thread")]
async fn a_basic_username_that_is_not_the_owner_is_refused() {
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let token = e.store.create_token("alice").await.unwrap();
    let router = rustic_git_server::router::router(common::app(e.store.clone()).await, common::no_jobs_state());
    let get = |user: &'static str, token: String| {
        let router = router.clone();
        async move {
            use base64::Engine;
            let cred = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{token}"));
            let req = Request::builder()
                .uri("/alice/web.git/info/refs?service=git-upload-pack")
                .header("git-protocol", "version=2")
                .header("authorization", format!("Basic {cred}"))
                .body(axum::body::Body::empty())
                .unwrap();
            router.oneshot(req).await.unwrap().status()
        }
    };
    assert_eq!(get("x", token.clone()).await, StatusCode::OK);
    assert_eq!(get("alice", token.clone()).await, StatusCode::OK);
    assert_eq!(get("mallory", token).await, StatusCode::UNAUTHORIZED);
}

/// The public listener: a token that was revoked is a 401, not the 403 a stranger gets — the
/// client must learn to present a different credential, not that this repo is closed to it.
#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_token_is_refused_on_the_public_listener() {
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let token = e.store.create_token("alice").await.unwrap();
    let router = rustic_git_server::router::router(common::app(e.store.clone()).await, common::no_jobs_state());
    let get = |token: String| {
        let router = router.clone();
        async move {
            let req = Request::builder()
                .uri("/alice/web.git/info/refs?service=git-upload-pack")
                .header("git-protocol", "version=2")
                .header("authorization", {
                    use base64::Engine;
                    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("x:{token}")))
                })
                .body(axum::body::Body::empty())
                .unwrap();
            router.oneshot(req).await.unwrap().status()
        }
    };
    assert_eq!(get(token.clone()).await, StatusCode::OK);
    e.store.revoke_token_digest(&rustic_git_storage::store::Store::token_digest(&token)).await.unwrap();
    assert_eq!(get(token).await, StatusCode::UNAUTHORIZED);
}

/// The worker's three endpoints. They hand out work and write outcomes, so the peer secret alone
/// is not enough — the caller must also assert an identity, and it must be the repo's owner. A
/// stranger with the secret (a leaked one, or a peer acting for someone else) gets nothing.
#[tokio::test(flavor = "multi_thread")]
async fn the_worker_endpoints_are_peer_and_owner_only() {
    let e = common::env().await;
    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());
    assert_eq!(post_as(&router, "alice", "/api/alice/widget/create").await, StatusCode::CREATED);
    assert_eq!(open_pr(&router, "/api/alice/widget/pulls", "t", "h").await.0, StatusCode::CREATED);

    for tail in ["claim", "outcome", "mergeability"] {
        let path = format!("/api/alice/widget/pulls/1/{tail}");
        // No secret at all: refused by the listener itself, before any handler.
        let req = axum::http::Request::builder()
            .method("POST")
            .uri(&path)
            .body(axum::body::Body::empty())
            .unwrap();
        let s = tower::ServiceExt::oneshot(router.clone(), req).await.unwrap().status();
        assert_eq!(s, StatusCode::FORBIDDEN, "{tail} without the peer secret");

        // The secret, but acting as someone else's owner.
        assert_eq!(
            post_json(&router, "bob", &path, serde_json::json!({})).await,
            StatusCode::FORBIDDEN,
            "{tail} as the wrong owner"
        );
    }

    // With both, the routes work: nothing is queued, so the claim is a 409 rather than a 404 —
    // it IS routable and it DID reach the handler.
    assert_eq!(
        post_as(&router, "alice", "/api/alice/widget/pulls/1/claim").await,
        StatusCode::CONFLICT
    );
    assert_eq!(
        post_json(
            &router,
            "alice",
            "/api/alice/widget/pulls/1/mergeability",
            serde_json::json!({"state": "clean", "fastForward": false}),
        )
        .await,
        StatusCode::NO_CONTENT
    );
}

async fn post_json(
    router: &axum::Router,
    owner: &str,
    path: &str,
    body: serde_json::Value,
) -> StatusCode {
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header(rustic_git_core::peer::PEER_HEADER, "test-peer-secret")
        .header(rustic_git_core::peer::OWNER_HEADER, owner)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    tower::ServiceExt::oneshot(router.clone(), req).await.unwrap().status()
}

/// Compression mounts on the browse router alone: git packs and registry blobs
/// are already compressed and must not pay for a second pass. The pull's long
/// body pushes the response past the compressor's minimum-size predicate.
#[tokio::test(flavor = "multi_thread")]
async fn browse_json_is_gzipped_when_asked_for() {
    let e = common::env().await;
    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());
    assert_eq!(post_as(&router, "alice", "/api/alice/widget/create").await, StatusCode::CREATED);
    let body = serde_json::json!({
        "title": "long enough to clear the size-above predicate ".repeat(4),
        "body": "",
        "base": "refs/heads/main",
        "head": "refs/heads/topic",
        "author": "a@example.com",
    });
    let (status, _) = post_json_as(&router, "alice", "/api/alice/widget/pulls", body).await;
    assert!(status.is_success(), "opening the pull: {status}");

    let req = Request::builder()
        .uri("/api/alice/widget/pulls")
        .header(rustic_git_core::peer::PEER_HEADER, "test-peer-secret")
        .header(rustic_git_core::peer::OWNER_HEADER, "alice")
        .header("accept-encoding", "gzip")
        .body(axum::body::Body::empty())
        .unwrap();
    let r = router.clone().oneshot(req).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        r.headers().get("content-encoding").and_then(|v| v.to_str().ok()),
        Some("gzip"),
    );
}

/// The Snapshots page's two reads, against a volume that exists ONLY as registry records — no
/// `Workspace`, no `Environment`, no `SnapshotRequest` anywhere. That is the whole point: a
/// snapshot outlives its parent, and this is the index that survives it.
#[tokio::test(flavor = "multi_thread")]
async fn volumes_and_history_read_without_any_cluster_object() {
    use rustic_git_workspaces::registry::{CommitRecord, VolExt};

    let e = common::env().await;
    let rec = |id: &str, msg: &str, at: chrono::DateTime<chrono::Utc>| CommitRecord {
        id: id.to_string(),
        state: serde_json::json!({"kind": "workspace", "name": "api-scratch"}),
        lineage: vec![],
        region: "centralindia".into(),
        message: Some(msg.to_string()),
        created_at: at,
    };
    let now = chrono::Utc::now();
    e.store
        .append_commits(
            "alice",
            "ws-1",
            &[rec("c1", "first", now - chrono::Duration::hours(1)), rec("c2", "second", now)],
        )
        .await
        .unwrap();

    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());

    let (status, body) = get_as(&router, "alice", "/api/alice/volumes").await;
    assert_eq!(status, StatusCode::OK);
    let rows = body.as_array().expect("a list");
    assert_eq!(rows.len(), 1, "one pushed volume: {body}");
    assert_eq!(rows[0]["name"], "ws-1");
    assert!(rows[0]["latest_ms"].as_i64().is_some_and(|m| m > 0), "dated from the prefix: {body}");

    // Newest first, and the provenance the list page shows rides in `state`.
    let (status, body) = get_as(&router, "alice", "/api/alice/ws-1/volumehistory").await;
    assert_eq!(status, StatusCode::OK);
    let hist = body.as_array().expect("a list");
    assert_eq!(hist.len(), 2);
    assert_eq!(hist[0]["id"], "c2");
    assert_eq!(hist[1]["id"], "c1");
    assert_eq!(hist[0]["state"]["name"], "api-scratch");
    assert_eq!(hist[0]["state"]["kind"], "workspace");

    // Someone else's volume is not theirs to list or read.
    assert_eq!(get_as(&router, "bob", "/api/alice/volumes").await.0, StatusCode::NOT_FOUND);
    assert_eq!(get_as(&router, "bob", "/api/alice/ws-1/volumehistory").await.0, StatusCode::NOT_FOUND);

    // An owner with nothing pushed gets an empty list, not an error.
    let (status, body) = get_as(&router, "carol", "/api/carol/volumes").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().map(|a| a.len()), Some(0));
}

/// The listing is two owner-scoped LISTs — the database directory names and the push markers —
/// never a walk of every database's objects. A volume pushed before the marker existed is still
/// named (the directory is there), just undated; one whose marker is present is dated by it.
#[tokio::test(flavor = "multi_thread")]
async fn the_volume_listing_reads_markers_not_database_objects() {
    use rustic_git_workspaces::registry::{volume_marker, CommitRecord, VolExt};
    use slatedb::object_store::ObjectStoreExt;

    let e = common::env().await;
    let rec = |id: &str| CommitRecord {
        id: id.to_string(),
        state: serde_json::Value::Null,
        lineage: vec![],
        region: "centralindia".into(),
        message: None,
        created_at: chrono::Utc::now(),
    };
    e.store.append_commits("alice", "ws-dated", &[rec("c1")]).await.unwrap();
    e.store.append_commits("alice", "ws-legacy", &[rec("c2")]).await.unwrap();
    // A volume from before markers existed: its records are there, its marker is not.
    e.store.os.delete(&volume_marker("alice", "ws-legacy")).await.unwrap();

    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());
    let (status, body) = get_as(&router, "alice", "/api/alice/volumes").await;
    assert_eq!(status, StatusCode::OK);
    let rows = body.as_array().expect("a list");
    assert_eq!(rows.len(), 2, "{body}");
    let row = |n: &str| rows.iter().find(|r| r["name"] == n).unwrap_or_else(|| panic!("{n} missing: {body}"));
    assert!(row("ws-dated")["latest_ms"].as_i64().is_some_and(|m| m > 0), "dated by its marker: {body}");
    assert!(row("ws-legacy")["latest_ms"].is_null(), "named by its directory, undated: {body}");
}

/// Opening a SlateDB CREATES it, so a history read of a name nobody has pushed must be refused
/// BEFORE the open — otherwise probing invents a volume that the owner-scoped listing then shows
/// forever, with no history behind it.
#[tokio::test(flavor = "multi_thread")]
async fn a_history_read_of_an_unknown_volume_creates_nothing() {
    use futures::StreamExt;
    use slatedb::object_store::ObjectStore;

    let e = common::env().await;
    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());

    let (status, _) = get_as(&router, "alice", "/api/alice/never-pushed/volumehistory").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let prefix = slatedb::object_store::path::Path::from("repo/vol");
    let found: Vec<_> = e.store.os.list(Some(&prefix)).collect::<Vec<_>>().await;
    assert!(found.is_empty(), "the probe minted a volume: {found:?}");

    // And the listing still shows nothing, which is the consequence that would have been permanent.
    let (status, body) = get_as(&router, "alice", "/api/alice/volumes").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().map(|a| a.len()), Some(0), "{body}");
}

/// Deleting a volume drops every commit record and every ref, and is scoped exactly as the history
/// read is — someone else's volume is a 404, not a 403, and nothing is touched.
///
/// The layer BLOBS are deliberately NOT deleted here; see `browse_api::volumes::volumedelete` for
/// why (a clone or a restore makes two volumes reference one blob id, and this node may not open
/// the other's database to find out).
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_volume_drops_its_records_and_refs_and_is_owner_scoped() {
    use rustic_git_workspaces::registry::{CommitRecord, VolExt};

    let e = common::env().await;
    let rec = |id: &str| CommitRecord {
        id: id.to_string(),
        state: serde_json::json!({"kind": "environment", "name": "staging"}),
        lineage: vec![],
        region: "centralindia".into(),
        message: None,
        created_at: chrono::Utc::now(),
    };
    e.store.append_commits("alice", "env-1", &[rec("c1"), rec("c2")]).await.unwrap();
    assert!(e.store.move_ref("alice", "env-1", "main", "c2").await.unwrap());

    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());

    // Not bob's to delete, and the refusal must leave everything where it was.
    let req = |as_owner: &str| {
        Request::builder()
            .method("DELETE")
            .uri("/api/alice/env-1/volumedelete")
            .header(rustic_git_core::peer::PEER_HEADER, "test-peer-secret")
            .header(rustic_git_core::peer::OWNER_HEADER, as_owner)
            .body(axum::body::Body::empty())
            .unwrap()
    };
    assert_eq!(router.clone().oneshot(req("bob")).await.unwrap().status(), StatusCode::NOT_FOUND);
    assert_eq!(e.store.history("alice", "env-1").await.unwrap().len(), 2, "bob's 404 deleted nothing");

    assert_eq!(router.clone().oneshot(req("alice")).await.unwrap().status(), StatusCode::NO_CONTENT);
    assert!(e.store.history("alice", "env-1").await.unwrap().is_empty(), "records gone");
    assert_eq!(e.store.ref_commit("alice", "env-1", "main").await.unwrap(), None, "ref gone");

    // And the page that reads it now shows nothing behind that volume.
    let (status, body) = get_as(&router, "alice", "/api/alice/env-1/volumehistory").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().map(|a| a.len()), Some(0), "{body}");
}

/// Same guard as the history read, for the same reason: a delete of a name nobody pushed must be
/// refused BEFORE the open, or the probe mints the ghost volume it was asked to remove.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_an_unknown_volume_creates_nothing() {
    use futures::StreamExt;
    use slatedb::object_store::ObjectStore;

    let e = common::env().await;
    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());
    let req = Request::builder()
        .method("DELETE")
        .uri("/api/alice/never-pushed/volumedelete")
        .header(rustic_git_core::peer::PEER_HEADER, "test-peer-secret")
        .header(rustic_git_core::peer::OWNER_HEADER, "alice")
        .body(axum::body::Body::empty())
        .unwrap();
    assert_eq!(router.clone().oneshot(req).await.unwrap().status(), StatusCode::NOT_FOUND);

    let prefix = slatedb::object_store::path::Path::from("repo/vol");
    let found: Vec<_> = e.store.os.list(Some(&prefix)).collect::<Vec<_>>().await;
    assert!(found.is_empty(), "the probe minted a volume: {found:?}");
}

/// Deleting ONE snapshot: the other records survive, and a delete of the record `main` points at
/// walks the ref back to the next-newest rather than leaving it naming something that is gone.
/// An unknown id is a 404 that changes nothing — the record set and the ref are both untouched.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_one_snapshot_keeps_the_rest_and_walks_the_ref_back() {
    use rustic_git_workspaces::registry::{CommitRecord, VolExt};

    let e = common::env().await;
    let now = chrono::Utc::now();
    let rec = |id: &str, ago: i64| CommitRecord {
        id: id.to_string(),
        state: serde_json::json!({"kind": "environment", "name": "staging"}),
        lineage: vec![],
        region: "centralindia".into(),
        message: None,
        created_at: now - chrono::Duration::hours(ago),
    };
    e.store
        .append_commits("alice", "env-1", &[rec("c1", 2), rec("c2", 1), rec("c3", 0)])
        .await
        .unwrap();
    assert!(e.store.move_ref("alice", "env-1", "main", "c3").await.unwrap());

    let router = rustic_git_server::router::peer_router(common::app(e.store.clone()).await, common::no_jobs_state());
    let req = |as_owner: &str, id: &str| {
        Request::builder()
            .method("DELETE")
            .uri(format!("/api/alice/env-1/snapshotdelete/{id}"))
            .header(rustic_git_core::peer::PEER_HEADER, "test-peer-secret")
            .header(rustic_git_core::peer::OWNER_HEADER, as_owner)
            .body(axum::body::Body::empty())
            .unwrap()
    };

    // Not bob's, and an unknown id: both 404, both side-effect free.
    assert_eq!(router.clone().oneshot(req("bob", "c2")).await.unwrap().status(), StatusCode::NOT_FOUND);
    assert_eq!(router.clone().oneshot(req("alice", "nope")).await.unwrap().status(), StatusCode::NOT_FOUND);
    assert_eq!(e.store.history("alice", "env-1").await.unwrap().len(), 3);
    assert_eq!(e.store.ref_commit("alice", "env-1", "main").await.unwrap().as_deref(), Some("c3"));

    // A middle record: the others stay, and the tip ref does not move.
    assert_eq!(router.clone().oneshot(req("alice", "c2")).await.unwrap().status(), StatusCode::NO_CONTENT);
    let (status, body) = get_as(&router, "alice", "/api/alice/env-1/volumehistory").await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = body.as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["c3", "c1"], "{body}");
    assert_eq!(e.store.ref_commit("alice", "env-1", "main").await.unwrap().as_deref(), Some("c3"));

    // A ref parked on an OLDER record walks back, never forward: `main` on c1 while c3 is still
    // the newest must land on nothing, not jump up the list.
    assert!(e.store.move_ref("alice", "env-1", "main", "c1").await.unwrap());
    assert_eq!(router.clone().oneshot(req("alice", "c1")).await.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(e.store.ref_commit("alice", "env-1", "main").await.unwrap(), None, "walked back, not forward");

    // The last one standing: nothing left to point at, so the ref goes rather than dangling.
    assert!(e.store.move_ref("alice", "env-1", "main", "c3").await.unwrap());
    assert_eq!(router.clone().oneshot(req("alice", "c3")).await.unwrap().status(), StatusCode::NO_CONTENT);
    assert!(e.store.history("alice", "env-1").await.unwrap().is_empty());
    assert_eq!(e.store.ref_commit("alice", "env-1", "main").await.unwrap(), None);
}
