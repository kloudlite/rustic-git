//! One logging init, shared by all the binaries.
//!
//! It lives here for the same reason `install_crypto_provider` lives in the storage
//! bootstrap: a binary that forgets it does not fail, it goes SILENT. `tracing` with no
//! subscriber installed drops every event on the floor, so the symptom is an empty log
//! stream on a pod that looks healthy — the hardest failure to attribute back to a
//! missing line in `main`. Call `init()` as the first statement of every `main`.
//!
//! Output goes to stderr: in a container stdout is frequently a protocol stream (the git
//! wire protocol on the ssh path, `docker compose` output on the agent), and interleaving
//! log lines into it corrupts the payload.
//!
//! `RUSTIC_GIT_LOG_FORMAT=json` switches every binary to one JSON object per line, and every
//! deployed pod sets it: the collectors (`deploy/k3s/otel-agent.yaml`, the AKS twin in
//! `deploy/rustic-git.yaml`) parse that object so level, module (`target`) and the call-site
//! fields become columns in HyperDX rather than text inside a coloured string. Unset — a
//! laptop, a test — gives the human-readable form.

use tracing::Subscriber;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::{fmt, EnvFilter};

const DEFAULT_FILTER: &str = "warn,rustic_git=info,rustic_git_core=info,rustic_git_storage=info,rustic_git_gitbase=info,rustic_git_pulls=info,rustic_git_app=info,rustic_git_git=info,rustic_git_registry=info,rustic_git_api=info,rustic_git_workspaces=info,rustic_git_server=info,rustic_git_worker=info,rustic_git_agent=info";

/// Install the process-wide subscriber. Reads `RUST_LOG`; defaults to `info` for our own
/// crates and `warn` for everything else, because the dependency graph (hyper, russh,
/// slatedb, aws-sdk) is chatty enough at `info` to bury our own lifecycle lines.
///
/// A second call is a no-op, not a panic — same contract as `install_crypto_provider`,
/// so a test or an embedded second entry point can call it freely.
pub fn init() {
    let json = std::env::var("RUSTIC_GIT_LOG_FORMAT").is_ok_and(|v| v.eq_ignore_ascii_case("json"));
    // `try_set_global_default` rather than `init`: the second caller gets an Err, not a panic.
    let _ = tracing::subscriber::set_global_default(subscriber(json, std::io::stderr));
}

/// The subscriber `init` installs, built over any writer so a test can capture what it emits
/// without touching the process-wide default.
pub fn subscriber<W>(json: bool, w: W) -> Box<dyn Subscriber + Send + Sync>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    let b = fmt().with_writer(w).with_env_filter(filter);
    if json {
        // `flatten_event`: `message` and the call-site fields land at the top level next to
        // `level`/`target`, which is what a pipeline query like `fields.repo == x` wants.
        Box::new(b.json().flatten_event(true).finish())
    } else {
        Box::new(b.finish())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    #[test]
    fn init_is_idempotent() {
        super::init();
        super::init();
        tracing::info!("subscriber installed");
    }

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
        type Writer = Buf;
        fn make_writer(&'a self) -> Buf {
            self.clone()
        }
    }

    #[test]
    fn json_lines_parse_with_the_indexed_fields() {
        let buf = Buf::default();
        tracing::subscriber::with_default(super::subscriber(true, buf.clone()), || {
            tracing::warn!(repo = "alice/x", "lease lost");
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let line = out.lines().next().expect("one line");
        let v: serde_json::Value = serde_json::from_str(line).expect("parseable json");
        assert_eq!(v["level"], "WARN");
        assert_eq!(v["target"], "rustic_git_core::log::tests");
        assert_eq!(v["message"], "lease lost");
        assert_eq!(v["repo"], "alice/x");
    }
}
