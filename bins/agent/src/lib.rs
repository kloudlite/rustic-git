//! Process setup for `rustic-git-agent`: the local `Engine`, the storage janitor (`janitor.rs`),
//! and the
//! Kubernetes client the node controller (`controller.rs`) reconciles with. The work itself is
//! there, not here — the CRD IS the work item, so there is no queue, no lease and no poll loop.

use rustic_git_workspaces::engine::{Engine, Pool};
use std::sync::Arc;

pub mod binding;
pub mod claim;
pub mod controller;
pub mod janitor;
pub mod nix;
pub mod peer;
pub mod snapshot;
pub mod sshkeys;

/// Env-derived config for `run`. `WS_REGISTRY_URL`/`WS_AGENT_TOKEN` are gone with the
/// object-store registry surface they pointed at (Task 8).
pub struct Config {
    pub region: String,
    pub pool: String,
    /// This node's name, from the downward-API `NODE_NAME`. It is the shard key: the controller
    /// watches only objects whose `spec.nodeName` equals it.
    pub node: String,
    /// `WS_HOMES_EXPORT`, e.g. `zerofs.rustic-git-system.svc:/` — the region's shared-home NFS
    /// export. Unset means no shared home on this node: workspace reconciles that need it park on
    /// HomeNotReady (fail closed, same shape as WS_PEER_SECRET gating the peer listener).
    pub homes_export: Option<String>,
}

impl Config {
    pub fn from_env() -> Config {
        Config {
            region: std::env::var("WS_REGION").unwrap_or_else(|_| "default".into()),
            pool: std::env::var("WS_POOL").unwrap_or_else(|_| "/mnt/wspool".into()),
            // Declared capacity is gone: the kubelet reports node allocatable, and a second
            // hand-maintained copy of it is a second thing that can be wrong.
            node: std::env::var("NODE_NAME").unwrap_or_default(),
            homes_export: std::env::var("WS_HOMES_EXPORT").ok().filter(|v| !v.is_empty()),
        }
    }
}

/// `{pool}/homes` — where the region's shared-home export is mounted, one mount per node.
pub fn homes_root(pool: &str) -> std::path::PathBuf {
    std::path::Path::new(pool).join("homes")
}

/// True when `target` already appears as a mount point's column-2 entry in `/proc/mounts`'s
/// contents. Split out from `mount_homes` so the parse decision is testable without root or NFS.
fn already_mounted(mounts: &str, target: &str) -> bool {
    mounts.lines().any(|l| l.split_whitespace().nth(1) == Some(target))
}

/// Refuses a `{pool}/homes` that is not a mount point, given `/proc/mounts`'s contents.
///
/// `mount_homes` runs ONCE, at agent boot, and everything after it assumes the export is still
/// there — silently wrong after an operator `umount`, an ESTALE from a re-created ZeroFS, or a
/// mount that never established. `create_dir_all` under a missing mount point then manufactures a
/// directory on the node's rootfs and the pod gets an EMPTY home, reported Ready; the hostPath
/// `type: Directory` guard cannot catch it, because the agent made the directory first. So every
/// materialize re-checks.
pub(crate) fn check_homes_mounted(mounts: &str, pool: &str) -> Result<(), String> {
    let target = homes_root(pool);
    let Some(target) = target.to_str() else {
        return Err(format!("{} is not valid UTF-8", target.display()));
    };
    if already_mounted(mounts, target) {
        return Ok(());
    }
    Err(format!("the shared-home NFS export is not mounted at {target}; refusing to serve a home off the node's rootfs"))
}

/// Whether an existing mount at `target` still ANSWERS. A mount can be listed in `/proc/mounts`
/// and be a corpse: the NFS transport lives in the network namespace of whoever called `mount(2)`,
/// so when the agent pod that made it is deleted, the namespace dies and the mount survives as an
/// entry that blocks forever on first touch (`hard`). A restarted agent would see it listed, skip
/// remounting, and then hang before the controller ever starts — with the pod reporting 2/2
/// Running the whole time. Presence is not liveness; this asks.
fn mount_answers(target: &str) -> bool {
    std::process::Command::new("timeout")
        .args(["5", "stat", "-f", target])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|st| st.success())
        .unwrap_or(false)
}

/// `host:/path` with `host` resolved to an address. The mount runs in the HOST's network
/// namespace (see `mount_homes`), where cluster DNS does not exist — so the Service name has to be
/// resolved HERE, in the pod, and handed on as an address. A ClusterIP is stable for the life of
/// the Service; recreating the Service means restarting the agents, which is the same restart the
/// mount already needs.
fn resolve_export(export: &str) -> Result<String, String> {
    use std::net::ToSocketAddrs;
    let (host, path) = export.split_once(':').ok_or_else(|| format!("{export}: expected host:/path"))?;
    let addr = (host, 2049)
        .to_socket_addrs()
        .map_err(|e| format!("resolving {host}: {e}"))?
        .next()
        .ok_or_else(|| format!("{host} resolved to no address"))?;
    Ok(format!("{}:{path}", addr.ip()))
}

