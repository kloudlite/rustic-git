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

impl Dir {
    /// Handle in, email out. Every identity this process is handed comes off the JWT as a HANDLE,
    /// and every place the directory compares one — `members.user`, `role_of`, `add_member` — holds
    /// the EMAIL that is the user row's `_id`. One identity, the directory's, everywhere; a handle
    /// nobody answers to resolves to `None` rather than being compared as if it were an address.
    ///
    /// The error stays an error rather than folding into `None`: "the directory is unreadable" and
    /// "no such person" are different answers, and only the caller knows which of the two its own
    /// surface can afford to conflate.
    async fn email_of(&self, handle: &str) -> Result<Option<String>> {
        Ok(self.0.user_by_handle(handle).await?.map(|u| u.email))
    }

    /// The read-side spelling: both membership lookups already failed closed on a query error, so
    /// an unreadable directory is no membership here — logged, never guessed at.
    async fn email_or_closed(&self, handle: &str) -> Option<String> {
        self.email_of(handle)
            .await
            .inspect_err(|e| tracing::error!(error = %e, %handle, "resolving handle"))
            .ok()
            .flatten()
    }
}

#[async_trait::async_trait]
impl rustic_git_workspaces::api::Directory for Dir {
    async fn teams_for(&self, user: &str) -> Vec<String> {
        let Some(email) = self.email_or_closed(user).await else { return vec![] };
        self.0.slugs_for(&email).await.unwrap_or_default()
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
        // The same value `slugs_for` matches on, through the same members array — one identity,
        // so membership and role can never disagree.
        let email = self.email_or_closed(user).await?;
        match rustic_git_pulls::directory::Directory::role_of(&t, &email)? {
            Role::Owner => Some(TeamRole::Owner),
            Role::Admin => Some(TeamRole::Admin),
            Role::Member => Some(TeamRole::Member),
        }
    }

    async fn is_team(&self, slug: &str) -> bool {
        self.0.get(slug).await.ok().flatten().is_some()
    }

