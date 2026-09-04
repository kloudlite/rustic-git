//! Process setup for `rustic-git-agent`: the local `Engine`, the storage janitor (`janitor.rs`),
//! and the
//! Kubernetes client the node controller (`controller/`) reconciles with. The work itself is
//! there, not here — the CRD IS the work item, so there is no queue, no lease and no poll loop.
//!
//! # Tests that do not run in CI
//!
//! CI has no loopback btrfs and no root. `janitor::cleanup_local_deletes_nested_worktree_subvolumes`
//! is the only test gated explicitly on `have_btrfs()` and the only one exercising `cleanup_local`
//! against real subvolumes; every other test of that path proves `btrfs_delete`'s test-only
//! `remove_dir_all` fallback instead. Several more pass on a Mac only because the code
//! short-circuits before touching btrfs (each carries an `IMPLICITLY GATED` doc line saying which
//! short-circuit). If you change the engine so a path that used to return early now shells out,
//! those tests keep passing here and fail on a node — run `tests/ws_e2e.sh` on the Linux VM.

use rustic_git_core::settings::LiveSettings;
use rustic_git_workspaces::crd;
use rustic_git_workspaces::engine::{Engine, Pool};
use rustic_git_workspaces::settings::AgentSettings;
use std::sync::Arc;

pub mod binding;
pub mod claim;
pub mod controller;
pub mod decommission;
pub mod janitor;
pub mod listing;
pub mod nix;
pub mod peer;
pub mod snapshot;
pub mod sshkeys;
pub mod stats;
pub mod sync;
#[cfg(test)]
mod testsupport;

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
/// Whether an existing mount at `target` still ANSWERS. A mount can be listed in `/proc/mounts`
/// and be a corpse: the NFS transport lives in the network namespace of whoever called `mount(2)`,
/// so when the agent pod that made it is deleted, the namespace dies and the mount survives as an
/// entry that blocks forever on first touch (`hard`). A restarted agent would see it listed, skip
/// remounting, and then hang before the controller ever starts — with the pod reporting 2/2
/// Running the whole time. Presence is not liveness; this asks.
///
/// `-s KILL`, because `timeout`'s default SIGTERM is exactly the signal a process wedged on a
/// `hard` NFS mount ignores: it sleeps uninterruptibly and only SIGKILL breaks an NFS wait. With
/// the default, `timeout` would send TERM and then wait forever for a child that never dies —
/// hanging on the very corpse this probe exists to detect.
fn mount_answers(target: &str) -> bool {
    // A READDIR, not `stat -f`: statfs is answered off the mount's superblock and kept succeeding
    // on a mount whose every real operation returned EIO after the NFS server moved to another
    // node (new ZeroFS process, new file handles). Listing the root walks a handle the server
    // must actually recognise.
    std::process::Command::new("timeout")
        .args(["-s", "KILL", "5", "ls", target])
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
    // `rsplit_once`: the path always starts at the LAST colon, so an IPv6 host (`fd00::1:/`)
    // keeps its own colons.
    let (host, path) = export.rsplit_once(':').ok_or_else(|| format!("{export}: expected host:/path"))?;
    let host = host.trim_matches(|c| c == '[' || c == ']');
    let addr = (host, 2049)
        .to_socket_addrs()
        .map_err(|e| format!("resolving {host}: {e}"))?
        .next()
        .ok_or_else(|| format!("{host} resolved to no address"))?;
    // A dual-stack resolver can answer AAAA first; mount.nfs needs a v6 address bracketed.
    Ok(match addr.ip() {
        std::net::IpAddr::V6(v6) => format!("[{v6}]:{path}"),
        std::net::IpAddr::V4(v4) => format!("{v4}:{path}"),
    })
}

