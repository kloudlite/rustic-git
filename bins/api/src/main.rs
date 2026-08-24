//! The read and team API — its own process, not a subcommand of the git server.
//!
//! Separate because the two have nothing in common at runtime. The git server owns
//! repositories: it holds SlateDB writer leases, answers SSH, and a restart moves
//! ownership around the fleet. This one owns no repository state at all — it reads
//! through a cache and talks to Cosmos — so it scales on request volume, restarts
//! freely, and must never be the reason a git node bounces.
//!
//! One binary with a subcommand made that distinction a convention. Two binaries
//! make it a fact: this process cannot open a repository for writing, because none
//! of that code is reachable from here.

use rustic_git_core::err;
use rustic_git_core::{require_jwt_secret_from_env, Result};
use rustic_git_storage::config::{env, install_crypto_provider, open_store};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("{e}"); // ponytail: eprintln
        std::process::exit(2);
    }
}

async fn run() -> Result<()> {
    // Explicit here as well as inside open_store: this process opens TLS to Cosmos
    // too, and a future reordering must not depend on which call happens first.
    install_crypto_provider();

    // `false`: compaction and garbage collection belong to the process that owns
    // the repository. Running them here would put two compactors on one database.
    let store = open_store(false).await?;
    let cache = store.cache.clone();

    // The browse routes live on the git nodes' PEER listener, so this must be the
    // peer Service, never the public one.
    let upstream = env("RUSTIC_GIT_UPSTREAM", "http://rustic-git:8081");
    let secret = std::env::var("RUSTIC_GIT_PEER_SECRET")
        .map_err(|_| err("RUSTIC_GIT_PEER_SECRET required"))?;

    // Optional on purpose: without it the browse routes still answer and only the
    // team routes report unavailable. A database outage must not stop reads that
    // never touched it.
    let directory = match std::env::var("RUSTIC_GIT_MONGO_URI") {
        Ok(uri) if !uri.is_empty() => {
            let db = env("RUSTIC_GIT_MONGO_DB", "kloudlite");
            let d = rustic_git_pulls::directory::Directory::connect(&uri, &db).await?;
            eprintln!("directory in mongo db `{db}`"); // ponytail: eprintln
            Some(Arc::new(d))
        }
        _ => {
            eprintln!("RUSTIC_GIT_MONGO_URI unset: /v1 routes will answer 503"); // ponytail: eprintln
            None
        }
    };

    // Same rule as the git tier: in a fleet an unset secret is a startup error, not a
    // degraded mode, because the tokens this tier mints are verified by the other one.
    require_jwt_secret_from_env()?;
    let jwt = match std::env::var("RUSTIC_GIT_JWT_SECRET") {
        Ok(s) if !s.is_empty() => Some(Arc::new(rustic_git_core::jwt::Jwt::new(&s)?)),
        _ => {
            eprintln!("RUSTIC_GIT_JWT_SECRET unset: sign-in cannot issue tokens"); // ponytail: eprintln
            None
        }
    };

    let l = tokio::net::TcpListener::bind(env("RUSTIC_GIT_API_ADDR", "0.0.0.0:8090")).await?;
    eprintln!("api on {} -> {upstream}", l.local_addr()?); // ponytail: eprintln
    rustic_git_api::serve(store, cache, directory, jwt, upstream, secret, l).await
}
