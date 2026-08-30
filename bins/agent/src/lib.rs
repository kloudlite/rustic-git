//! Process setup for `rustic-git-agent`: the local `Engine`, the storage janitor (`janitor.rs`),
//! and the
//! Kubernetes client the node controller (`controller.rs`) reconciles with. The work itself is
//! there, not here — the CRD IS the work item, so there is no queue, no lease and no poll loop.

use rustic_git_workspaces::engine::{blob, Engine, Pool};
use std::sync::Arc;

pub mod binding;
pub mod claim;
pub mod controller;
pub mod janitor;
pub mod nix;
pub mod peer;
pub mod snapshot;
pub mod sshkeys;

/// Env-derived config shared by `run` and the `squash` subcommand — both need the same Engine.
pub struct Config {
    pub api_url: String,
    pub region: String,
    pub agent_token: String,
    pub pool: String,
    /// This node's name, from the downward-API `NODE_NAME`. It is the shard key: the controller
    /// watches only objects whose `spec.nodeName` equals it.
    pub node: String,
}

impl Config {
    /// Base URL for the agent work surface: `WS_REGISTRY_URL` names the SERVER tier, not
    /// `bins/api`, which is where register/work/done/failed live.
    pub fn from_env() -> Config {
        let api_url = match std::env::var("WS_REGISTRY_URL") {
            Ok(v) if !v.is_empty() => v,
            _ => "http://127.0.0.1:8081".into(),
        };
        Config {
            api_url,
            region: std::env::var("WS_REGION").unwrap_or_else(|_| "default".into()),
            agent_token: std::env::var("WS_AGENT_TOKEN").unwrap_or_default(),
            pool: std::env::var("WS_POOL").unwrap_or_else(|_| "/mnt/wspool".into()),
            // Declared capacity is gone: the kubelet reports node allocatable, and a second
            // hand-maintained copy of it is a second thing that can be wrong.
            node: std::env::var("NODE_NAME").unwrap_or_default(),
        }
    }
}

/// Build the region's blob store: Azure when `AZURE_ACCOUNT` is set, else the `S3_URL` MinIO
/// fallback used by tests (`engine::blob` already has both constructors).
pub fn blob_store() -> Arc<dyn object_store::ObjectStore> {
    match (std::env::var("AZURE_ACCOUNT"), std::env::var("AZURE_KEY"), std::env::var("AZURE_CONTAINER")) {
        (Ok(a), Ok(k), Ok(c)) => blob::region_store(&a, &k, &c),
        _ => blob::s3_store(),
    }
}

/// Construct the `Engine` this agent (or the detached `squash` subcommand) operates against.
/// `registry_url`/`agent_token` point the engine's `RegistryClient` at the same server tier
/// (and same token) the agent already uses for `register`/`work`/`jobs/*` — `WS_REGISTRY_URL`
/// serves both surfaces.
pub fn build_engine(pool: &str, registry_url: &str, agent_token: &str) -> Engine {
    Engine::new(
        Pool::new(pool),
        blob_store(),
        rustic_git_workspaces::registry_client::RegistryClient::new(registry_url, agent_token),
    )
}

/// Boots the node controller: Engine, janitor, Kubernetes client, then reconcile forever.
pub async fn run(cfg: Config) -> Result<(), String> {
    let engine = Arc::new(build_engine(&cfg.pool, &cfg.api_url, &cfg.agent_token));
    let nix_client: Arc<dyn nix::Nix> = Arc::new(nix::RealNix { bin: "/nix/var/nix/profiles/default/bin".into() });
    janitor::spawn_janitor(engine.clone(), cfg.pool.clone(), nix_client.clone());
    if cfg.node.is_empty() {
        return Err("NODE_NAME is unset: the controller would watch every node's objects".into());
    }
    let pin = nix::nixpkgs_pin();
    if pin.is_empty() {
        return Err("WS_NIXPKGS is required: the nixpkgs pin every profile on this node is built against".into());
    }
    // A branch or a tag would make the same package hash mean different bits on different days,
    // which is the one promise the profile hash makes.
    if !nix::valid_pin(&pin) {
        return Err(format!("WS_NIXPKGS must be github:NixOS/nixpkgs/<40-hex-rev>, not {pin:?}"));
    }
    if let Err(e) = std::fs::create_dir_all(nix::PROFILES_DIR) {
        tracing::warn!(error = %e, "could not create the Nix profiles dir — the daemon container seeds /nix");
    }
    // One indirect root over the whole profiles tree: `nix build --no-link` registers none, and
    // the publish rename would orphan an out-link's auto-root anyway.
    nix::ensure_gcroot();
    // The CRDs must be Established before the watch starts, or it fails at startup and the
    // controller sits idle looking healthy. Fail loudly here rather than in production.
    let client = kube::Client::try_default().await.map_err(|e| e.to_string())?;
    let roles = node_roles(&client, &cfg.node).await;
    tracing::info!(node = %cfg.node, ?roles, "node roles");
    let ctx = Arc::new(controller::Ctx::new(client, engine, cfg.node, cfg.pool, cfg.region, roles, nix_client, nix::PROFILES_DIR.into()));
    // Fail closed: no `WS_PEER_SECRET` means no listener at all, never one guarded by an empty
    // secret that would compare-equal to a missing header.
    if let Ok(secret) = std::env::var("WS_PEER_SECRET") {
        if !secret.is_empty() {
            let peer_ctx = ctx.clone();
            tokio::spawn(async move {
                if let Err(e) = peer::serve(&peer_ctx, secret).await {
                    tracing::error!(error = %e, "peer listener exited");
                }
            });
        }
    }
    controller::run(ctx).await
}


/// The roles this node advertises. An unreadable Node object yields no roles, so the agent
/// converges what it already owns and claims nothing new — the safe direction, since the
/// alternative is claiming work for a pool this box may not have.
async fn node_roles(client: &kube::Client, node: &str) -> Vec<String> {
    let api: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(client.clone());
    let Ok(Some(n)) = api.get_opt(node).await else {
        tracing::warn!(%node, "could not read this node's labels: claiming no unplaced work");
        return vec![];
    };
    let labels = n.metadata.labels.unwrap_or_default();
    let roles: Vec<String> = ["session", "env"]
        .into_iter()
        .filter(|r| labels.get(&format!("rustic-git.io/{r}")).map(String::as_str) == Some("true"))
        .map(str::to_string)
        .collect();
    if roles.is_empty() {
        // Zero roles means zero claim watches, and an agent with no claim watch looks identical to
        // a healthy one from the outside — it just never picks anything up. Say so.
        tracing::warn!(%node, "no rustic-git.io/session or /env label: this node claims no unplaced work");
    }
    roles
}


/// `Engine::push`'s detached `squash <ws-id>` child (`ops.rs`) is spawned with only the
/// workspace id, no owner — so the id -> owner mapping has to be recoverable locally.
/// `MetaStore` has no owner-less lookup (Cosmos partitions by owner), so the controller leaves a
/// breadcrumb on the pool itself, right where the lineage file already lives.
pub fn owner_file(pool: &str, ws_id: &str) -> std::path::PathBuf {
    std::path::Path::new(pool).join("vol").join(format!("{ws_id}.owner"))
}

fn record_owner(pool: &str, id: &str, owner: &str) {
    let _ = std::fs::create_dir_all(std::path::Path::new(pool).join("vol"));
    let _ = std::fs::write(owner_file(pool, id), owner);
}
