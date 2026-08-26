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

/// Adapts the mongo-backed `Directory` to `workspaces::api::MembershipCheck` — kept here rather
/// than in either crate so `rustic-git-workspaces` never needs a dependency on `rustic-git-pulls`
/// just for this one lookup.
struct DirMembership(Arc<rustic_git_pulls::directory::Directory>);

#[async_trait::async_trait]
impl rustic_git_workspaces::api::MembershipCheck for DirMembership {
    async fn teams_for(&self, user: &str) -> Vec<String> {
        self.0.for_user(user).await.unwrap_or_default().into_iter().map(|t| t.slug).collect()
    }
}

#[tokio::main]
async fn main() {
    rustic_git_core::log::init();
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

    // Workspaces/environments/regions routes need both a MetaStore and a signer, so they can
    // only be mounted once `jwt` above is Some. `COSMOS_ENDPOINT` unset means dev: an in-memory
    // store, lost on restart, same spirit as `RUSTIC_GIT_S3_URL=mem://` for the git store.
    let workspaces = match jwt.clone() {
        Some(jwt) => {
            let meta_store: Arc<dyn rustic_git_workspaces::store::MetaStore> =
                match std::env::var("COSMOS_ENDPOINT") {
                    Ok(endpoint) if !endpoint.is_empty() => {
                        let key = std::env::var("COSMOS_KEY")
                            .map_err(|_| err("COSMOS_KEY required with COSMOS_ENDPOINT"))?;
                        let db = env("COSMOS_DB", "rustic-git");
                        eprintln!("workspaces metadata in cosmos db `{db}`"); // ponytail: eprintln
                        Arc::new(
                            rustic_git_workspaces::cosmos::CosmosStore::new(&endpoint, &key, &db)
                                .await
                                .map_err(|e| err(format!("connecting to cosmos: {e:?}")))?,
                        )
                    }
                    _ => {
                        eprintln!("COSMOS_ENDPOINT unset: workspaces metadata is in-memory (dev only)"); // ponytail: eprintln
                        Arc::new(rustic_git_workspaces::store::MemStore::new())
                    }
                };
            // No admin-role system exists yet anywhere in this codebase (checked); a static
            // allowlist of emails is the whole mechanism for the region routes until one does.
            let admins = std::env::var("RUSTIC_GIT_WORKSPACES_ADMINS")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let mut state = rustic_git_workspaces::api::ApiState::new(meta_store, jwt, admins);
            if let Some(dir) = directory.clone() {
                state = state.with_membership(Arc::new(DirMembership(dir)));
            }
            // Volume history/refs reads go straight to the server tier's public
            // `/vol-agent/{owner}/{name}/*` surface with a shared agent token (same token shape
            // as `RUSTIC_GIT_VOL_AGENT_TOKENS` on that tier) — see `ApiState::registry`'s doc for
            // why that beats a peer-listener forward here. Both unset in dev: volume routes
            // answer 503 rather than not existing.
            if let (Ok(base), Ok(token)) =
                (std::env::var("RUSTIC_GIT_VOL_AGENT_URL"), std::env::var("RUSTIC_GIT_VOL_AGENT_TOKEN"))
            {
                state = state.with_registry(rustic_git_workspaces::registry_client::RegistryClient::new(base, token));
            } else {
                eprintln!(
                    "RUSTIC_GIT_VOL_AGENT_URL/RUSTIC_GIT_VOL_AGENT_TOKEN unset: /v1/volumes routes will answer 503"
                ); // ponytail: eprintln
            }
            // In-cluster config when the pod has a ServiceAccount, else the operator's kubeconfig.
            // `None` is a legitimate dev configuration (no cluster) — workspace, environment and
            // volume routes answer 503, the same shape an unset RUSTIC_GIT_VOL_AGENT_URL has.
            match kube::Client::try_default().await {
                Ok(c) => state = state.with_kube(c),
                Err(e) => eprintln!("no kubernetes config ({e}): /v1 workspace routes will answer 503"), // ponytail: eprintln
            }
            // The requeue sweep and the agent register/work/done/failed routes moved to the
            // server tier (Task 14) — this process now only serves the user-facing
            // /v1/workspaces|environments|regions|volumes routes.
            Some(Arc::new(state))
        }
        None => None,
    };

    let l = tokio::net::TcpListener::bind(env("RUSTIC_GIT_API_ADDR", "0.0.0.0:8090")).await?;
    eprintln!("api on {} -> {upstream}", l.local_addr()?); // ponytail: eprintln
    rustic_git_api::serve(store, cache, directory, jwt, upstream, secret, l, workspaces).await
}
