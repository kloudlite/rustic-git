//! `kloudlite.slo_runs` / `kloudlite.slo_results`: what the probe reports, and the maths the
//! console reads back.
//!
//! The probe is the only writer and it reports whole runs, so a re-sent report must collapse
//! rather than double-count — both tables are `ReplacingMergeTree` keyed on the probe's own
//! coordinates (`run_id`, and `(slo_id, ts, run_id)`), which only works because `ts` is the STEP's
//! timestamp from the probe and never insert time. Every reader queries `FINAL` for the same
//! reason.
//!
//! The per-SLO maths lives in one statement (`statuses_sql`) rather than one query per SLO: the
//! target's `max_ms` and each suite's windows are compiled into a `multiIf` over `slo_id` from the
//! catalogue, so ~70 SLOs cost one round trip and the catalogue stays the only place a target is
//! written down.

use chrono::{DateTime, NaiveDateTime, Utc};

use super::{History, HistoryError};
use crate::slo::catalogue::{self, Suite, CATALOGUE};

/// The wire format ClickHouse's `DateTime64(3)` accepts over HTTP.
const TS_FMT: &str = "%Y-%m-%d %H:%M:%S%.3f";

/// A run report carries every step, and a journey has nowhere near this many; the cap exists so a
/// malformed or hostile PUT cannot turn into an unbounded insert.
const MAX_STEPS: usize = 200;
/// A step's detail is a failure message for a human, not a log.
const MAX_DETAIL: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunState {
    Running,
    Passed,
    Failed,
}

impl RunState {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunState::Running => "running",
            RunState::Passed => "passed",
            RunState::Failed => "failed",
        }
    }

    /// Anything the table does not name is `running`: a row written by an older probe is better
    /// read as in-flight than dropped from the console entirely.
    fn parse(s: &str) -> RunState {
        match s {
            "passed" => RunState::Passed,
            "failed" => RunState::Failed,
            _ => RunState::Running,
        }
    }
}

/// One step's outcome. `skipped` steps are stored but excluded from every count — a step the probe
/// could not attempt is not evidence either way.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StepReport {
    pub slo_id: String,
    pub ts: DateTime<Utc>,
    pub ok: bool,
    pub ms: u32,
    pub skipped: bool,
    pub detail: String,
    /// The journey stage this step ran in ("5 · Workspace"), so a failed run reads as a place in
    /// the journey rather than as an id somebody has to look up.
    pub stage: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunReport {
    pub run_id: String,
    pub suite: String,
    pub region: String,
    pub started: DateTime<Utc>,
    pub finished: Option<DateTime<Utc>>,
    pub state: RunState,
    /// The stage the run is in (or died in).
    pub stage: String,
    pub steps: Vec<StepReport>,
}

