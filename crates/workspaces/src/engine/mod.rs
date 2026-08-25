//! Snapshot engine: workspace = btrfs subvolume, snapshot = RO snapshot, delta = incremental
//! `btrfs send -p` stream, zstd-compressed, stored in the region's object store.
//!
//! Ported from `docs/superpowers/poc/wssnap/main.rs` (Azure-tested). Lineage model: every
//! layer blob is immutable, named by a UUID; a snapshot record stores the FULL ordered list
//! of layer entries from the base up to itself, so records are freely deletable and clones
//! share ancestors' blobs. `model::LineageEntry` carries the `s:{blob}:{sha}` /
//! `b:{blob}:{snap}:{sha}` encoding via `encode`/`parse`/`snap_name`.
//!
//! Object-store layout:
//!   layers/{uuid}.zst      zstd send stream or zstd block image
//!   snaps/{uuid}.json      {"lineage": ["s:...", "b:...:...", ...]}
//!   refs/{ws}              snapshot record uuid
//!
//! Pool layout:
//!   {pool}/vol/{name}/live    RW subvolume; for a block-restored workspace, {pool}/vol/{name}
//!                            is a loop mount of the image — its own filesystem. "vol" not "ws":
//!                            environments live here too, matching the registry's vol/{owner}/{id}.
//!   {pool}/vol/{name}.lineage local ordered entry list for live (outside the mount on purpose)
//!   {pool}/recv/{snap}       RO snapshots on the shared pool fs — the local layer cache.
//!   {pool}/img/{blob}.img    decompressed block images backing mounted workspaces.
//!
//! Requires root: btrfs subvolume/send/receive/mount need it.

pub mod blob;
pub mod fsck;
pub mod ops;
pub mod pool;

pub use fsck::FsckReport;
pub use ops::{CloneOut, EngErr, Engine, PullOut, PushOut};
pub use pool::{Pool, is_mountpoint, migrate_ws_to_vol, ws_lock};

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
