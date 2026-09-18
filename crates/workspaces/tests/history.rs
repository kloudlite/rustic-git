use kloudlite_workspaces::history::events::EventRow;
use kloudlite_workspaces::history::outbox::{drain_outbox_once, enqueue_events, outbox_status};
use kloudlite_workspaces::history::History;
use slatedb::object_store::{memory::InMemory, ObjectStore};
use std::sync::{atomic::{AtomicUsize, Ordering}, Arc};

fn row(id: &str) -> EventRow {
    EventRow {
        ts: chrono::DateTime::parse_from_rfc3339("2026-09-18T00:00:00Z").unwrap().with_timezone(&chrono::Utc),
        id: id.to_string(),
        kind: "workspace.created".into(),
        actor: String::new(),
        owner: "alice".into(),
        target: "dev".into(),
        region: "central".into(),
        attrs: serde_json::json!({"state": "ready"}),
    }
}

#[tokio::test]
async fn outbox_survives_clickhouse_failure_and_replays_after_restart() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new()
        .fallback({
            let attempts = attempts.clone();
            move || {
                let attempts = attempts.clone();
                async move {
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR
                    } else {
                        axum::http::StatusCode::OK
                    }
                }
            }
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });

    let os: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let history = History::new(&url, "test", "test").unwrap().with_outbox(os.clone());
    enqueue_events(&history, &[row("stable")]).await.unwrap();
    assert_eq!(outbox_status(&history).await.unwrap().count, 1);

    assert!(drain_outbox_once(&history).await.is_err());
    assert_eq!(outbox_status(&history).await.unwrap().count, 1);

    let restarted = History::new(&url, "test", "test").unwrap().with_outbox(os);
    assert_eq!(drain_outbox_once(&restarted).await.unwrap(), 1);
    assert_eq!(outbox_status(&restarted).await.unwrap().count, 0);
}

#[tokio::test]
async fn duplicate_enqueue_keeps_one_immutable_event() {
    let os: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let history = History::new("http://127.0.0.1:1", "test", "test").unwrap().with_outbox(os);
    enqueue_events(&history, &[row("same")]).await.unwrap();
    enqueue_events(&history, &[row("same")]).await.unwrap();
    assert_eq!(outbox_status(&history).await.unwrap().count, 1);
}
