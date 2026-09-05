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
    /// The burn rates over this SLO's own two windows, and what those windows are — a weekly or
    /// monthly SLO has no short window at all (`None`), because a 1 h window over one sample a
    /// week could only ever be empty and reporting it as a rate would be a made-up number.
    pub burn_short: Option<f64>,
    pub burn_long: Option<f64>,
    pub window_short_secs: u64,
    pub window_long_secs: u64,
    pub last: Option<Sample>,
    pub state: SloState,
}

/// `bad / total` divided by the error budget's own rate — 1.0 means the budget is being spent
/// exactly as fast as 30 days allows. `None` when the window holds no sample: a burn rate over
/// nothing is not zero, it is unknown.
///
/// A 100 % target has no budget to burn, so it is `None` too rather than an infinity: the first
/// bad sample takes its attainment below target and `Breaching` is what carries it.
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

/// One multiwindow, multi-burn-rate pair from the SRE workbook: both the short companion and the
/// long window have to exceed the threshold, which is what stops a single blip paging.
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

/// The windows one suite is judged over, in seconds:
/// `(attainment, long, long-companion, short, short-companion)`.
///
/// A fast SLO gets the workbook's two pairs — 6 h with a 30 m companion, 1 h with a 5 m one. A
/// weekly or monthly SLO produces ONE sample per period, so it has a single pair (4 w / 1 w,
/// 6 m / 2 m per the spec) and no short window at all: `0` means "no window", the count over it is
/// empty by construction and the rate is honestly `None` rather than a second copy of the long
/// one. The attainment window stays 30 d for every suite because that is what the catalogue's
/// targets are defined over.
fn windows(suite: Suite) -> (u64, u64, u64, u64, u64) {
    const H: u64 = 3_600;
    const D: u64 = 86_400;
    match suite {
        Suite::Fast => (30 * D, 6 * H, 1_800, H, 300),
        Suite::Weekly => (30 * D, 28 * D, 7 * D, 0, 0),
        Suite::Monthly => (30 * D, 180 * D, 60 * D, 0, 0),
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

/// The catalogue's latency ceilings as a `multiIf`. `0` is the sentinel for `max_ms: None` — an
/// availability SLO has no ceiling to miss.
fn max_ms_expr() -> String {
    multi_if(|s| s.target.max_ms.unwrap_or(0).to_string(), "0")
}

/// One window from `windows` as a `multiIf` over the catalogue, so every reader of these windows
/// (`statuses_sql` and `burn_sql`) compiles the SAME table rather than keeping a second copy of it.
fn window_expr(pick: fn((u64, u64, u64, u64, u64)) -> u64) -> String {
    multi_if(move |s| pick(windows(s.suite)).to_string(), "0")
}

/// `SloBurn`'s inner query: one row per SLO with samples, `[slo_id, burning]`.
///
/// The whole multiwindow rule is in the SQL because the evaluator yields ONE state per rule per
/// region — like `ProbeDown`, where the worst url decides, here the worst SLO does. Both windows of
/// a pair must exceed the threshold (14.4 on the fast pair, 6 on the long one), which is what stops
/// a single blip paging; a window with no samples, or a 100 % target with no budget to burn, gets
/// rate `-1` so it can never satisfy a threshold — the same "no samples is not zero" rule `burn`
/// holds in Rust.
pub fn burn_sql() -> String {
    let max_ms = max_ms_expr();
    let (long, longc) = (window_expr(|x| x.1), window_expr(|x| x.2));
    let (short, shortc) = (window_expr(|x| x.3), window_expr(|x| x.4));
    let budget = multi_if(|s| format!("{:.6}", 1.0 - s.target.good_pct / 100.0), "0");
    let counts = |w: &str| format!("countIf(in_{w}) AS t_{w}, countIf(in_{w} AND good) AS g_{w}");
    let rate = |w: &str| {
        format!("if(t_{w} > 0 AND budget > 0, (t_{w} - g_{w}) / t_{w} / budget, -1) AS r_{w}")
    };
    format!(
        "SELECT slo_id, \
                toUInt8((r_short > 14.4 AND r_shortc > 14.4) OR (r_long > 6 AND r_longc > 6)) \
                    AS burning \
         FROM (SELECT slo_id, {budget} AS budget, {rl}, {rlc}, {rs}, {rsc} FROM (\
             WITH {max_ms} AS mx, \
                  {long} AS w_long, {longc} AS w_longc, \
                  {short} AS w_short, {shortc} AS w_shortc, \
                  (ok = 1 AND (mx = 0 OR ms <= mx)) AS good, \
                  (ts > now() - toIntervalSecond(w_long)) AS in_long, \
                  (ts > now() - toIntervalSecond(w_longc)) AS in_longc, \
                  (w_short > 0 AND ts > now() - toIntervalSecond(w_short)) AS in_short, \
                  (w_shortc > 0 AND ts > now() - toIntervalSecond(w_shortc)) AS in_shortc \
             SELECT slo_id, {cl}, {clc}, {cs}, {csc} \
             FROM kloudlite.slo_results FINAL \
             WHERE skipped = 0 AND ts > now() - INTERVAL 400 DAY \
             GROUP BY slo_id))",
        cl = counts("long"),
        clc = counts("longc"),
        cs = counts("short"),
        csc = counts("shortc"),
        rl = rate("long"),
        rlc = rate("longc"),
        rs = rate("short"),
        rsc = rate("shortc"),
    )
}

/// The alias list `statuses_sql` selects, in the order `statuses` reads positionally. Named once
/// so the reader and the statement cannot drift apart silently — a shifted column would move
/// every attainment on the console without failing anything.
const STATUS_COLS: &str = "slo_id, \
     countIf(in_att) AS total_att, countIf(in_att AND good) AS good_att, \
     countIf(in_long) AS total_long, countIf(in_long AND good) AS good_long, \
     countIf(in_longc) AS total_longc, countIf(in_longc AND good) AS good_longc, \
     countIf(in_short) AS total_short, countIf(in_short AND good) AS good_short, \
     countIf(in_shortc) AS total_shortc, countIf(in_shortc AND good) AS good_shortc, \
     argMax(ok, ts) AS last_ok, argMax(ms, ts) AS last_ms, toString(max(ts)) AS last_ts";

/// One statement for every SLO's five windows plus its newest sample. `FINAL` because a re-sent
/// report is a second row until the parts merge, and counting it twice would move an attainment.
pub fn statuses_sql() -> String {
    // `0` is the sentinel for the catalogue's `max_ms: None` — an availability SLO has no ceiling
    // to miss — which is only unambiguous because no target may legitimately be 0 ms.
    debug_assert!(
        CATALOGUE.iter().all(|s| s.target.max_ms != Some(0)),
        "max_ms: Some(0) collides with the no-ceiling sentinel"
    );
    let max_ms = max_ms_expr();
    let w = window_expr;
    // Every per-row expression is hoisted into the WITH clause: each window's `multiIf` over the
    // whole catalogue is long, and it would otherwise appear twice per window in the counts.
    format!(
        "WITH {max_ms} AS mx, \
              {att} AS w_att, {long} AS w_long, {longc} AS w_longc, \
              {short} AS w_short, {shortc} AS w_shortc, \
              (ok = 1 AND (mx = 0 OR ms <= mx)) AS good, \
              (ts > now() - toIntervalSecond(w_att)) AS in_att, \
              (ts > now() - toIntervalSecond(w_long)) AS in_long, \
              (ts > now() - toIntervalSecond(w_longc)) AS in_longc, \
              (w_short > 0 AND ts > now() - toIntervalSecond(w_short)) AS in_short, \
              (w_shortc > 0 AND ts > now() - toIntervalSecond(w_shortc)) AS in_shortc \
         SELECT {STATUS_COLS} \
         FROM kloudlite.slo_results FINAL \
         WHERE skipped = 0 AND ts > now() - INTERVAL 400 DAY \
         GROUP BY slo_id",
        att = w(|x| x.0),
        long = w(|x| x.1),
        longc = w(|x| x.2),
        short = w(|x| x.3),
        shortc = w(|x| x.4),
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
            let burn_long = burn(g(3) - g(4), g(3), good_pct);
            let burn_longc = burn(g(5) - g(6), g(5), good_pct);
            let burn_short = burn(g(7) - g(8), g(7), good_pct);
            let burn_shortc = burn(g(9) - g(10), g(9), good_pct);
            let w = windows(slo.suite);
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
            // Precedence is Unknown > Burning > Breaching > Ok, per the plan: an SLO that is both
            // already past its 30-day target AND burning right now is reported as burning, because
            // the burn is the thing somebody can still act on.
            let state = if stale {
                SloState::Unknown
            } else if burning(burn_shortc, burn_short, 14.4) || burning(burn_longc, burn_long, 6.0)
            {
                SloState::Burning
            } else if attainment_30d.is_some_and(|a| a < good_pct / 100.0) {
                SloState::Breaching
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
                burn_short,
                burn_long,
                window_short_secs: w.3,
                window_long_secs: w.1,
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
    Ok(h.query(&runs_sql(suite, limit))
        .await?
        .iter()
        .map(|r| parse_run(r))
        .collect())
}

fn runs_sql(suite: Option<&str>, limit: usize) -> String {
    let filter = match suite {
        Some(s @ ("fast" | "weekly" | "monthly")) => format!("WHERE suite = '{s}'"),
        // An unknown suite filters everything out rather than silently listing all of them.
        Some(_) => "WHERE 1 = 0".to_string(),
        None => String::new(),
    };
    let limit = limit.clamp(1, 100);
    format!(
        "SELECT {RUN_COLS} FROM kloudlite.slo_runs FINAL {filter} ORDER BY started DESC LIMIT {limit}"
    )
}

/// The run in flight, if any. A crashed probe leaves its row `running` forever, so the newest one
/// wins — `SloProbeMissing` is what notices that it never finished, not this.
pub async fn running(h: &History) -> Result<Option<Run>, HistoryError> {
    Ok(h.query(&running_sql()).await?.first().map(|r| parse_run(r)))
}

fn running_sql() -> String {
    format!(
        "SELECT {RUN_COLS} FROM kloudlite.slo_runs FINAL WHERE state = 'running' \
         ORDER BY started DESC LIMIT 1"
    )
}

/// One run and its steps in probe order (`ts`), or `None` when no such run exists.
pub async fn run_steps(
    h: &History,
    run_id: &str,
) -> Result<Option<(Run, Vec<StepReport>)>, HistoryError> {
    let Some((run_sql, steps_sql)) = run_steps_sql(run_id) else {
        return Ok(None);
    };
    let Some(run) = h.query(&run_sql).await?.first().map(|r| parse_run(r)) else {
        return Ok(None);
    };
    let steps = h
        .query(&steps_sql)
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

/// The run's own row and its steps, or `None` when `run_id` is not an identifier we are willing to
/// interpolate — it is the one caller-shaped value on this path, so it goes through the same check
/// every other read uses; a `{suite}-{digits}` id is well inside it.
fn run_steps_sql(run_id: &str) -> Option<(String, String)> {
    let id = super::series::ident(run_id)?;
    Some((
        format!("SELECT {RUN_COLS} FROM kloudlite.slo_runs FINAL WHERE run_id = '{id}'"),
        format!(
            "SELECT slo_id, toString(ts), ok, ms, skipped, detail, stage \
             FROM kloudlite.slo_results FINAL WHERE run_id = '{id}' ORDER BY ts"
        ),
    ))
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
    fn run_reads_are_final_and_ident_checked() {
        for sql in [runs_sql(None, 20), runs_sql(Some("fast"), 1_000), running_sql()] {
            assert!(sql.contains("kloudlite.slo_runs FINAL"));
            // The console shows a run's duration, and a NULL `finished` must read as 0 rather
            // than blowing up the subtraction.
            assert!(sql.contains("dateDiff('millisecond'"));
        }
        assert!(runs_sql(Some("fast"), 20).contains("WHERE suite = 'fast'"));
        // An unknown suite filters everything out rather than silently listing every run.
        assert!(runs_sql(Some("nope'; DROP"), 20).contains("WHERE 1 = 0"));
        assert!(runs_sql(None, 1_000).ends_with("LIMIT 100"));
        assert!(running_sql().contains("state = 'running'"));

        let (run, steps) = run_steps_sql("fast-42").unwrap();
        assert!(run.contains("WHERE run_id = 'fast-42'"));
        assert!(steps.contains("kloudlite.slo_results FINAL"));
        assert!(steps.contains("WHERE run_id = 'fast-42' ORDER BY ts"));
        // Anything that is not an identifier never reaches a statement at all.
        assert!(run_steps_sql("fast-42' OR 1=1 --").is_none());
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
        assert!(sql.contains("kloudlite.slo_results FINAL"));
        assert!(sql.contains("WHERE skipped = 0"));
        // The reader is positional, so the alias ORDER is part of the contract: this fails the
        // moment a column is inserted, reordered or renamed on either side.
        let aliases: Vec<&str> = STATUS_COLS
            .split(" AS ")
            .skip(1)
            .map(|a| a.split(',').next().unwrap_or("").trim())
            .collect();
        assert_eq!(
            aliases,
            [
                "total_att",
                "good_att",
                "total_long",
                "good_long",
                "total_longc",
                "good_longc",
                "total_short",
                "good_short",
                "total_shortc",
                "good_shortc",
                "last_ok",
                "last_ms",
                "last_ts"
            ]
        );
        assert!(sql.contains(STATUS_COLS), "the statement selects those columns");

        // Every SLO's ceiling has to be in the statement, or its samples would be judged by
        // somebody else's target.
        for slo in CATALOGUE {
            let ceiling = format!("slo_id = '{}', {}", slo.id, slo.target.max_ms.unwrap_or(0));
            assert!(sql.contains(&ceiling), "{} missing its max_ms", slo.id);
        }
    }
}
