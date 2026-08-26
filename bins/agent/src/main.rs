//! `rustic-git-agent`: the fleet-side process that materializes workspaces on local btrfs.
//!
//! `run` boots the node-scoped controller (`controller.rs`), which watches the CRDs bound to this
//! node and converges the local btrfs pool and its pods. The hidden `squash <ws-id>` subcommand is
//! what `Engine::push` detaches via `std::env::current_exe` to build a block layer in the
//! background — running it, this binary IS `current_exe`, so that spawn now actually resolves
//! to a real process in production (previously nothing installed `current_exe` with a `squash`
//! arm). Its stdio is nulled (`ops.rs`'s `Stdio::null()`), so a failure also lands in
//! `{pool}/vol/{id}.squash-err` — otherwise it vanishes with no trace at all.

use rustic_git_agent::{build_engine, meta_store_from_env, owner_file, run, Config};
use rustic_git_workspaces::engine::migrate_ws_to_vol;

#[tokio::main]
async fn main() {
    rustic_git_core::log::init();
    // Exactly one rustls CryptoProvider must be installed before the FIRST TLS handshake, which
    // for this binary is the kube client connecting to the API server. Its absence is not a
    // connection error — it is a panic in rustls that names nothing about kube or startup order.
    // The same omission crash-looped the api binary once; see the helper's own doc comment.
    rustic_git_storage::config::install_crypto_provider();

    let args: Vec<String> = std::env::args().skip(1).collect();
    // One-time pool layout upgrade: `ws` was a misnomer (environments live there too, and it
    // didn't match the registry's `vol/{owner}/{id}` naming) — see `pool::migrate_ws_to_vol`.
    migrate_ws_to_vol(std::path::Path::new(&Config::from_env().pool));
    let result = match args.first().map(String::as_str) {
        Some("squash") => match args.get(1) {
            Some(id) => squash(id).await,
            // CLI output, not a log line: this is a person mistyping the command at a
            // terminal, and RUST_LOG must not be able to suppress their usage text.
            None => {
                eprintln!("usage: rustic-git-agent squash <ws-id>");
                std::process::exit(2);
            }
        },
        _ => run(Config::from_env()).await,
    };
    if let Err(e) = result {
        tracing::error!("{e}");
        std::process::exit(1);
    }
}

async fn squash(ws_id: &str) -> Result<(), String> {
    let cfg = Config::from_env();
    let meta = meta_store_from_env().await?;
    let engine = build_engine(&cfg.pool, meta.clone(), &cfg.api_url, &cfg.agent_token);
    // `Engine::push` spawns this with only the workspace id (`ops.rs`'s detached child) — the
    // owner it needs was left on the pool by the volume reconcile when it materialized this
    // volume (see `owner_file`'s doc comment).
    let path = owner_file(&cfg.pool, ws_id);
    let owner = std::fs::read_to_string(&path)
        .map(|s| s.trim().to_string())
        .map_err(|_| format!("squash {ws_id}: no {}", path.display()))?;
    let (w, _) = meta.get_ws(&owner, ws_id).await.map_err(|e| format!("{e:?}"))?.ok_or("workspace not found")?;
    if let Err(e) = engine.squash(&w).await {
        let msg = e.to_string();
        // CLI output: when a person runs `squash` by hand this is their only feedback, and
        // RUST_LOG must not be able to suppress it. Detached (`ops.rs`), stdio is nulled and it
        // is lost anyway — the file written below is the real trace for that path.
        eprintln!("squash {ws_id}: {msg}");
        let _ = std::fs::write(std::path::Path::new(&cfg.pool).join("vol").join(format!("{ws_id}.squash-err")), &msg);
        return Err(msg);
    }
    Ok(())
}