/// One row of `slo_runs`, as both list endpoints answer it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Run {
    pub run_id: String,
    pub suite: String,
    pub region: String,
    pub started: DateTime<Utc>,
    pub finished: Option<DateTime<Utc>>,
    pub state: RunState,
    pub stage: String,
    pub steps_total: u16,
    pub steps_failed: u16,
    pub failed_step: String,
    pub failed_detail: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SloState {
    Ok,
    Burning,
    Breaching,
    Unknown,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Sample {
    pub ts: DateTime<Utc>,
    pub ok: bool,
    pub ms: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SloStatus {
    pub id: String,
    pub feature: String,
    pub sli: String,
    /// The catalogue's rendered Target column, so the console never re-formats a percentage.
    pub target: String,
    pub suite: String,
    /// `None` when the window holds no sample at all — a fresh cluster, not 0 %.
    pub attainment_30d: Option<f64>,
    pub total_30d: u64,
    pub budget_left: Option<f64>,
    pub burn_1h: Option<f64>,
    pub burn_6h: Option<f64>,
    pub last: Option<Sample>,
    pub state: SloState,
}

/// `bad / total` divided by the error budget's own rate — 1.0 means the budget is being spent
/// exactly as fast as 30 days allows. `None` when the window holds no sample: a burn rate over
/// nothing is not zero, it is unknown.
pub fn burn(bad: u64, total: u64, good_pct: f64) -> Option<f64> {
    let budget_rate = 1.0 - good_pct / 100.0;
    if total == 0 || budget_rate <= 0.0 {
        return None;
    }
    Some((bad as f64 / total as f64) / budget_rate)
}

/// How many bad samples the window can still afford. Negative means the budget is already spent —
/// kept as a signed number rather than clamped, because "how far past" is the useful part.
pub fn budget_left(good: u64, total: u64, good_pct: f64) -> f64 {
    let budget = (1.0 - good_pct / 100.0) * total as f64;
    budget - (total - good) as f64
}

/// The multiwindow, multi-burn-rate pairs from the SRE workbook, as the spec names them.
fn burning(short: Option<f64>, long: Option<f64>, threshold: f64) -> bool {
    matches!((short, long), (Some(s), Some(l)) if s > threshold && l > threshold)
}

pub fn validate(r: &RunReport) -> Result<(), String> {
    let suite = match r.suite.as_str() {
        s @ ("fast" | "weekly" | "monthly") => s,
        other => return Err(format!("unknown suite {other:?}")),
    };
    // `{suite}-{digits}` and nothing else: `run_id` is the ReplacingMergeTree key AND is
    // interpolated into every read, so its shape is the whole safety argument for that.
    let digits = r
        .run_id
        .strip_prefix(suite)
        .and_then(|rest| rest.strip_prefix('-'))
        .ok_or_else(|| format!("run id {:?} is not {suite}-{{digits}}", r.run_id))?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("run id {:?} is not {suite}-{{digits}}", r.run_id));
    }
    if r.steps.len() > MAX_STEPS {
        return Err(format!("{} steps, at most {MAX_STEPS}", r.steps.len()));
    }
    for s in &r.steps {
        if catalogue::find(&s.slo_id).is_none() {
            return Err(format!("unknown slo id {:?}", s.slo_id));
        }
        if s.detail.len() > MAX_DETAIL {
            return Err(format!("detail for {} is over {MAX_DETAIL} bytes", s.slo_id));
        }
    }
    Ok(())
}

/// Both tables, one report. `updated = now64(3)` is the ReplacingMergeTree version, so the LAST
/// write of a run wins — which is what makes the probe's running → passed/failed updates work at
/// all, and what makes a retried report a no-op.
pub async fn upsert(h: &History, r: &RunReport) -> Result<(), HistoryError> {
    let failed = r.steps.iter().find(|s| !s.ok && !s.skipped);
    let run = serde_json::json!({
        "run_id": r.run_id,
        "suite": r.suite,
        "region": r.region,
        "started": r.started.format(TS_FMT).to_string(),
        "finished": r.finished.map(|f| f.format(TS_FMT).to_string()),
        "state": r.state.as_str(),
        "stage": r.stage,
        "steps_total": r.steps.len(),
        "steps_failed": r.steps.iter().filter(|s| !s.ok && !s.skipped).count(),
        "failed_step": failed.map(|s| s.slo_id.as_str()).unwrap_or_default(),
        "failed_detail": failed.map(|s| s.detail.as_str()).unwrap_or_default(),
        "updated": Utc::now().format(TS_FMT).to_string(),
    });
    h.insert("slo_runs", &[run]).await?;
    let now = Utc::now().format(TS_FMT).to_string();
    let results: Vec<serde_json::Value> = r
        .steps
        .iter()
        .map(|s| {
            serde_json::json!({
                "run_id": r.run_id,
                "slo_id": s.slo_id,
                "ts": s.ts.format(TS_FMT).to_string(),
                "ok": u8::from(s.ok),
                "ms": s.ms,
                "skipped": u8::from(s.skipped),
                "detail": s.detail,
                "stage": s.stage,
                "updated": now,
            })
        })
        .collect();
    h.insert("slo_results", &results).await
}

