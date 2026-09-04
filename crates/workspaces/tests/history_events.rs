//! The event row shape and the audit dual write. The object-store audit log stays the append-only
//! legal record; this is the queryable copy, and a failure to write the copy must never affect it.

use rustic_git_workspaces::history::events::{audit_event, stream_event, EventRow};

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

fn field(k: &str, v: &str) -> (String, String) {
    (k.to_string(), v.to_string())
}

/// A PR event off the `events` stream becomes an event row. The stream entry id is the dedupe key:
/// Redis assigns it once, so a redelivered entry (XAUTOCLAIM after a crash) writes the same row.
#[test]
fn a_stream_entry_becomes_an_event_row_keyed_by_its_stream_id() {
    let fields = vec![
        field("kind", "pull_merged"),
        field("repo", "alice/web"),
        field("number", "7"),
        field("actor", "alice@example.com"),
        field("at_ms", "1788523872000"),
        field("title", "fix the thing"),
        field("base", "main"),
        field("head", "fix-it"),
    ];
    let e = stream_event("1788523872000-0", &fields).expect("a known kind must map");
    assert_eq!(e.id, "stream:1788523872000-0");
    assert_eq!(e.kind, "git.pull_merged");
    assert_eq!(e.owner, "alice");
    assert_eq!(e.target, "alice/web#7");
    assert_eq!(e.actor, "alice@example.com");
    assert_eq!(e.attrs["title"], serde_json::json!("fix the thing"));
    // The stream is a nudge about a repo, not about a region; `central` is where the record lives.
    assert_eq!(e.region, "central");
}

/// An unknown kind is skipped, never fatal — the same rule `storage::events::from_fields` follows.
/// A future producer must not be able to wedge this consumer.
#[test]
fn an_unknown_stream_kind_is_skipped() {
    let fields = vec![field("kind", "from_the_future"), field("repo", "a/b")];
    assert!(stream_event("1-0", &fields).is_none());
}

/// A repo with no owner segment must not panic or invent one.
#[test]
fn a_malformed_repo_yields_an_empty_owner_rather_than_a_panic() {
    let fields = vec![
        field("kind", "pull_opened"),
        field("repo", "noslash"),
        field("number", "1"),
        field("actor", "a@b.c"),
        field("at_ms", "0"),
    ];
    let e = stream_event("2-0", &fields).expect("a known kind must still map");
    assert_eq!(e.owner, "");
    assert_eq!(e.target, "noslash#1");
}

/// Every kind the git tier can publish must map, and the target shape must match what the kind
/// actually is: a PR event names a pull request, `head_moved` is repo-wide and names only the repo.
#[test]
fn every_git_kind_maps_with_the_target_shape_its_scope_implies() {
    for (kind, target) in [
        ("pull_opened", "alice/web#7"),
        ("pull_commented", "alice/web#7"),
        ("merge_requested", "alice/web#7"),
        ("pull_merged", "alice/web#7"),
        ("pull_closed", "alice/web#7"),
        // Repo-wide: `number` is a 0 marker, not a pull request to name.
        ("head_moved", "alice/web"),
    ] {
        let number = if kind == "head_moved" { "0" } else { "7" };
        let fields = vec![
            field("kind", kind),
            field("repo", "alice/web"),
            field("number", number),
            field("actor", "alice@example.com"),
            field("at_ms", "1788523872000"),
        ];
        let e = stream_event("3-0", &fields).unwrap_or_else(|| panic!("{kind} must map"));
        assert_eq!(e.kind, format!("git.{kind}"));
        assert_eq!(e.target, target, "target shape for {kind}");
    }
}
