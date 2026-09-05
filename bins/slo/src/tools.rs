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

/// Which binary each tool actually is.
///
/// A field on `Ctx` rather than a lookup in the process environment: `ssh.unregistered.refused`
/// is the one step whose meaning is that a command FAILED, and the only way to test it is to point
/// `git` at something that succeeds. Process env is global to the test binary, so two tests
/// running in parallel would set and unset each other's overrides.
#[derive(Debug, Clone)]
pub struct Programs {
    pub git: String,
    pub ssh_keygen: String,
    pub ssh_keyscan: String,
    pub crane: String,
}

impl Default for Programs {
    fn default() -> Self {
        Programs { git: "git".into(), ssh_keygen: "ssh-keygen".into(), ssh_keyscan: "ssh-keyscan".into(), crane: "crane".into() }
    }
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
        // `-p` and `--password` are `crane auth login`'s, and `"auth":` is the docker config
        // document crane writes and echoes back in some failures — both carry the probe's
        // personal token, which is a credential for the whole registry namespace.
        let cut = ["authorization:", "bearer ", "authorization=", "-p ", "--password ", "--password=", "\"auth\":"]
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
    let mut cmd = Command::new(name);
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

    /// `crane auth login … -p {token}` is the one argv in the probe that carries a credential
    /// crane itself echoes back on some failures — the personal token, which is a credential for
    /// the whole registry namespace, not one image.
    #[test]
    fn a_crane_password_never_survives_the_scrubber() {
        let secret = "kl_tokensecretvalue";
        for line in [
            format!("Error: logging in: crane auth login cr.example -u slo-probe -p {secret}"),
            format!("--password {secret}"),
            format!("--password={secret}"),
            format!(r#"{{"auths":{{"cr.example":{{"auth":"{secret}"}}}}}}"#),
        ] {
            let out = scrub(&line);
            assert!(!out.contains(secret), "{out}");
        }
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