/// The burn windows for one suite, in seconds: `(attainment, long, long-short, fast, fast-short)`.
/// Fast SLOs use the workbook's 6 h / 30 m and 1 h / 5 m pairs; a weekly or monthly SLO produces
/// one sample per period, so the same formula is applied over its own periods (4 w / 1 w, 6 m / 2 m
/// per the spec) — a 1 h window over a monthly SLO could only ever be empty.
fn windows(suite: Suite) -> (u64, u64, u64, u64, u64) {
    const H: u64 = 3_600;
    const D: u64 = 86_400;
    match suite {
        Suite::Fast => (30 * D, 6 * H, 1_800, H, 300),
        Suite::Weekly => (30 * D, 28 * D, 7 * D, 7 * D, 7 * D),
        Suite::Monthly => (30 * D, 180 * D, 60 * D, 60 * D, 60 * D),
    }
}

/// A `multiIf(slo_id = 'a', va, slo_id = 'b', vb, …, fallback)` over the whole catalogue. Every id
/// is a literal from `CATALOGUE`, never from a request, so nothing here is caller-shaped.
fn multi_if(value: impl Fn(&catalogue::Slo) -> String, fallback: &str) -> String {
    let mut s = String::from("multiIf(");
    for slo in CATALOGUE {
        s.push_str(&format!("slo_id = '{}', {}, ", slo.id, value(slo)));
    }
    s.push_str(fallback);
    s.push(')');
    s
}

/// One statement for every SLO's five windows plus its newest sample. `FINAL` because a re-sent
/// report is a second row until the parts merge, and counting it twice would move an attainment.
pub fn statuses_sql() -> String {
    // A "good" sample is a successful step that also met the target's ceiling; `0` is the
    // catalogue's `max_ms: None` (an availability SLO has no ceiling to miss).
    let max_ms = multi_if(|s| s.target.max_ms.unwrap_or(0).to_string(), "0");
    let w = |pick: fn((u64, u64, u64, u64, u64)) -> u64| {
        multi_if(move |s| pick(windows(s.suite)).to_string(), "0")
    };
    let counts = |name: &str, window: String| {
        format!(
            "countIf(skipped = 0 AND ts > now() - toIntervalSecond({window})) AS total_{name}, \
             countIf(skipped = 0 AND ts > now() - toIntervalSecond({window}) \
                     AND ok = 1 AND (mx = 0 OR ms <= mx)) AS good_{name}"
        )
    };
    format!(
        "WITH {max_ms} AS mx SELECT slo_id, {att}, {long}, {longs}, {fast}, {fasts}, \
         argMax(ok, ts) AS last_ok, argMax(ms, ts) AS last_ms, toString(max(ts)) AS last_ts \
         FROM kloudlite.slo_results FINAL \
         WHERE skipped = 0 AND ts > now() - INTERVAL 400 DAY \
         GROUP BY slo_id",
        att = counts("att", w(|x| x.0)),
        long = counts("long", w(|x| x.1)),
        longs = counts("longs", w(|x| x.2)),
        fast = counts("fast", w(|x| x.3)),
        fasts = counts("fasts", w(|x| x.4)),
    )
}