fn mount_homes(pool: &str, export: &str) -> Result<(), String> {
    let target = homes_root(pool);
    let Some(target_str) = target.to_str() else {
        return Err(format!("{} is not valid UTF-8", target.display()));
    };
    // The stale-mount repair MUST come before `create_dir_all`: on a node carrying a wedged mount
    // every syscall against this path is already answered by a dead NFS client, and `create_dir_all`
    // fails EEXIST (mkdir says it exists, the stat that would confirm it is a directory cannot get
    // an answer). Creating the directory first is what made the agent die with a bare
    // "File exists (os error 17)" and never reach the repair that would have fixed it.
    let mounts = std::fs::read_to_string("/proc/mounts").map_err(|e| e.to_string())?;
    if already_mounted(&mounts, target_str) {
        if mount_answers(target_str) {
            return Ok(());
        }
        // Listed but dead — the previous agent pod's namespace took the transport with it. Lazy
        // AND forced: lazy detaches the tree even though the workspace pods still hold it open,
        // forced stops the kernel waiting on a server that will never answer this client again.
        tracing::warn!(target = %target_str, "shared home is mounted but not answering; unmounting the stale mount before remounting");
        // In this pod's own mount namespace: `Bidirectional` propagation carries the detach out
        // to the node, and the host filesystem has no umount.nfs helper to reach anyway.
        let _ = std::process::Command::new("umount").args(["-f", "-l", target_str]).status();
    }
    // Safe only now: either nothing was mounted here, or the corpse above has been detached, so
    // the path is a plain directory the kernel can answer for.
    std::fs::create_dir_all(&target).map_err(|e| format!("creating {}: {e}", target.display()))?;
    // `port=2049,mountport=2049` are NOT optional and NOT tuning: NFSv3 normally finds mountd by
    // asking rpcbind on port 111, and ZeroFS runs no rpcbind — without both, `mount.nfs` blocks
    // forever on a portmapper that will never answer, and because this runs before the controller
    // starts the agent then sits at 2/2 Running doing nothing at all. Upstream's README documents
    // exactly this option set.
    //
    // hard: a flapping ZeroFS must block, not corrupt (spec ruling). vers=3: ZeroFS serves NFSv3.
    // nolock: no NLM sideband — append-mode files are node-local by design. r/wsize 1 MiB: the
    // default 128 KiB triples the round trips on the config reads that dominate this mount.
    //
    // `retry=0` plus the outer `timeout` are belt and braces: retry=0 stops mount.nfs re-trying a
    // dead server for two minutes, and the timeout means even a wedge that survives that surfaces
    // as a failed startup — which the DaemonSet restarts and an operator can see.
    let opts = "vers=3,tcp,port=2049,mountport=2049,nolock,hard,async,rsize=1048576,wsize=1048576,retry=0";
    // `nsenter -t 1 -n` — pid 1's NETWORK namespace only, deliberately not `-m`. The transport is
    // the part that has to outlive this pod: created in the node's netns it survives every agent
    // restart, whereas one created in the pod's netns dies with the pod and leaves a mount that
    // blocks forever on `hard`. The MOUNT namespace stays the container's on purpose — `-m` would
    // switch to the host's filesystem, where `/sbin/mount.nfs` does not exist (it ships in this
    // image, not on the node), and `Bidirectional` propagation publishes the mount to the node
    // regardless. NOT `hostNetwork: true`, which would also fix the lifetime but take the agent
    // out of reach of the `agent-peer` NetworkPolicy restricting the peer listener on 8444.
    let addr_export = resolve_export(export)?;
    let st = std::process::Command::new("timeout")
        .arg("60")
        .args(["nsenter", "-t", "1", "-n", "--", "mount", "-t", "nfs", "-o", opts])
        .arg(&addr_export)
        .arg(&target)
        .status()
        .map_err(|e| e.to_string())?;
    if st.success() {
        Ok(())
    } else {
        Err(format!(
            "mount {addr_export} (from {export}) at {} failed: {st} (124 = timed out; check the export answers on 2049)",
            target.display()
        ))
    }
}

/// Boots the node controller: Engine, janitor, Kubernetes client, then reconcile forever.
pub async fn run(cfg: Config) -> Result<(), String> {
    let engine = Arc::new(Engine::new(Pool::new(&cfg.pool)));
    // Fail closed like the pin/CRD checks below: a shared home the agent claims to serve but
    // cannot actually reach is worse than the DaemonSet restart loop this causes.
    if let Some(export) = &cfg.homes_export {
        mount_homes(&cfg.pool, export)?;
    }
    let nix_client: Arc<dyn nix::Nix> = Arc::new(nix::RealNix { bin: "/nix/var/nix/profiles/default/bin".into() });
    janitor::spawn_janitor(cfg.pool.clone(), nix_client.clone());
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
    let ctx = Arc::new(controller::Ctx::new(client, engine, cfg.node, cfg.pool, cfg.region, roles, cfg.homes_export, nix_client, nix::PROFILES_DIR.into()));
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



#[cfg(test)]
mod tests {
    use super::{already_mounted, check_homes_mounted};

    /// The mount-liveness hole: with the export gone, `ensure_shared_home` would otherwise mkdir
    /// on the node's rootfs and hand the pod a silently empty home.
    #[test]
    fn a_homes_root_that_is_not_a_mount_point_is_refused() {
        let mounts = "zerofs:/ /wspool-prod/homes nfs rw 0 0\n";
        assert!(check_homes_mounted(mounts, "/wspool-prod").is_ok());
        let err = check_homes_mounted("/dev/sda1 / ext4 rw 0 0\n", "/wspool-prod").unwrap_err();
        assert!(err.contains("not mounted"), "{err}");
    }

    #[test]
    fn already_mounted_matches_the_target_column_exactly() {
        let mounts = "zerofs:/ /wspool-prod/homes nfs rw 0 0\nother /wspool-prod/homes2 nfs rw 0 0\n";
        assert!(already_mounted(mounts, "/wspool-prod/homes"));
        assert!(!already_mounted(mounts, "/wspool-prod/home"));
    }
}
