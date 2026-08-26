//! One logging init, shared by all four binaries.
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

/// Install the process-wide subscriber. Reads `RUST_LOG`; defaults to `info` for our own
/// crates and `warn` for everything else, because the dependency graph (hyper, russh,
/// slatedb, aws-sdk) is chatty enough at `info` to bury our own lifecycle lines.
///
/// A second call is a no-op, not a panic — same contract as `install_crypto_provider`,
/// so a test or an embedded second entry point can call it freely.
pub fn init() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,rustic_git=info,rustic_git_core=info,rustic_git_storage=info,rustic_git_gitbase=info,rustic_git_pulls=info,rustic_git_app=info,rustic_git_git=info,rustic_git_registry=info,rustic_git_api=info,rustic_git_workspaces=info,rustic_git_server=info,rustic_git_worker=info,rustic_git_agent=info"));
    // `try_init` rather than `init`: the second caller gets an Err, not a panic.
    let _ = fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .try_init();
}

#[cfg(test)]
mod tests {
    #[test]
    fn init_is_idempotent() {
        super::init();
        super::init();
        tracing::info!("subscriber installed");
    }
}
