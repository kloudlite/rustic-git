//! The event row shape and the audit dual write. The object-store audit log stays the append-only
//! legal record; this is the queryable copy, and a failure to write the copy must never affect it.

use rustic_git_workspaces::history::events::{audit_event, EventRow};

#[test]
fn a_row_serializes_in_the_shape_the_events_table_takes() {
    let row = EventRow {
        ts: chrono::DateTime::parse_from_rfc3339("2026-09-04T10:11:12.345Z").unwrap().into(),
        id: "uid-1:4711:created".into(),
        kind: "workspace.created".into(),
        actor: "meera@example.com".into(),
        owner: "acme".into(),
        target: "ws-abc".into(),
        region: "westeurope-k3s".into(),
        attrs: serde_json::json!({"image": "alpine"}),
    };
    let v = row.to_json();
    // ClickHouse's DateTime64(3) over HTTP wants a space, not a `T`, and no zone suffix.
    assert_eq!(v["ts"], serde_json::json!("2026-09-04 10:11:12.345"));
    assert_eq!(v["kind"], serde_json::json!("workspace.created"));
    // `attrs` is a String column: the JSON goes in as text, not as a nested object.
    assert_eq!(v["attrs"], serde_json::json!(r#"{"image":"alpine"}"#));
}

/// The id is what makes at-least-once safe. Two writes of the same audit row must collapse.
#[test]
fn an_audit_event_id_is_deterministic() {
    let a = audit_event("2026-09-04T10:11:12Z", "root@example.com", "drain", "eu/node-1", "ok");
    let b = audit_event("2026-09-04T10:11:12Z", "root@example.com", "drain", "eu/node-1", "ok");
    assert_eq!(a.id, b.id);
    assert_eq!(a.kind, "admin.drain");
    assert_eq!(a.actor, "root@example.com");
    assert_eq!(a.target, "eu/node-1");
    assert_eq!(a.attrs["result"], serde_json::json!("ok"));
}

/// An audit entry that cannot be copied is still an audit entry: dropping it would make the
/// queryable copy silently incomplete, which is worse than a stamped-now timestamp.
#[test]
fn a_bad_timestamp_falls_back_to_now_rather_than_dropping_the_row() {
    let e = audit_event("not-a-timestamp", "root@example.com", "drain", "eu/node-1", "ok");
    assert_eq!(e.kind, "admin.drain");
    assert!(e.ts <= chrono::Utc::now());
}
