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
use std::time::Instant;

use chrono::Datelike;
use clap::{Parser, Subcommand};
use kloudlite_git_core::metrics::{register, Kind};
use kloudlite_git_slo::ctx::{Ctx, OTHER_EMAIL, OTHER_USER, PROBE_EMAIL, PROBE_USER};
use kloudlite_git_slo::stages;
use kloudlite_git_slo::suite::TEARDOWN;
use kloudlite_git_slo::{suite, Config};
use kloudlite_git_workspaces::history::slo::StepReport;
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
        /// Set by the parent on the child it spawns. Not for humans: a second process reporting
        /// under a run id nobody owns would file a run the parent then overwrites.
        #[arg(long, hide = true)]
        inner: bool,
        /// The run the child must report under — the parent's, never its own.
        #[arg(long, hide = true)]
        run_id: Option<String>,
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
            std::process::exit(EXIT_CONFIG);
        }
    };
    let code = match cli.cmd {
        Cmd::Bootstrap => bootstrap(cfg).await,
        Cmd::Run { suite: kind, inner: false, .. } => parent(cfg, kind).await,
        Cmd::Run { suite: kind, inner: true, run_id } => child(cfg, kind, run_id).await,
    };
    std::process::exit(code);
}

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

    let exe = std::env::current_exe().unwrap_or_else(|_| Path::new("kloudlite-git-slo").into());
    let status = std::process::Command::new(exe)
        .args(["run", "--suite", kind.as_str(), "--inner", "--run-id", &c.run_id])
        .status();
    let child_code = match status {
        Ok(s) => s.code().unwrap_or(EXIT_FAILED),
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
async fn child(cfg: Config, kind: Suite, run_id: Option<String>) -> i32 {
    let mut c = match Ctx::new(cfg, kind, run_id).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "slo.run.failed");
            return EXIT_CONFIG;
        }
    };
    for stage in suite(kind) {
        c.stage = stage.name.to_string();
        let started = Instant::now();
        (stage.run)(&mut c).await;
        tracing::info!(stage = stage.name, failed = c.failed(), duration_ms = started.elapsed().as_millis() as u64, "slo.stage.done");
        // Before the PUT, not after: if the report is what is broken, the parent still gets every
        // step this run measured.
        if let Ok(b) = serde_json::to_vec(&c.steps) {
            if let Err(e) = std::fs::write(c.steps_path(), b) {
                tracing::warn!(op = "write", error = %e, "slo.steps.failed");
            }
        }
        // A failed report does NOT stop the run: the parent's final PUT may well succeed, and
        // stopping here would cost teardown the rest of the journey for nothing.
        if let Err(e) = c.report(stage.name, false).await {
            tracing::error!(error = %format!("{e:#}"), "slo.report.failed");
            c.report_failed = true;
        }
    }
    match () {
        _ if c.report_failed => EXIT_REPORT_FAILED,
        _ if c.failed() > 0 => EXIT_FAILED,
        _ => 0,
    }
}

/// Claims both usernames. A name already claimed by this probe answers a 4xx, which is a success
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
    let body = serde_json::json!({ "users": [
        { "email": PROBE_EMAIL, "name": "SLO probe", "username": PROBE_USER },
        { "email": OTHER_EMAIL, "name": "SLO other", "username": OTHER_USER },
    ]});
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
