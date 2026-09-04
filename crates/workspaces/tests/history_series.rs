//! The series catalogue. Each entry is one SQL statement, and what is asserted here is that the
//! statements are built from an ALLOW-LIST — a range, a step and a series name that came off the
//! wire must never reach a query as text.

use rustic_git_workspaces::history::series::{
    parse_range, parse_step, sql_for, summarize, SeriesQuery,
};

fn q() -> SeriesQuery {
    SeriesQuery {
        range: "7d".into(),
        step: "1h".into(),
        region: None,
        owner: None,
        dimension: None,
    }
}

const NAMES: &[&str] = &[
    "pending_requests",
    "firing_signals",
    "owners_over_80",
    "live_workspaces",
    "live_environments",
    "decided_requests",
    "time_to_decide_p50",
    "pool_used",
    "cpu_used",
    "memory_used",
    "restarts",
    "audit_events",
];

#[test]
fn every_named_series_the_console_asks_for_has_a_statement() {
    for name in NAMES {
        let sql = sql_for(name, &q()).unwrap_or_else(|| panic!("{name} has no statement"));
        assert!(sql.to_uppercase().starts_with("SELECT"), "{name}: {sql}");
        // Every series is a time series: two columns, ts first.
        assert!(sql.contains("ORDER BY"), "{name} must order its buckets: {sql}");
    }
}

/// Every series reads a table this plan actually creates, bucketed by the step the caller asked
/// for. A typo in a table name is a runtime 502 on a page nobody looks at until an incident.
#[test]
fn every_series_reads_a_known_table_and_buckets_by_the_step() {
    let tables = [
        "rustic.events",
        "rustic.usage_hourly",
        "rustic.fleet_hourly",
        "rustic.alerts",
        "rustic.metrics_5m",
    ];
    for name in NAMES {
        let sql = sql_for(name, &q()).unwrap();
        assert!(
            tables.iter().any(|t| sql.contains(t)),
            "{name} reads no known table: {sql}"
        );
        assert!(sql.contains("toStartOfHour"), "{name} ignores step: {sql}");
        assert!(
            sql_for(name, &SeriesQuery { step: "1d".into(), ..q() })
                .unwrap()
                .contains("toStartOfDay"),
            "{name} ignores a 1d step"
        );
        // A range that means something else must produce a different statement.
        assert!(
            sql_for(name, &SeriesQuery { range: "90d".into(), ..q() })
                .unwrap()
                .contains("INTERVAL 90 DAY"),
            "{name} ignores range"
        );
    }
}

/// The three `*_used` series share one percentage axis in the console, so each must be a ratio —
/// `pool_used` by dividing, the two node metrics by being clamped to [0,1].
#[test]
fn the_used_series_are_ratios() {
    assert!(sql_for("pool_used", &q()).unwrap().contains("nullIf(max(pool_total_bytes), 0)"));
    for name in ["cpu_used", "memory_used"] {
        let sql = sql_for(name, &q()).unwrap();
        assert!(sql.contains("least(greatest(avg(v), 0), 1)"), "{name}: {sql}");
    }
}

/// Every consumer of `events` and `alerts` queries `FINAL` (Global Constraints): both are
/// ReplacingMergeTrees and an at-least-once writer double-counts without it.
#[test]
fn the_deduplicated_tables_are_read_final() {
    for name in ["pending_requests", "decided_requests", "audit_events", "time_to_decide_p50"] {
        assert!(sql_for(name, &q()).unwrap().contains("rustic.events FINAL"), "{name}");
    }
    assert!(sql_for("firing_signals", &q()).unwrap().contains("rustic.alerts FINAL"));
}

/// `usage` needs an owner and a dimension; without them it is a 404-shaped miss rather than a query
/// over every owner at once.
#[test]
fn the_usage_series_requires_an_owner_and_a_dimension() {
    assert!(sql_for("usage", &q()).is_none());
    assert!(sql_for("usage", &SeriesQuery { owner: Some("acme".into()), ..q() }).is_none());
    assert!(sql_for("usage", &SeriesQuery { dimension: Some("cpu".into()), ..q() }).is_none());
    let with = SeriesQuery {
        owner: Some("acme".into()),
        dimension: Some("cpu".into()),
        ..q()
    };
    let sql = sql_for("usage", &with).unwrap();
    assert!(sql.contains("'acme'") && sql.contains("'cpu'"), "{sql}");
}

#[test]
fn an_unknown_series_has_no_statement() {
    assert!(sql_for("../../etc/passwd", &q()).is_none());
    assert!(sql_for("drop_table", &q()).is_none());
}

/// The one injection surface: `owner` and `region` are caller-supplied. They are quoted into SQL,
/// so a quote in them must be rejected outright rather than escaped — an owner slug never contains
/// one, and rejecting is the arm that cannot be got subtly wrong.
#[test]
fn a_quote_in_an_owner_or_region_is_refused_not_escaped() {
    let bad = SeriesQuery {
        owner: Some("a' OR '1'='1".into()),
        dimension: Some("cpu".into()),
        ..q()
    };
    assert!(sql_for("usage", &bad).is_none());
    let bad = SeriesQuery {
        region: Some("eu'; DROP TABLE rustic.events; --".into()),
        ..q()
    };
    assert!(sql_for("pool_used", &bad).is_none());
    // And a good region still filters, or the check would be passing by refusing everything.
    let good = SeriesQuery { region: Some("eu-west".into()), ..q() };
    assert!(sql_for("pool_used", &good).unwrap().contains("region = 'eu-west'"));
}

#[test]
fn range_and_step_are_allow_lists() {
    assert_eq!(parse_range("7d"), Some(7));
    assert_eq!(parse_range("30d"), Some(30));
    assert_eq!(parse_range("90d"), Some(90));
    assert_eq!(parse_range("9999d"), None);
    assert_eq!(parse_range("7d; DROP"), None);
    assert!(parse_step("1h").is_some());
    assert!(parse_step("1d").is_some());
    assert!(parse_step("1s").is_none());
    // A malformed range or step is a miss for every series, not a statement with a default in it.
    assert!(sql_for("pool_used", &SeriesQuery { range: "1y".into(), ..q() }).is_none());
    assert!(sql_for("pool_used", &SeriesQuery { step: "1m".into(), ..q() }).is_none());
}

#[test]
fn the_summary_is_last_delta_min_and_max() {
    let s = summarize(&[("a".into(), 3.0), ("b".into(), 9.0), ("c".into(), 5.0)]);
    assert_eq!(s.last, 5.0);
    // Delta is against the FIRST point in the range — "how much has this moved over the window".
    assert_eq!(s.delta, 2.0);
    assert_eq!(s.min, 3.0);
    assert_eq!(s.max, 9.0);
}

/// An empty series is the normal state of a fresh cluster and must summarize to zeros rather than
/// NaN, which serializes as `null` and renders as a broken chart.
#[test]
fn an_empty_series_summarizes_to_zeros() {
    let s = summarize(&[]);
    assert_eq!((s.last, s.delta, s.min, s.max), (0.0, 0.0, 0.0, 0.0));
}
