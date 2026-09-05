//! Run one suite, or claim the two usernames the runs need.

use std::panic::AssertUnwindSafe;
use std::time::Instant;

use clap::{Parser, Subcommand};
use futures::FutureExt;
use kloudlite_git_core::metrics::{register, Kind};
use kloudlite_git_slo::ctx::{Ctx, OTHER_EMAIL, OTHER_USER, PROBE_EMAIL, PROBE_USER};
use kloudlite_git_slo::{suite, Config};
use kloudlite_git_workspaces::slo::catalogue::Suite;

#[derive(Parser)]
#[command(name = "kloudlite-git-slo")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Run {
        #[arg(long, value_parser = parse_suite)]
        suite: Suite,
    },
    /// Idempotent one-off: claim the probe's two usernames. Safe to re-run.
    Bootstrap,
}

fn parse_suite(s: &str) -> Result<Suite, String> {
    match s {
        "fast" => Ok(Suite::Fast),
        "weekly" => Ok(Suite::Weekly),
        "monthly" => Ok(Suite::Monthly),
        other => Err(format!("unknown suite {other:?}")),
    }
}

/// 0 passed · 1 a step failed · 3 the report could not be filed. Three exit codes rather than
/// two because a run nobody stored needs its own alert: the console cannot tell it from a run
/// that never started.
const EXIT_FAILED: i32 = 1;
const EXIT_REPORT_FAILED: i32 = 3;

#[tokio::main]
async fn main() {
    // `reqwest` is pinned `rustls-no-provider`, and with two providers reachable rustls refuses to
    // pick one for us.
    let _ = rustls::crypto::ring::default_provider().install_default();
    kloudlite_git_core::log::init();
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
            std::process::exit(EXIT_FAILED);
        }
    };
    let code = match cli.cmd {
        Cmd::Bootstrap => bootstrap(cfg).await,
        Cmd::Run { suite: kind } => run(cfg, kind).await,
    };
    std::process::exit(code);
}

async fn run(cfg: Config, kind: Suite) -> i32 {
    // The monthly suite's CronJob fires on a day-of-month schedule, and the drills in it are
    // expensive enough that a misconfigured schedule must not run them daily.
    if kind == Suite::Monthly && chrono::Utc::now().format("%d").to_string().parse::<u32>().unwrap_or(99) > 7 {
        tracing::info!("slo.run.skipped");
        return 0;
    }
    let mut c = match Ctx::new(cfg, kind).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "slo.run.failed");
            return EXIT_FAILED;
        }
    };
    tracing::info!(run_id = %c.run_id, suite = kind.as_str(), "slo.run.started");
    let started = Instant::now();
    let stages = suite(kind);
    let last = stages.len() - 1;
    for (i, stage) in stages.iter().enumerate() {
        c.stage = stage.name.to_string();
        // A panicking stage must not cost the run its teardown or its report — those are the two
        // things that make a broken run visible and leave nothing behind.
        if AssertUnwindSafe((stage.run)(&mut c)).catch_unwind().await.is_err() {
            tracing::error!(stage = stage.name, "slo.stage.panicked");
        }
        tracing::info!(stage = stage.name, failed = c.failed(), "slo.stage.done");
        // The final report is the one after teardown, and it is the only one that says finished.
        if let Err(e) = c.report(stage.name, i == last).await {
            tracing::error!(error = %format!("{e:#}"), "slo.report.failed");
            return EXIT_REPORT_FAILED;
        }
    }
    metrics::histogram!("slo_run_duration_seconds").record(started.elapsed().as_secs_f64());
    let failed = c.failed();
    let state = if failed == 0 { "passed" } else { "failed" };
    tracing::info!(run_id = %c.run_id, state, failed, "slo.run.finished");
    if failed == 0 {
        0
    } else {
        EXIT_FAILED
    }
}

/// Claims both usernames. A name already claimed by this probe answers a 4xx, which is a success
/// here — `bootstrap` is re-run on every deploy and must never fail on the second one.
async fn bootstrap(cfg: Config) -> i32 {
    let c = match Ctx::new(cfg, Suite::Fast).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "slo.bootstrap.failed");
            return EXIT_FAILED;
        }
    };
    let mut code = 0;
    for (email, user, jwt) in
        [(PROBE_EMAIL, PROBE_USER, &c.probe_jwt), (OTHER_EMAIL, OTHER_USER, &c.other_jwt)]
    {
        let url = format!("{}/v1/users/username", c.cfg.api_url.trim_end_matches('/'));
        match c
            .http
            .post(&url)
            .header("authorization", c.bearer(jwt))
            .json(&serde_json::json!({ "username": user }))
            .send()
            .await
        {
            Ok(r) => tracing::info!(email, user, status = %r.status(), "slo.bootstrap.username"),
            Err(e) => {
                tracing::error!(email, user, error = %e, "slo.bootstrap.failed");
                code = EXIT_FAILED;
            }
        }
    }
    // The registry canary (`slo-probe/canary`) is pushed with `crane`, which stage 4 is the first
    // thing to need — until it exists there is nothing honest to do here.
    tracing::info!("slo.bootstrap.canary.skipped");
    code
}
