//! Shared plumbing for the live-settings tiers (`ClusterSettings`/`CentralSettings`): only the
//! bit both tiers' meta tables need, so `crates/workspaces` and `crates/api` (Task 2+) don't each
//! invent their own copy that can drift.

/// Whether a setting can change on the next refresh beat, or only takes effect at process start
/// because it feeds a pod template / env var read once at boot (e.g. an image tag — changing it
/// mid-process would not restart anything, so it must instead be readers restarting to pick it up).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Live,
    Boot,
}
