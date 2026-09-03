//! The read and team API — its own process, not a subcommand of the git server.
//!
//! Separate because the two have nothing in common at runtime. The git server owns
//! repositories: it holds SlateDB writer leases, answers SSH, and a restart moves
//! ownership around the fleet. This one owns no repository state at all — it reads
//! through a cache and the cluster (Kubernetes CRDs, for workspaces/environments/regions) — so
//! it scales on request volume, restarts freely, and must never be the reason a git node bounces.
//!
//! One binary with a subcommand made that distinction a convention. Two binaries
//! make it a fact: this process cannot open a repository for writing, because none
//! of that code is reachable from here.

use rustic_git_core::err;
use rustic_git_core::{require_jwt_secret_from_env, Result};
use rustic_git_storage::config::{env, install_crypto_provider, open_store};
use std::sync::Arc;

/// The mongo-backed `Directory` wearing the workspaces api's own `Directory` trait: team
/// membership, the CLI-token revocation list (a token works only while its row stands, the same
/// rule `crates/api`'s `user_identity` enforces), and the owner's `authorized_keys` for the
/// Secret every workspace's sshd reads. Kept here rather than in either crate so
/// `rustic-git-workspaces` never needs a dependency on `rustic-git-pulls` just for these lookups.
struct Dir(Arc<rustic_git_pulls::directory::Directory>);

#[async_trait::async_trait]
impl rustic_git_workspaces::api::Directory for Dir {
    async fn teams_for(&self, user: &str) -> Vec<String> {
        self.0.slugs_for(user).await.unwrap_or_default()
    }

    async fn is_live(&self, jti: &str) -> bool {
        matches!(
            self.0.credential(jti).await,
            Ok(Some(c)) if c.kind == rustic_git_pulls::directory::CredentialKind::CliToken
        )
    }

    async fn for_owner(&self, owner: &str) -> Option<rustic_git_workspaces::api::OwnerMaterial> {
        let authorized_keys = rustic_git_api::authorized_keys_for(&self.0, owner)
            .await
            .inspect_err(|e| tracing::warn!(%owner, error = %e, "reading ssh keys"))
            .ok()?;
        let (git_name, git_email) = rustic_git_api::git_identity_for(&self.0, owner)
            .await
            .inspect_err(|e| tracing::warn!(%owner, error = %e, "reading git identity"))
            .ok()?;
        Some(rustic_git_workspaces::api::OwnerMaterial { authorized_keys, git_name, git_email })
    }

    async fn team_role(&self, user: &str, team: &str) -> Option<rustic_git_workspaces::api::TeamRole> {
        use rustic_git_pulls::directory::Role;
        use rustic_git_workspaces::api::TeamRole;
        let t = self.0.get(team).await.ok().flatten()?;
        // The same `user` value `slugs_for` matches on, through the same members array — one
        // identity, so membership and role can never disagree.
        match rustic_git_pulls::directory::Directory::role_of(&t, user)? {
            Role::Owner => Some(TeamRole::Owner),
            Role::Admin => Some(TeamRole::Admin),
            Role::Member => Some(TeamRole::Member),
        }
    }

    async fn is_team(&self, slug: &str) -> bool {
        self.0.get(slug).await.ok().flatten().is_some()
    }
}

#[tokio::main]
async fn main() {
    rustic_git_core::log::init();
    rustic_git_core::metrics::init();
    // Its own listener: 8090 is what the ingress forwards `/v1` to.
    rustic_git_core::metrics::serve_if_configured().await;
    if let Err(e) = run().await {
        tracing::error!("{e}");
        std::process::exit(2);
    }
}

async fn run() -> Result<()> {
    // Explicit here as well as inside open_store: this process opens TLS to the k3s API
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
    // Same binary, same image, one env choosing which surface it exposes. Read once, up here,
    // both because the bootstrap below needs it and so the router-selection match downstream
    // reuses this binding rather than reading the env var a second time.
    let role = std::env::var("RUSTIC_GIT_API_ROLE").unwrap_or_else(|_| "user".into());
    let directory = match std::env::var("RUSTIC_GIT_MONGO_URI") {
        Ok(uri) if !uri.is_empty() => {
            let db = env("RUSTIC_GIT_MONGO_DB", "kloudlite");
            let d = rustic_git_pulls::directory::Directory::connect(&uri, &db).await?;
            tracing::info!(db = %db, "directory in mongo db");
            // Only the admin process seeds the directory: the bootstrap is additive and harmless
            // to run twice, but running it from the user role too would mean an operator who
            // scales the user Deployment to zero and only runs `admin` still gets it seeded —
            // reversed, running it here only, a fleet with no admin replica up yet simply has no
            // bootstrap run until one is, which is the safe direction to be wrong in.
            //
            // `RUSTIC_GIT_WORKSPACES_ADMINS` is a BOOTSTRAP now, not the list: it seeds the
            // directory once so an empty cluster has a first administrator, and after that the
            // list is managed through /api/admin/superadmins. Additive only — dropping an address
            // from the env must not silently revoke someone. Unset or empty defaults to the
            // owner's own address (2026-09-04) so a fresh deployment always has one superadmin to
            // add the rest from the admin area.
            if role == "admin" {
                let seed: Vec<String> = std::env::var("RUSTIC_GIT_WORKSPACES_ADMINS")
                    .unwrap_or_default()
                    .split(',')
                    .map(|s| s.trim().to_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect();
                let seed = if seed.is_empty() { vec!["karthik@kloudlite.io".to_string()] } else { seed };
                match d.ensure_superadmins(&seed).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(added = n, "superadmins seeded from RUSTIC_GIT_WORKSPACES_ADMINS"),
                    Err(e) => tracing::warn!(error = %e, "superadmin bootstrap skipped"),
                }
            }
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

    // Workspaces/environments/regions routes need a signer, so they can only be mounted once
    // `jwt` above is Some. Regions themselves are a CRD (`crd::Region`), read through the same
    // kube client every other route here uses — no separate store to wire up.
    let workspaces = match jwt.clone() {
        Some(jwt) => {
            let mut state = rustic_git_workspaces::api::ApiState::new(jwt);
            // So a new workspace comes up with the owner's platform-issued git key already mounted.
            state = state.with_keys(store.clone());
            if let Some(dir) = directory.clone() {
                state = state.with_directory(Arc::new(Dir(dir)));
            }
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
    // The admin role mounts ONLY `/admin` — no `/v1` route is compiled into that router at all, so
    // a `/v1` authorization bug literally cannot reach an admin handler on that process; the user
    // role mounts ONLY `/v1` and never sees an admin route (design doc §5). `role` was read once,
    // above, before the bootstrap decided whether to run.
    let workspaces_router = workspaces.map(|ws| match role.as_str() {
        "admin" => rustic_git_workspaces::api::admin::router(ws),
        _ => rustic_git_workspaces::api::router(ws),
    });
    rustic_git_api::serve(store, cache, directory, jwt, upstream, secret, l, workspaces_router, on_keys_changed)
        .await
}
