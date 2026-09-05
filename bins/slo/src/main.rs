//! Run one suite, or claim the two usernames the runs need.
//!
//! **A run is two processes.** The release profile is `panic = "abort"` and Cargo does not allow
//! `panic` in a per-package profile override, so no in-process `catch_unwind` can save a run
//! whose stage panicked — the pod would die with no teardown and no final report, which is the
//! one outcome the console cannot distinguish from "the CronJob never fired". So `run` re-executes
//! this same binary with `--inner`: the child walks the journey and reports after every stage,
//! the parent waits for it whatever it does, then runs teardown and files the final report from
//! the steps the child left on disk. The parent itself does nothing that can panic.

use std::path::Path;
use std::time::{Duration, Instant};

use chrono::Datelike;
use clap::{Parser, Subcommand};
use kloudlite_core::metrics::{register, Kind};
use kloudlite_slo::ctx::{email_of, Ctx, SUITE_TENANTS};
use kloudlite_slo::stages;
use kloudlite_slo::suite::TEARDOWN;
use kloudlite_slo::Config;
use kloudlite_workspaces::history::slo::StepReport;
use kloudlite_workspaces::slo::catalogue::Suite;

#[derive(Parser)]
#[command(name = "kloudlite-slo")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Run {
        #[arg(long, value_parser = parse_suite)]
        suite: Suite,
        /// Set by the parent on the child it spawns. Not for humans: a second process reporting
        /// under a run id nobody owns would file a run the parent then overwrites.
        #[arg(long, hide = true)]
        inner: bool,
        /// The run the child must report under — the parent's, never its own.
        #[arg(long, hide = true)]
        run_id: Option<String>,
        /// The run's wall-clock budget, in seconds. Set by the parent on the child from
        /// `KLOUDLITE_SLO_BUDGET_SECS`; the child stops STARTING stages once it is spent.
        #[arg(long, hide = true)]
        budget_secs: Option<u64>,
    },
    /// Idempotent one-off: claim the probe's two usernames. Safe to re-run.
    Bootstrap,
}

fn parse_suite(s: &str) -> Result<Suite, String> {
    Suite::parse(s).ok_or_else(|| format!("unknown suite {s:?}"))
}

/// 0 passed · 1 a step failed · 2 the environment is unusable · 3 the report could not be filed.
/// Four codes rather than two because each needs a different human: a failed step is an SLO
/// breach, a bad config is a deploy, and a run nobody stored is invisible to the console — it
/// cannot be told apart from a CronJob that never fired.
const EXIT_FAILED: i32 = 1;
const EXIT_CONFIG: i32 = 2;
const EXIT_REPORT_FAILED: i32 = 3;

#[tokio::main]
async fn main() {
    // `reqwest` is pinned `rustls-no-provider`, and with two providers reachable rustls refuses to
    // pick one for us.
    let _ = rustls::crypto::ring::default_provider().install_default();
    kloudlite_core::log::init();
    register(&[
        ("slo_steps_total", Kind::Counter, &[("ok", "true")]),
        ("slo_steps_total", Kind::Counter, &[("ok", "false")]),
        ("slo_run_duration_seconds", Kind::Histogram, &[]),
    ]);
    let cli = Cli::parse();
    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "slo.config.failed");
            std::process::exit(EXIT_CONFIG);
        }
    };
    let code = match cli.cmd {
        Cmd::Bootstrap => bootstrap(cfg).await,
        Cmd::Run { suite: kind, inner: false, .. } => parent(cfg, kind).await,
        Cmd::Run { suite: kind, inner: true, run_id, budget_secs } => {
            child(cfg, kind, run_id, budget(budget_secs)).await
        }
    };
    std::process::exit(code);
}

/// The run's wall-clock budget: the flag the parent passes, else the environment, else the
/// fallback. One function so the parent and the child read the same number the same way.
fn budget(flag: Option<u64>) -> Duration {
    let secs = flag
        .or_else(|| std::env::var("KLOUDLITE_SLO_BUDGET_SECS").ok()?.parse().ok())
        .unwrap_or(kloudlite_slo::suite::DEFAULT_BUDGET_SECS);
    Duration::from_secs(secs)
}