/// Idempotent and re-entrant from the reconcile path, not only from boot: a ZeroFS pod that moves
/// nodes leaves every client's mount stale, and the fix (detach, remount) is the same one boot
/// runs. Serialised so two reconciles cannot race an unmount against a mount.
pub(crate) fn mount_homes(pool: &str, export: &str) -> Result<(), String> {
    static REPAIR: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = REPAIR.lock().unwrap_or_else(|e| e.into_inner());
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
        let st = std::process::Command::new("umount")
            .args(["-f", "-l", target_str])
            .status()
            .map_err(|e| format!("running umount: {e}"))?;
        if !st.success() {
            // Never mount over an undetached corpse, and never let `create_dir_all` below turn
            // this into the bare "File exists" that hid the real cause once already.
            return Err(format!("unmounting the stale shared home at {target_str} failed: {st}"));
        }
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
    // `-s KILL` for the same reason as `mount_answers`: a mount.nfs stuck inside the mount
    // syscall ignores SIGTERM.
    let st = std::process::Command::new("timeout")
        .args(["-s", "KILL", "60"])
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
    // Env-only, ahead of `LiveSettings` — this validates the pin the process is ABOUT to build
    // with, before anything (including the settings merge below) can build with it.
    let pin = nix::nixpkgs_pin_env();
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
    // Resolved BEFORE `Ctx`: `Ctx::new` reads the boot-marked fields (`default_image`,
    // `git_init_image`, `runtime_class`) straight off this handle instead of `std::env` itself,
    // so the CRD's admin-written value is what a fresh pod boots with, not just what a running
    // one picks up later.
    let settings = LiveSettings::new(initial_settings(&client).await);
    // The gauges the collector cannot get from the kubelet: the btrfs pool is this process's
    // filesystem to read, and "working copies running here" is this node's own view. Must run
    // before `Ctx::new` below, which moves `cfg.pool`/`cfg.node`.
    stats::spawn_stats(cfg.pool.clone(), client.clone(), cfg.node.clone());
    let ctx = Arc::new(controller::Ctx::new(client.clone(), engine, cfg.node, cfg.pool, cfg.region, roles, cfg.homes_export, nix_client, nix::PROFILES_DIR.into(), settings.clone()));
    spawn_settings_reflector(client, settings);
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

/// `SETTINGS_REFRESH_SECS`, default 30 — bootstrap-only, so this one stays a plain env read even
/// though everything it governs is now live: there is no live source to refresh THIS with.
fn settings_refresh_interval() -> std::time::Duration {
    std::time::Duration::from_secs(std::env::var("SETTINGS_REFRESH_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(30))
}

/// The one-shot GET `Ctx::new` needs before it can build a pod template. Missing object or a
/// failed GET falls back to `AgentSettings::from_env()` alone — first boot, or a region that has
/// never had a `ClusterSettings/default` written; the reflector spawned right after this will
/// pick up the real one the moment it exists.
async fn initial_settings(client: &kube::Client) -> AgentSettings {
    let base = AgentSettings::from_env();
    let api: kube::Api<crd::ClusterSettings> = kube::Api::all(client.clone());
    match api.get_opt("default").await {
        Ok(Some(obj)) => base.merged_with(&obj.spec),
        Ok(None) => base,
        Err(e) => {
            tracing::warn!(error = %e, "boot: could not read ClusterSettings/default; starting from env/default alone");
            base
        }
    }
}

/// Keeps `settings` live for the rest of the process: a watch on the single `default` object —
/// the first CLUSTER-WIDE singleton watch this agent does, everything else shards by node — plus
/// a periodic re-GET (`settings_refresh_interval`) as the backstop for a watch event this node
/// missed (a reconnect gap, an apiserver restart). "Last good wins": a spec that fails to
/// deserialize (a future field, a hand-edit with the wrong type) surfaces as a stream error, is
/// logged once, and changes nothing — the process keeps whatever it last applied.
fn spawn_settings_reflector(client: kube::Client, settings: LiveSettings<AgentSettings>) {
    tokio::spawn(async move {
        use futures::StreamExt;
        use kube::runtime::{watcher, WatchStreamExt};
        let api: kube::Api<crd::ClusterSettings> = kube::Api::all(client.clone());
        let cfg = watcher::Config::default().fields("metadata.name=default");
        let mut events = std::pin::pin!(watcher(api.clone(), cfg).default_backoff().applied_objects());
        let mut tick = tokio::time::interval(settings_refresh_interval());
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                event = events.next() => {
                    match event {
                        Some(Ok(obj)) => apply_settings(&api, &settings, obj).await,
                        // A malformed spec never reaches here as `Ok` — kube-runtime's own
                        // decode failed first, which IS the "logged once, changes nothing" case.
                        Some(Err(e)) => tracing::warn!(error = %e, "settings watch: last good wins"),
                        None => return,
                    }
                }
                _ = tick.tick() => {
                    if let Ok(Some(obj)) = api.get_opt("default").await {
                        apply_settings(&api, &settings, obj).await;
                    }
                }
            }
        }
    });
}

/// The store, plus the one write-back the spec asks for: `status.observedGeneration`, so the
/// admin UI's pending marker has something to compare `metadata.generation` against. Never called
/// from the boot-time `initial_settings` load — only a watch/refresh event that actually reached
/// this node earns the write.
async fn apply_settings(api: &kube::Api<crd::ClusterSettings>, settings: &LiveSettings<AgentSettings>, obj: crd::ClusterSettings) {
    settings.store(AgentSettings::from_env().merged_with(&obj.spec));
    let body = serde_json::json!({
        "apiVersion": format!("{}/{}", crd::GROUP, crd::VERSION),
        "kind": "ClusterSettings",
        "status": {"observedGeneration": obj.metadata.generation},
    });
    let params = kube::api::PatchParams::apply(crd::AGENT_FIELD_MANAGER).force();
    if let Err(e) = api.patch_status("default", &params, &kube::api::Patch::Apply(&body)).await {
        tracing::warn!(error = %e, "settings: writing status.observedGeneration");
    }
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
    use super::{already_mounted, resolve_export};

    /// Literal addresses only — `to_socket_addrs` on a literal never touches DNS, so this pins the
    /// parsing (last-colon split, v6 brackets) without a resolver in the test.
    #[test]
    fn resolve_export_splits_at_the_last_colon_and_brackets_ipv6() {
        assert_eq!(resolve_export("10.43.1.2:/").unwrap(), "10.43.1.2:/");
        assert_eq!(resolve_export("10.43.1.2:/homes").unwrap(), "10.43.1.2:/homes");
        assert_eq!(resolve_export("fd00::1:/").unwrap(), "[fd00::1]:/");
        assert_eq!(resolve_export("[fd00::1]:/homes").unwrap(), "[fd00::1]:/homes");
        assert!(resolve_export("no-colon").is_err());
    }

    #[test]
    fn already_mounted_matches_the_target_column_exactly() {
        let mounts = "zerofs:/ /wspool-prod/homes nfs rw 0 0\nother /wspool-prod/homes2 nfs rw 0 0\n";
        assert!(already_mounted(mounts, "/wspool-prod/homes"));
        assert!(!already_mounted(mounts, "/wspool-prod/home"));
    }
}
