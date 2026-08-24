//! `rustic-git-agent`: the fleet-side process that materializes workspaces on local btrfs.
//!
//! `run` registers with the control-plane API (Task 7) and long-polls `/v1/agent/work`,
//! dispatching each job to the `Engine` (Task 4). The hidden `squash <ws-id>` subcommand is
//! what `Engine::push` detaches via `std::env::current_exe` to build a block layer in the
//! background — running it, this binary IS `current_exe`, so that spawn now actually resolves
//! to a real process in production (previously nothing installed `current_exe` with a `squash`
//! arm).

use rustic_git_agent::{build_engine, meta_store_from_env, owner_file, run, Config};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("squash") => squash(args.get(1)).await,
        _ => run(Config::from_env()).await,
    };
    if let Err(e) = result {
        eprintln!("{e}"); // ponytail: eprintln
        std::process::exit(1);
    }
}

async fn squash(ws_id: Option<&String>) -> Result<(), String> {
    let ws_id = ws_id.ok_or("usage: rustic-git-agent squash <ws-id>")?;
    let cfg = Config::from_env();
    let meta = meta_store_from_env().await?;
    let engine = build_engine(&cfg.pool, meta.clone());
    // `Engine::push` spawns this with only the workspace id (`ops.rs`'s detached child) — the
    // owner it needs for the `MetaStore` lookup was left on the pool by `run_job` when it
    // created/forked/cloned this workspace (see `owner_file`'s doc comment).
    let path = owner_file(&cfg.pool, ws_id);
    let owner = std::fs::read_to_string(&path)
        .map(|s| s.trim().to_string())
        .map_err(|_| format!("squash {ws_id}: no {}", path.display()))?;
    let (w, _) = meta.get_ws(&owner, ws_id).await.map_err(|e| format!("{e:?}"))?.ok_or("workspace not found")?;
    engine.squash(&w).await.map_err(|e| e.to_string())
}
