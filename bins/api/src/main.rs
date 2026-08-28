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

/// The directory as the workspaces api's revocation list: a CLI token works only while its row
/// stands, the same rule `crates/api`'s `user_identity` enforces on this tier's own routes.
struct DirCliTokens(Arc<rustic_git_pulls::directory::Directory>);

#[async_trait::async_trait]
impl rustic_git_workspaces::api::CliTokenCheck for DirCliTokens {
    async fn is_live(&self, jti: &str) -> bool {
        matches!(
            self.0.credential(jti).await,
            Ok(Some(c)) if c.kind == rustic_git_pulls::directory::CredentialKind::CliToken
        )
    }
}

/// The owner's `authorized_keys`, for the Secret every workspace's sshd reads.
struct DirKeys(Arc<rustic_git_pulls::directory::Directory>);

#[async_trait::async_trait]
impl rustic_git_workspaces::api::AuthorizedKeys for DirKeys {
    async fn for_owner(&self, owner: &str) -> Option<String> {
        rustic_git_api::authorized_keys_for(&self.0, owner)
            .await
            .inspect_err(|e| tracing::warn!(%owner, error = %e, "reading ssh keys"))
            .ok()
    }
}

#[tokio::main]
async fn main() {
    rustic_git_core::log::init();
    if let Err(e) = run().await {
        tracing::error!("{e}");
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
            tracing::info!(db = %db, "directory in mongo db");
            Some(Arc::new(d))
        }
        _ => {
            tracing::warn!("RUSTIC_GIT_MONGO_URI unset: /v1 routes will answer 503");
            None
        }
    };

    // Same rule as the git tier: in a fleet an unset secret is a startup error, not a
    // degraded mode, because the tokens this tier mints are verified by the other one.
    require_jwt_secret_from_env()?;
    let jwt = match std::env::var("RUSTIC_GIT_JWT_SECRET") {
        Ok(s) if !s.is_empty() => Some(Arc::new(rustic_git_core::jwt::Jwt::new(&s)?)),
        _ => {
            tracing::warn!("RUSTIC_GIT_JWT_SECRET unset: sign-in cannot issue tokens");
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
                        tracing::info!(db = %db, "workspaces metadata in cosmos db");
                        Arc::new(
                            rustic_git_workspaces::cosmos::CosmosStore::new(&endpoint, &key, &db)
                                .await
                                .map_err(|e| err(format!("connecting to cosmos: {e:?}")))?,
                        )
                    }
                    _ => {
                        tracing::warn!("COSMOS_ENDPOINT unset: workspaces metadata is in-memory (dev only)");
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
            // So a new workspace comes up with the owner's platform-issued git key already mounted.
            state = state.with_keys(store.clone());
            if let Some(dir) = directory.clone() {
                state = state.with_membership(Arc::new(DirMembership(dir.clone())));
                state = state.with_cli_tokens(Arc::new(DirCliTokens(dir.clone())));
                state = state.with_authorized_keys(Arc::new(DirKeys(dir)));
            }
            // Snapshots live on the server tier, not in the cluster: a snapshot outlives the
            // workspace it was taken of, so the volume routes read the browse tier's
            // `/api/{owner}/volumes|volumehistory` over the same peer credentials this process
            // already proxies browse reads with.
            state = state.with_upstream(Arc::new(rustic_git_workspaces::upstream::Upstream::new(
                &upstream,
                &secret,
            )));
            // In-cluster config when the pod has a ServiceAccount, else the operator's kubeconfig.
            // `None` is a legitimate dev configuration (no cluster) — workspace and environment
            // routes answer 503 rather than not existing. The volume routes keep working without
            // it: the cluster only says whether a snapshot's parent is still around.
            match kube::Client::try_default().await {
                Ok(c) => state = state.with_kube(c),
                Err(e) => tracing::warn!(error = %e, "no kubernetes config: /v1 workspace routes will answer 503"),
            }
            // The requeue sweep and the agent register/work/done/failed routes moved to the
            // server tier (Task 14) — this process now only serves the user-facing
            // /v1/workspaces|environments|regions|volumes routes.
            Some(Arc::new(state))
        }
        None => None,
    };

    let l = tokio::net::TcpListener::bind(env("RUSTIC_GIT_API_ADDR", "0.0.0.0:8090")).await?;
    tracing::info!(addr = %l.local_addr()?, %upstream, "api listening");
    // Adding or removing an ssh key has to reach every running workspace of that owner, and the
    // Secret it lands in is the workspaces tier's to write — so the hook is just that call.
    let on_keys_changed: Option<rustic_git_api::KeysChanged> = workspaces.clone().map(|ws| {
        Arc::new(move |owner: String| {
            let ws = ws.clone();
            Box::pin(async move { rustic_git_workspaces::api::refresh_user_keys(&ws, &owner).await })
                as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }) as rustic_git_api::KeysChanged
    });
    rustic_git_api::serve(store, cache, directory, jwt, upstream, secret, l, workspaces, on_keys_changed)
        .await
}