/// How long past the budget the parent lets the child live before killing it. The child stops
/// starting stages at the budget, but the one already running has its own ceiling, and the kill is
/// for the child that is wedged somewhere no ceiling covers.
const KILL_SLACK: Duration = Duration::from_secs(60);

/// Spawn the journey, then clean up after it and file the run — whatever it did.
async fn parent(cfg: Config, kind: Suite) -> i32 {
    // The monthly CronJob fires `0 3 * * 0` — every SUNDAY, because cron cannot say "the first
    // Sunday of the month". This guard is the other half of that expression: past the 7th it is
    // not the first Sunday, and the drills below are far too destructive to run four times a month.
    if kind == Suite::Monthly && chrono::Utc::now().day() > 7 {
        tracing::info!(reason = "not the first week of the month", "slo.run.skipped");
        return 0;
    }
    let mut c = match Ctx::new(cfg, kind, None).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "slo.run.failed");
            return EXIT_CONFIG;
        }
    };
    tracing::info!(run_id = %c.run_id, suite = kind.as_str(), "slo.run.started");
    let started = Instant::now();
    // The child needs the tmp directory to exist before its first stage; its own boot creates it
    // too, but the parent reads `steps.json` out of it and must not depend on that ordering.
    let _ = std::fs::create_dir_all(&c.tmp);

    let budget = budget(None);
    let exe = std::env::current_exe().unwrap_or_else(|_| Path::new("kloudlite-slo").into());
    let spawned = tokio::process::Command::new(exe)
        .args([
            "run",
            "--suite",
            kind.as_str(),
            "--inner",
            "--run-id",
            &c.run_id,
            "--budget-secs",
            &budget.as_secs().to_string(),
        ])
        .spawn();
    let child_code = match spawned {
        // The kill is the backstop under the child's own budget: the child stops starting stages
        // when the budget is spent, but a child wedged somewhere no ceiling covers would otherwise
        // run until the CronJob's `activeDeadlineSeconds` killed the POD — losing teardown and the
        // report with it, which is the one outcome the console cannot tell from "never fired".
        Ok(mut ch) => match tokio::time::timeout(budget + KILL_SLACK, ch.wait()).await {
            Ok(Ok(s)) => s.code().unwrap_or(EXIT_FAILED),
            Ok(Err(e)) => {
                tracing::error!(error = %e, "slo.child.failed");
                EXIT_FAILED
            }
            Err(_) => {
                tracing::error!(budget_secs = budget.as_secs(), "slo.child.killed");
                if let Err(e) = ch.kill().await {
                    tracing::error!(error = %e, "slo.child.kill.failed");
                }
                EXIT_FAILED
            }
        },
        Err(e) => {
            // Nothing walked the journey, but the run still gets a row: a run that reports
            // nothing is indistinguishable from one that never started.
            tracing::error!(error = %e, "slo.child.failed");
            EXIT_FAILED
        }
    };
    tracing::info!(code = child_code, "slo.child.completed");
    // A child that died mid-journey may have left no failing step at all — its last stage never
    // finished. The run is a failure regardless of what the step list says.
    c.run_failed = child_code != 0;

    // Whatever the child managed to record. A missing or half-written file is an empty list —
    // the report then carries only teardown, which is still a run the console can see failed.
    c.steps = std::fs::read(c.steps_path())
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<StepReport>>(&b).ok())
        .unwrap_or_default();
    // And every name the child recorded, so teardown deletes by name what the prefix sweep
    // cannot see (an environment's volume is named by the platform, not by us).
    if let Some(state) = std::fs::read(c.state_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
        c.state = state;
    }

    c.stage = TEARDOWN.to_string();
    let teardown_started = Instant::now();
    stages::teardown(&mut c).await;
    tracing::info!(stage = TEARDOWN, duration_ms = teardown_started.elapsed().as_millis() as u64, "slo.stage.done");

    // Always recorded, on every path out of here: a duration only present on the happy path is a
    // series that silently under-reports exactly the runs somebody wants to see.
    metrics::histogram!("slo_run_duration_seconds").record(started.elapsed().as_secs_f64());

    let report_failed = c.report(TEARDOWN, true).await.is_err_and(|e| {
        tracing::error!(error = %format!("{e:#}"), "slo.report.failed");
        true
    });
    let failed = c.failed();
    let state = if failed == 0 && !c.run_failed { "passed" } else { "failed" };
    tracing::info!(run_id = %c.run_id, state, failed, "slo.run.finished");
    match () {
        // Report first: an unstored run is the failure a human must act on, even if it also had
        // a failing step somebody would otherwise be paged for.
        _ if report_failed || child_code == EXIT_REPORT_FAILED => EXIT_REPORT_FAILED,
        _ if state == "failed" => EXIT_FAILED,
        _ => 0,
    }
}

