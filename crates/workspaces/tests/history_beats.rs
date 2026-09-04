//! The hourly folds. Usage is recomputed from the CRDs on every run and never derived from an
//! earlier row (CLAUDE.md), so what is tested here is the ROW SHAPE — six dimensions per owner,
//! every one carrying both the used value and the limit it was measured against.

use rustic_git_workspaces::crd::QuotaSpec;
use rustic_git_workspaces::history::beats::{fleet_rows, usage_rows, FleetInput, UsageInput};
use rustic_git_workspaces::quota::Usage;

fn ts() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-09-04T10:00:00Z").unwrap().into()
}

#[test]
fn one_row_per_owner_per_dimension_with_its_limit() {
    let rows = usage_rows(
        ts(),
        &[UsageInput {
            owner: "acme".into(),
            is_team: true,
            used: Usage { workspaces: 3, environments: 1, snapshots: 9, disk_gb: 120, cpu: 6, memory_gb: 24 },
            limit: QuotaSpec { workspaces: 10, environments: 4, snapshots: 50, disk_gb: 500, cpu: 16, memory_gb: 64 },
        }],
    );
    assert_eq!(rows.len(), 6, "six dimensions, one row each");
    assert_eq!(rows[0]["ts"], serde_json::json!("2026-09-04 10:00:00"));
    let ws = rows.iter().find(|r| r["dimension"] == "workspaces").unwrap();
    assert_eq!(ws["owner"], serde_json::json!("acme"));
    // A team is `1`, a person `0`: the column is UInt8, not a Bool.
    assert_eq!(ws["is_team"], serde_json::json!(1));
    assert_eq!(ws["used"], serde_json::json!(3.0));
    assert_eq!(ws["limit"], serde_json::json!(10.0));
    // The dimension words are `Dim::word`'s, which the 409 message and the request form already
    // key off — a second vocabulary here would silently split every chart in two.
    let mut dims: Vec<&str> = rows.iter().map(|r| r["dimension"].as_str().unwrap()).collect();
    dims.sort_unstable();
    assert_eq!(dims, ["cpu", "diskGb", "environments", "memoryGb", "snapshots", "workspaces"]);
}

#[test]
fn a_fleet_row_carries_every_column_the_table_declares() {
    let rows = fleet_rows(
        ts(),
        &[FleetInput {
            region: "westeurope-k3s".into(),
            nodes_total: 3,
            nodes_ready: 2,
            agents_ready: 2,
            live_workspaces: 7,
            live_environments: 2,
            snapshots: 41,
            disk_gb: 900,
            cpu: 24,
            memory_gb: 96,
            pool_used_bytes: 500_000_000_000,
            pool_total_bytes: 1_000_000_000_000,
        }],
    );
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    for col in [
        "ts",
        "region",
        "nodes_total",
        "nodes_ready",
        "agents_ready",
        "live_workspaces",
        "live_environments",
        "snapshots",
        "disk_gb",
        "cpu",
        "memory_gb",
        "pool_used_bytes",
        "pool_total_bytes",
    ] {
        assert!(r.get(col).is_some(), "fleet row is missing {col}");
    }
    assert_eq!(r["nodes_ready"], serde_json::json!(2));
    assert_eq!(r["pool_total_bytes"], serde_json::json!(1_000_000_000_000u64));
}

/// No owners is a legitimate hour (a brand-new cluster), and it must produce no rows rather than
/// a row of zeros that a chart would draw as a cliff.
#[test]
fn an_empty_fold_writes_nothing() {
    assert!(usage_rows(ts(), &[]).is_empty());
    assert!(fleet_rows(ts(), &[]).is_empty());
}
