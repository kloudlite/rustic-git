//! The snapshot model: a volume's history is local btrfs subvolumes under `snap/`, one per
//! `Snapshot` CR, replicated peer-to-peer (`bins/agent/src/peer/`) — never an object store,
//! never uploaded anywhere. `snapshot.rs` holds the snapshot/checkout primitives; `ops.rs` holds
//! what predates and still serves both the snapshot model and the old single-subvolume layout
//! (subvolume creation, quota) plus the one clone path environments still use.
//!
//! Pool layout:
//!   {pool}/vol/{name}/live         old layout: the RW subvolume directly, or (snapshot model,
//!                                 migrated) a DIRECTORY of worktree subvolumes, one per
//!                                 workspace checked out against this volume (`Pool::worktree`).
//!   {pool}/vol/{name}/snap/{name}  snapshot model: one RO subvolume per snapshot (`Pool::snap`).
//!   {pool}/repl/{name}             replica transfer staging (`Pool::repl`).
//!
//! Requires root: btrfs subvolume/send/receive/mount need it.

pub mod snapshot;
pub mod ops;
pub mod pool;

pub use ops::{is_subvolume, EngErr, Engine};
pub use pool::{Pool, is_mountpoint, ws_lock};

/// True when `btrfs` is on PATH and this process is root — every subvolume/send/receive/mount
/// call below needs both, so tests gate on this and skip cleanly where it's false (e.g. this
/// Mac, or any non-root CI runner).
pub fn have_btrfs() -> bool {
    let has_binary = std::process::Command::new("btrfs")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    has_binary && unsafe { libc::geteuid() } == 0
}