/// ClickHouse quotes 64-bit integers as strings in JSON by default, so every numeric column has to
/// accept both shapes.
fn num(v: Option<&serde_json::Value>) -> u64 {
    match v {
        Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn text(v: Option<&serde_json::Value>) -> String {
    v.and_then(|v| v.as_str()).unwrap_or_default().to_string()
}

/// `2026-09-05 10:00:00.000` as ClickHouse returns it. An unparsable timestamp reads as the epoch
/// rather than dropping the row: a visibly wrong date is easier to notice than a missing run.
fn ts(s: &str) -> DateTime<Utc> {
    NaiveDateTime::parse_from_str(s, TS_FMT)
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S"))
        .map(|t| t.and_utc())
        .unwrap_or(DateTime::UNIX_EPOCH)
}

/// Every catalogue entry, whether or not the probe has ever reported it — an SLO with no samples
/// is `Unknown` on the console, which is the honest answer and the one that shows a probe that
/// never ran.
pub async fn statuses(h: &History) -> Result<Vec<SloStatus>, HistoryError> {
    let rows = h.query(&statuses_sql()).await?;
    let now = Utc::now();
    Ok(CATALOGUE
        .iter()
        .map(|slo| {
            let row = rows.iter().find(|r| text(r.first()) == slo.id);
            let g = |i: usize| row.map(|r| num(r.get(i))).unwrap_or(0);
            let (total, good) = (g(1), g(2));
            let good_pct = slo.target.good_pct;
            let attainment_30d = (total > 0).then(|| good as f64 / total as f64);
            let burn_6h = burn(g(3) - g(4), g(3), good_pct);
            let burn_30m = burn(g(5) - g(6), g(5), good_pct);
            let burn_1h = burn(g(7) - g(8), g(7), good_pct);
            let burn_5m = burn(g(9) - g(10), g(9), good_pct);
            let last = row.and_then(|r| {
                let t = text(r.get(13));
                (!t.is_empty()).then(|| Sample {
                    ts: ts(&t),
                    ok: num(r.get(11)) == 1,
                    ms: num(r.get(12)) as u32,
                })
            });
            // Two periods of silence, not one: a probe that runs a minute late must not turn the
            // whole catalogue amber.
            let stale = last.as_ref().is_none_or(|s| {
                (now - s.ts).num_seconds() as u64 > 2 * slo.suite.period_secs()
            });
            let state = if stale {
                SloState::Unknown
            } else if attainment_30d.is_some_and(|a| a < good_pct / 100.0) {
                SloState::Breaching
            } else if burning(burn_5m, burn_1h, 14.4) || burning(burn_30m, burn_6h, 6.0) {
                SloState::Burning
            } else {
                SloState::Ok
            };
            SloStatus {
                id: slo.id.to_string(),
                feature: slo.feature.to_string(),
                sli: slo.sli.to_string(),
                target: slo.target.render(),
                suite: slo.suite.as_str().to_string(),
                attainment_30d,
                total_30d: total,
                budget_left: (total > 0).then(|| budget_left(good, total, good_pct)),
                burn_1h,
                burn_6h,
                last,
                state,
            }
        })
        .collect())
}

/// The columns every `Run` read selects, in the order `parse_run` expects.
const RUN_COLS: &str = "run_id, suite, region, toString(started), toString(finished), state, \
     stage, steps_total, steps_failed, failed_step, failed_detail, \
     if(finished IS NULL, 0, dateDiff('millisecond', started, finished)) AS duration_ms";

fn parse_run(r: &[serde_json::Value]) -> Run {
    let finished = text(r.get(4));
    Run {
        run_id: text(r.first()),
        suite: text(r.get(1)),
        region: text(r.get(2)),
        started: ts(&text(r.get(3))),
        // A NULL `finished` comes back as JSON null, whose `toString` is an empty string.
        finished: (!finished.is_empty()).then(|| ts(&finished)),
        state: RunState::parse(&text(r.get(5))),
        stage: text(r.get(6)),
        steps_total: num(r.get(7)) as u16,
        steps_failed: num(r.get(8)) as u16,
        failed_step: text(r.get(9)),
        failed_detail: text(r.get(10)),
        duration_ms: num(r.get(11)),
    }
}

/// Newest first. `suite` is the one caller-shaped value and it is matched against the three names
/// rather than escaped; `limit` is clamped to the spec's 100.
pub async fn runs(h: &History, suite: Option<&str>, limit: usize) -> Result<Vec<Run>, HistoryError> {
    let filter = match suite {
        Some(s @ ("fast" | "weekly" | "monthly")) => format!("WHERE suite = '{s}'"),
        // An unknown suite filters everything out rather than silently listing all of them.
        Some(_) => "WHERE 1 = 0".to_string(),
        None => String::new(),
    };
    let limit = limit.clamp(1, 100);
    let sql = format!(
        "SELECT {RUN_COLS} FROM kloudlite.slo_runs FINAL {filter} ORDER BY started DESC LIMIT {limit}"
    );
    Ok(h.query(&sql).await?.iter().map(|r| parse_run(r)).collect())
}

/// The run in flight, if any. A crashed probe leaves its row `running` forever, so the newest one
/// wins — `SloProbeMissing` is what notices that it never finished, not this.
pub async fn running(h: &History) -> Result<Option<Run>, HistoryError> {
    let sql = format!(
        "SELECT {RUN_COLS} FROM kloudlite.slo_runs FINAL WHERE state = 'running' \
         ORDER BY started DESC LIMIT 1"
    );
    Ok(h.query(&sql).await?.first().map(|r| parse_run(r)))
}

/// One run and its steps in probe order (`ts`), or `None` when no such run exists.
pub async fn run_steps(
    h: &History,
    run_id: &str,
) -> Result<Option<(Run, Vec<StepReport>)>, HistoryError> {
    // `run_id` is caller-shaped, so it goes through the same identifier check every other read
    // path uses — its `{suite}-{digits}` shape is well inside it.
    let Some(id) = super::series::ident(run_id) else {
        return Ok(None);
    };
    let sql = format!("SELECT {RUN_COLS} FROM kloudlite.slo_runs FINAL WHERE run_id = '{id}'");
    let Some(run) = h.query(&sql).await?.first().map(|r| parse_run(r)) else {
        return Ok(None);
    };
    let sql = format!(
        "SELECT slo_id, toString(ts), ok, ms, skipped, detail, stage \
         FROM kloudlite.slo_results FINAL WHERE run_id = '{id}' ORDER BY ts"
    );
    let steps = h
        .query(&sql)
        .await?
        .iter()
        .map(|r| StepReport {
            slo_id: text(r.first()),
            ts: ts(&text(r.get(1))),
            ok: num(r.get(2)) == 1,
            ms: num(r.get(3)) as u32,
            skipped: num(r.get(4)) == 1,
            detail: text(r.get(5)),
            stage: text(r.get(6)),
        })
        .collect();
    Ok(Some((run, steps)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(slo_id: &str) -> StepReport {
        StepReport {
            slo_id: slo_id.into(),
            ts: Utc::now(),
            ok: true,
            ms: 1,
            skipped: false,
            detail: String::new(),
            stage: "1 · Identity".into(),
        }
    }

    fn report(run_id: &str, suite: &str, steps: Vec<StepReport>) -> RunReport {
        RunReport {
            run_id: run_id.into(),
            suite: suite.into(),
            region: "central".into(),
            started: Utc::now(),
            finished: None,
            state: RunState::Running,
            stage: "1 · Identity".into(),
            steps,
        }
    }

    #[test]
    fn burn_and_budget_on_fixed_samples() {
        // One bad sample out of a 5-minute hour against a 99.9 % target: 83× the budget rate,
        // which is why the fast pair fires on the first failure.
        assert!((burn(1, 12, 99.9).unwrap() - 83.333).abs() < 0.01);
        assert_eq!(burn(0, 12, 99.9), Some(0.0));
        assert_eq!(burn(0, 0, 99.9), None);
        assert!((budget_left(11, 12, 99.9) - -0.988).abs() < 1e-9);
    }

    #[test]
    fn validate_rejects_bad_ids_and_unknown_slos() {
        assert!(validate(&report("fast-1", "fast", vec![step("git.push.ok")])).is_ok());
        assert!(validate(&report("fast-abc", "fast", vec![])).is_err());
        assert!(validate(&report("hourly-1", "hourly", vec![])).is_err());
        assert!(validate(&report("fast-1", "fast", vec![step("nope")])).is_err());
        let many = (0..201).map(|_| step("git.push.ok")).collect();
        assert!(validate(&report("fast-1", "fast", many)).is_err());
    }

    #[test]
    fn sql_for_statuses_is_one_statement_with_final() {
        let sql = statuses_sql();
        assert_eq!(sql.matches(';').count(), 0, "one statement");
        assert!(sql.contains("FINAL"));
        assert!(sql.contains("countIf(skipped = 0"));
        // Every SLO's ceiling has to be in the statement, or its samples would be judged by
        // somebody else's target.
        for slo in CATALOGUE {
            let ceiling = format!("slo_id = '{}', {}", slo.id, slo.target.max_ms.unwrap_or(0));
            assert!(sql.contains(&ceiling), "{} missing its max_ms", slo.id);
        }
    }
}
