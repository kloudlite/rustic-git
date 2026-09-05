//! Running `git`, `ssh` and friends with a timeout, and turning a failure into a step detail.
//!
//! Two rules make this a module rather than three inline `Command::output()` calls.
//!
//! The first is the scrubber. Every git call carries the probe's JWT in `-c http.extraHeader`, and
//! `merge_worker.rs` learned the hard way that the moment an argv is formatted into an error
//! message the credential is in the logs forever. So the argv is NEVER part of what comes back:
//! only the program name, the exit status and the child's own stderr, and that stderr is scrubbed
//! too because git echoes parts of its configuration in some failures.
//!
//! The second is the timeout. A hung `git clone` against a wedged node would otherwise sit there
//! until the pod's `activeDeadlineSeconds` killed the whole run, losing every later stage's
//! sample — the failure has to be one bad step, not a lost journey.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::process::Command;

/// Lets a test point `git`/`ssh` at a program that succeeds or fails on demand. Nothing else in
/// the binary can make a real `ssh` refuse a connection, and `ssh.unregistered.refused` is a step
/// whose whole meaning is that it did. Never set in a deployment — the same shape as
/// `suite::PANIC_ENV`.
const PROGRAM_OVERRIDE: &str = "KLOUDLITE_GIT_SLO_TEST_PROGRAM";

fn program(name: &str) -> String {
    std::env::var(format!("{PROGRAM_OVERRIDE}_{}", name.to_uppercase()))
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| name.to_string())
}

/// Everything after `Bearer` / `Authorization:` in one line of text, replaced.
///
/// Deliberately blunt: it keeps the shape of the message ("fatal: Authorization: …") so the reader
/// still learns which header was involved, and it errs towards redacting a word that was not a
/// secret rather than towards printing one that was.
pub fn scrub(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, line) in s.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let lower = line.to_ascii_lowercase();
        // The earliest marker on the line wins: a line can carry both spellings, and cutting at
        // the later one would leave the first credential in place.
        let cut = ["authorization:", "bearer ", "authorization="]
            .iter()
            .filter_map(|m| lower.find(m).map(|at| at + m.len()))
            .min();
        match cut {
            Some(at) => {
                out.push_str(&line[..at]);
                out.push('…');
            }
            None => out.push_str(line),
        }
    }
    out
}

/// Run `name` with `args`, `env` on top of the inherited environment, in `dir`.
///
/// `Ok` is the child's stdout; `Err` is a message safe to put in a step detail. Both stdout and
/// stderr are captured rather than inherited so a probe pod's logs stay the probe's own lines.
pub async fn run(
    name: &str,
    args: &[String],
    env: &HashMap<String, String>,
    dir: Option<&Path>,
    timeout: Duration,
) -> Result<String> {
    let mut cmd = Command::new(program(name));
    cmd.args(args).envs(env).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    // `kill_on_drop`: the timeout below drops the future, and a `git clone` left running would go
    // on writing into the tmp tree that the next step is about to read.
    cmd.kill_on_drop(true);
    let out = match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(anyhow!("could not run {name}: {e}")),
        Err(_) => return Err(anyhow!("{name} timed out after {} ms", timeout.as_millis())),
    };
    if !out.status.success() {
        let err = scrub(String::from_utf8_lossy(&out.stderr).trim());
        // No argv, ever: see the module comment.
        return Err(anyhow!("{name} exited {}: {err}", out.status.code().unwrap_or(-1)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// `run` with no environment and no working directory, for the one-shot tools.
pub async fn plain(name: &str, args: &[&str], timeout: Duration) -> Result<String> {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    run(name, &args, &HashMap::new(), None, timeout).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bearer_token_never_survives_the_scrubber() {
        let secret = "eyJhbGciOiJIUzI1NiJ9.tokenbody.sig";
        let leaked = format!("fatal: could not read Authorization: Bearer {secret}");
        let out = scrub(&leaked);
        assert!(!out.contains(secret), "{out}");
        assert!(out.starts_with("fatal: could not read Authorization:"), "{out}");
    }

    #[test]
    fn every_line_is_scrubbed_not_only_the_first() {
        let out = scrub("ok\nAuthorization: Bearer abc\nBearer def");
        assert!(!out.contains("abc") && !out.contains("def"), "{out}");
        assert!(out.starts_with("ok\n"), "{out}");
    }

    #[tokio::test]
    async fn a_failing_command_reports_its_stderr_and_never_its_argv() {
        let token = "eyJsecret";
        let args = vec![
            "-c".to_string(),
            format!("echo 'Authorization: Bearer {token}' >&2; exit 7"),
        ];
        let e = run("sh", &args, &HashMap::new(), None, Duration::from_secs(10))
            .await
            .expect_err("sh exits 7");
        let detail = format!("{e:#}");
        assert!(!detail.contains(token), "the token leaked into the detail: {detail}");
        assert!(detail.contains("sh exited 7"), "{detail}");
    }

    #[tokio::test]
    async fn a_hung_command_is_a_timeout_not_a_hang() {
        let args = vec!["30".to_string()];
        let e = run("sleep", &args, &HashMap::new(), None, Duration::from_millis(50))
            .await
            .expect_err("cut off");
        assert!(format!("{e:#}").contains("timed out"), "{e:#}");
    }
}