/// The journey. Exits 0 / 1 / 3 and never runs teardown — the parent owns that, because this
/// process may not survive its own stages.
async fn child(cfg: Config, kind: Suite, run_id: Option<String>, budget: Duration) -> i32 {
    let mut c = match Ctx::new(cfg, kind, run_id).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "slo.run.failed");
            return EXIT_CONFIG;
        }
    };
    kloudlite_slo::suite::walk(&mut c, kind, budget).await;
    match () {
        _ if c.report_failed => EXIT_REPORT_FAILED,
        _ if c.failed() > 0 => EXIT_FAILED,
        _ => 0,
    }
}

/// Claims every suite's usernames. A name already claimed by this probe answers a 4xx, which is a success
/// here — `bootstrap` is re-run on every deploy and must never fail on the second one.
async fn bootstrap(cfg: Config) -> i32 {
    let c = match Ctx::new(cfg, Suite::Fast, None).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "slo.bootstrap.failed");
            return EXIT_CONFIG;
        }
    };
    let mut code = 0;
    // One admin call creates both identities: the api tier only creates a person at sign-in, and a
    // synthetic user never signs in, so `/v1/users/username` alone answered 400 "no such user".
    let url = format!("{}/admin/slo/bootstrap", c.cfg.admin_url.trim_end_matches('/'));
    // Every suite's pair, not just this process's: `bootstrap` is run once per deploy and the
    // suites are isolated from each other by owning DIFFERENT tenants (ctx::SUITE_TENANTS), so a
    // bootstrap that only claimed the pair its own env named would leave the hourly and drill
    // suites with no accounts at all until somebody ran it again with a different env.
    let users: Vec<_> = SUITE_TENANTS
        .iter()
        .flat_map(|(p, o)| [(*p, "SLO probe"), (*o, "SLO other")])
        .map(|(user, name)| serde_json::json!({ "email": email_of(user), "name": name, "username": user }))
        .collect();
    let body = serde_json::json!({ "users": users });
    match c.http.post(&url).header("authorization", c.bearer(&c.admin_jwt)).json(&body).send().await {
        Ok(r) if r.status().is_success() => tracing::info!(kind = "users", "slo.bootstrap.completed"),
        Ok(r) => {
            let status = r.status().as_u16();
            let detail = r.text().await.unwrap_or_default();
            tracing::error!(kind = "users", code = status, detail, "slo.bootstrap.failed");
            code = EXIT_FAILED;
        }
        Err(e) => {
            tracing::error!(kind = "users", error = %e.without_url(), "slo.bootstrap.failed");
            code = EXIT_FAILED;
        }
    }
    // The digest is printed, never stored: `reg.canary` reads it from the environment, so a human
    // pins it into the CronJob's yaml deliberately rather than the probe trusting its own registry.
    match stages::registry::ensure_canary(&c).await {
        Ok(digest) => tracing::info!(kind = "canary", digest = %digest, "slo.bootstrap.completed"),
        Err(e) => {
            tracing::error!(kind = "canary", error = %format!("{e:#}"), "slo.bootstrap.failed");
            code = EXIT_FAILED;
        }
    }
    code
}