    async fn grant_access(
        &self,
        team: &str,
        user: &str,
        role: rustic_git_workspaces::api::TeamRole,
    ) -> rustic_git_workspaces::api::GrantAccess {
        use rustic_git_pulls::directory::{AddMember, Membership, Role};
        use rustic_git_workspaces::api::{GrantAccess, TeamRole};
        let role = match role {
            TeamRole::Owner => Role::Owner,
            TeamRole::Admin => Role::Admin,
            TeamRole::Member => Role::Member,
        };
        // `user` is the handle the request was opened under; `add_member` keys on the email. An
        // unresolved handle would reach it as an address nobody has, and every approve would
        // answer "no such user" without ever having looked the asker up.
        let email = match self.email_of(user).await {
            Ok(Some(email)) => email,
            Ok(None) => return GrantAccess::NoSuchUser,
            Err(e) => {
                // An unreadable directory is not a verdict on the asker: a decider retries this,
                // where "no such user" would have them chasing a person who exists.
                tracing::error!(error = %e, %team, "resolving handle for team access");
                return GrantAccess::Refused("the directory could not be read".into());
            }
        };
        // Add first, then fall through to a role change: a grant is "be in this team at this
        // role", and whether they were already in it is not the decider's problem. `add_member`'s
        // filter carries its own duplicate check, so this is safe to retry.
        match self.0.add_member(team, &email, role).await {
            Ok(AddMember::Added) => return GrantAccess::Done,
            Ok(AddMember::NoSuchUser) => return GrantAccess::NoSuchUser,
            Ok(AddMember::NoSuchTeam) => return GrantAccess::NoSuchTeam,
            Ok(AddMember::AlreadyMember) => {}
            Err(e) => {
                tracing::error!(error = %e, %team, "granting team access");
                return GrantAccess::Refused("the directory could not be written".into());
            }
        }
        match self.0.set_role(team, &email, role).await {
            Ok(Membership::Done) => GrantAccess::Done,
            Ok(Membership::NotAMember) => GrantAccess::NoSuchUser,
            Ok(Membership::NoSuchTeam) => GrantAccess::NoSuchTeam,
            Ok(Membership::LastOwner) => {
                GrantAccess::Refused("a team must keep at least one owner".into())
            }
            Err(e) => {
                tracing::error!(error = %e, %team, "setting team role");
                GrantAccess::Refused("the directory could not be written".into())
            }
        }
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
            // The admin role's one outbound call to the git tier: `PUT /admin/settings/central`
            // forwards a validated patch to the server tier's peer route rather than writing the
            // object store itself (spec's "no direct object-store write path of its own by
            // design"). `GET /admin/settings/central` reads `cluster/settings` off `store`
            // directly instead — see `ApiState::keys`, already wired below.
            if role == "admin" {
                state = state.with_peer(rustic_git_workspaces::api::admin::PeerClient::new(upstream.clone(), secret.clone()));
            }
            // Only the admin role ever talks to its OWN cluster (`admin::workloads`'s central
            // half) — server/api/worker/gateway have no business with it, only this process does.
            // `kube::Config::incluster()` is used explicitly rather than `try_default()`: the
            // block above already points `KUBECONFIG` at a mounted region kubeconfig Secret, and
            // `try_default()` would honor that env var and hand back the SAME region client
            // again instead of this cluster's own projected ServiceAccount token.
            // `automountServiceAccountToken: true` is set on the admin Deployment alone
            // (deploy/rustic-git.yaml), so this succeeds only there.
            if role == "admin" {
                match kube::Config::incluster() {
                    Ok(cfg) => match kube::Client::try_from(cfg) {
                        Ok(c) => state = state.with_aks(c),
                        Err(e) => tracing::warn!(error = %e, "in-cluster config rejected: /admin/workloads central rolls will answer 503"),
                    },
                    Err(e) => tracing::warn!(error = %e, "no in-cluster config: /admin/workloads central rolls will answer 503"),
                }
            }
            // ClickHouse is the admin process's alone (design §A1: it is the only writer of the
            // `rustic` database). Optional: an unset URL leaves `history` None, every
            // /admin/history route answers 503, and nothing is recorded — exactly how the
            // deployment behaved before ClickStack existed.
            if role == "admin" {
                match rustic_git_workspaces::history::History::from_env() {
                    Some(h) => {
                        // Migrations at boot, so a fresh ClickStack becomes usable with no manual
                        // step. A failure is LOGGED, not fatal: quota decisions and node drains
                        // must not be held hostage by an analytics store, and the next restart
                        // retries an idempotent set of statements.
                        match rustic_git_workspaces::history::schema::migrate(&h).await {
                            Ok(0) => tracing::info!("clickhouse schema up to date"),
                            Ok(n) => tracing::info!(applied = n, "clickhouse migrations applied"),
                            Err(e) => tracing::error!(error = %e, "clickhouse migrations failed; history will be incomplete until the next restart"),
                        }
                        let h = Arc::new(h);
                        // The second consumer group on the one `events` stream. Spawned only in
                        // the admin role, because it is the only writer of `rustic.events`.
                        let consumer_cache = cache.clone();
                        let consumer_history = h.clone();
                        tokio::spawn(async move {
                            rustic_git_workspaces::history::events::consume_forever(
                                consumer_cache,
                                consumer_history,
                            )
                            .await
                        });
                        // Every watch runs against `state.kube` — the region's k3s through the
                        // mounted kubeconfig, the same client `client_for_region` hands out —
                        // because that is where every CRD of ours, `Region` included, lives. A
                        // missing client simply means no watch: history is optional, and boot
                        // never waits on one.
                        // No fallback name: every row a watch writes is STAMPED with the region,
                        // so `"default"` on a cluster that is really `eu-west` mislabels history
                        // permanently. A missing watch only leaves a gap an operator closes by
                        // setting the variable and restarting; a wrong region cannot be undone.
                        match std::env::var("RUSTIC_GIT_REGION").ok().filter(|r| !r.is_empty()) {
                            Some(region) => {
                                if let Some(k) = state.kube.clone() {
                                    let h = h.clone();
                                    tokio::spawn(
                                        rustic_git_workspaces::history::watch::watch_region(
                                            k, region, h,
                                        ),
                                    );
                                }
                            }
                            None => tracing::warn!(
                                "RUSTIC_GIT_REGION unset: region history watches not started"
                            ),
                        }
                        // `Region` objects live in the same k3s cluster as everything else
                        // (`/v1/regions` writes through the mounted kubeconfig); this AKS cluster
                        // holds none of our CRDs, so a watch against `state.aks` 404s forever.
                        if let Some(k) = state.kube.clone() {
                            let h = h.clone();
                            tokio::spawn(rustic_git_workspaces::history::watch::watch_central(
                                k, h,
                            ));
                        }
                        state = state.with_cache(cache.clone()).with_history(h);
                    }
                    None => tracing::warn!(
                        "RUSTIC_GIT_CLICKHOUSE_URL unset: /admin/history answers 503 and nothing is recorded"
                    ),
                }
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
    // The hourly folds and the alert evaluator. Spawned from the admin role only, and only with
    // ClickHouse configured — the fold itself is a cluster-wide list, and running either for
    // nowhere to write it would be pure load on the API server.
    if role == "admin" {
        if let Some(ws) = workspaces.clone() {
            if ws.history.is_some() {
                tokio::spawn(rustic_git_workspaces::history::alerts::evaluate_forever(ws.clone()));
                tokio::spawn(rustic_git_workspaces::history::beats::run_beats(ws));
            }
        }
    }
    // The admin role mounts ONLY `/admin` — no `/v1` route is compiled into that router at all, so
    // a `/v1` authorization bug literally cannot reach an admin handler on that process; the user
    // role mounts ONLY `/v1` and never sees an admin route (design doc §5). `role` was read once,
    // above, before the bootstrap decided whether to run.
    let workspaces_router = workspaces.map(|ws| match role.as_str() {
        "admin" => rustic_git_workspaces::api::admin::router(ws),
        _ => rustic_git_workspaces::api::router(ws),
    });
    rustic_git_api::serve(
        store,
        cache,
        directory,
        jwt,
        upstream,
        secret,
        l,
        workspaces_router,
        on_keys_changed,
        role == "admin",
    )
    .await
}
