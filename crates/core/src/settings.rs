//! Shared plumbing for the live-settings tiers (`ClusterSettings`/`CentralSettings`): only the
//! bit both tiers' meta tables need, so `crates/workspaces` and `crates/api` (Task 2+) don't each
//! invent their own copy that can drift.

use arc_swap::ArcSwap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Whether a setting can change on the next refresh beat, or only takes effect at process start
/// because it feeds a pod template / env var read once at boot (e.g. an image tag — changing it
/// mid-process would not restart anything, so it must instead be readers restarting to pick it up).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Live,
    Boot,
}

/// One handle shape for every process that reads a settings document on a beat: load the
/// current value with no lock contention on the hot path (`ArcSwap::load` is a single atomic
/// read), swap in a new one from the refresh beat. Generic so the agent's `AgentSettings` and
/// the central tier's `CentralSettings` share one type instead of two hand-rolled RwLocks.
#[derive(Clone)]
pub struct LiveSettings<T>(Arc<ArcSwap<T>>);

impl<T> LiveSettings<T> {
    pub fn new(initial: T) -> Self {
        Self(Arc::new(ArcSwap::from_pointee(initial)))
    }

    /// The current value. Cheap enough to call at the top of every beat iteration — that is
    /// the whole point.
    pub fn load(&self) -> Arc<T> {
        self.0.load_full()
    }

    /// Refresh beats call this after a successful parse. Never called on a parse failure —
    /// "last good wins" is enforced by the CALLER simply not calling this, not by anything
    /// here.
    pub fn store(&self, new: T) {
        self.0.store(Arc::new(new));
    }
}

/// Central-tier tunables (server/worker/gateway/api), sourced from `RUSTIC_GIT_*` env at boot and
/// overridable from the `cluster/settings` object-store document thereafter. Every numeric field
/// has a range enforced by the admin write path (Task 4/5), not here — a struct with an
/// out-of-range value can still exist transiently between `from_env()` and the first merge.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct CentralSettings {
    /// Git request body cap, bytes. 1048576..=8589934592.
    pub max_body: u64,
    /// Registry blob upload cap, bytes (matches S3's single-request CopyObject limit).
    /// 1048576..=21474836480.
    pub max_layer: u64,
    /// Registry manifest body cap, bytes. 65536..=67108864.
    // ponytail: no caller reads this yet; ships for the admin UI ahead of the manifest-size gate
    // it is meant for.
    pub max_manifest: u64,
    /// How long an unfinished upload session is kept before GC reclaims it. 3600..=604800 seconds.
    pub upload_grace_secs: u64,
    /// Registry GC sweep interval. 300..=86400 seconds.
    // ponytail: unread until a caller needs it — see Task 4 Step 4's grep.
    pub gc_interval_secs: u64,
    /// Merge-worker lease TTL. 30..=3600 seconds.
    // ponytail: unread until a caller needs it — see Task 4 Step 4's grep.
    pub merge_lease_secs: u64,
    /// `announce_stranded_merges`'s beat interval. 5..=300 seconds.
    // ponytail: unread until a caller needs it — see Task 4 Step 4's grep.
    pub announce_stranded_secs: u64,
    /// Activity-feed retention. 3600..=2592000 seconds.
    // ponytail: not currently enforced anywhere; field is a no-op until a caller reads it.
    pub feed_retention_secs: u64,
    /// Host shown in `git clone` instructions. Unbounded string, empty means "unset".
    pub clone_host: String,
    /// SSH host for workspace tunnels. Unbounded string, empty means "unset".
    pub ssh_host: String,
    /// SSH port for workspace tunnels. 1..=65535.
    pub ssh_port: u16,
    /// Host shown for `docker pull` instructions. Unbounded string, empty means "unset".
    pub registry_host: String,
    /// Whether new-account signup is open. No current gate reads this.
    // ponytail: no signup gate exists today; ships true and is a no-op until one does.
    pub signup_open: bool,
}

impl Default for CentralSettings {
    fn default() -> Self {
        Self::from_env()
    }
}

impl CentralSettings {
    /// Env ?? built-in default, one field per existing `RUSTIC_GIT_*` var — this is the FLOOR
    /// `merged_with` overrides from the stored document, never the other way around.
    pub fn from_env() -> Self {
        fn env_u64(key: &str, default: u64) -> u64 {
            std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
        }
        fn env_u16(key: &str, default: u16) -> u16 {
            std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
        }
        fn env_string(key: &str) -> String {
            std::env::var(key).unwrap_or_default()
        }

        Self {
            max_body: env_u64("RUSTIC_GIT_MAX_BODY", 2_147_483_648),
            max_layer: env_u64("RUSTIC_GIT_MAX_LAYER", 5_368_709_120),
            max_manifest: 4_194_304,
            upload_grace_secs: env_u64("RUSTIC_GIT_UPLOAD_GRACE_SECS", 86_400),
            gc_interval_secs: 3_600,
            merge_lease_secs: 300,
            announce_stranded_secs: 15,
            feed_retention_secs: 604_800,
            clone_host: env_string("RUSTIC_GIT_CLONE_HOST"),
            ssh_host: env_string("RUSTIC_GIT_SSH_HOST"),
            ssh_port: env_u16("RUSTIC_GIT_SSH_PORT", 22),
            registry_host: env_string("RUSTIC_GIT_REGISTRY_HOST"),
            signup_open: true,
        }
    }

    /// `stored ?? env ?? default`: `self` (already env ?? default from `from_env`) provides
    /// the floor, `stored`'s `Option<..>`-shaped twin overrides field by field. The stored
    /// document is `serde(default)` too, so a partial document (one field written, the rest
    /// never touched) merges cleanly.
    pub fn merged_with(mut self, stored: &StoredCentralSettings) -> Self {
        macro_rules! over {
            ($f:ident) => {
                if let Some(v) = stored.$f.clone() {
                    self.$f = v;
                }
            };
        }
        over!(max_body);
        over!(max_layer);
        over!(max_manifest);
        over!(upload_grace_secs);
        over!(gc_interval_secs);
        over!(merge_lease_secs);
        over!(announce_stranded_secs);
        over!(feed_retention_secs);
        over!(clone_host);
        over!(ssh_host);
        over!(ssh_port);
        over!(registry_host);
        over!(signup_open);
        self
    }
}

/// The wire type at the object-store key `cluster/settings`. Every field `Option<_>` so a
/// document that has never touched (say) `maxBody` doesn't silently coerce it to `0` on
/// deserialize — `CentralSettings::merged_with` only overrides the fields actually set here.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, rename_all = "camelCase")]
pub struct StoredCentralSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_body: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_layer: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_manifest: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_grace_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gc_interval_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_lease_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub announce_stranded_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feed_retention_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clone_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signup_open: Option<bool>,
    /// Last ten versions, newest first, kept inline rather than as ten separate object-store
    /// keys — one small object either way, and one GET beats eleven.
    #[serde(default)]
    pub history: Vec<StoredCentralSettingsSnapshot>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated_by: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated_at: String,
}

/// A past version pushed onto `history` on every write, minus `history` itself (no nesting).
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, rename_all = "camelCase")]
pub struct StoredCentralSettingsSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_body: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_layer: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_manifest: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_grace_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gc_interval_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_lease_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub announce_stranded_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feed_retention_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clone_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signup_open: Option<bool>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated_by: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `stored ?? env ?? default` precedence: an env var sets one field, the stored document
    /// overrides a different field, and a third field neither touches must land on the
    /// built-in default — proves the merge order, not range validation (that's the admin write
    /// path's job, Task 4/5).
    #[test]
    fn merge_precedence() {
        // SAFETY: test-only, single-threaded within this test's scope.
        unsafe {
            std::env::set_var("RUSTIC_GIT_SSH_PORT", "2222");
        }
        let base = CentralSettings::from_env();
        assert_eq!(base.ssh_port, 2222, "env must override the built-in default");

        let stored = StoredCentralSettings { max_body: Some(999), ..Default::default() };
        let merged = base.clone().merged_with(&stored);
        assert_eq!(merged.max_body, 999, "stored must override env/default");
        assert_eq!(merged.ssh_port, 2222, "a field the stored doc never touched keeps the env value");
        assert_eq!(
            merged.upload_grace_secs, 86_400,
            "a field neither env nor stored touched keeps the built-in default"
        );

        unsafe {
            std::env::remove_var("RUSTIC_GIT_SSH_PORT");
        }
    }

    /// `LiveSettings::load`/`store` round-trip: last good wins is the CALLER's job (nothing here
    /// enforces it), so this only proves the handle itself swaps and reads correctly.
    #[test]
    fn live_settings_round_trips() {
        let live = LiveSettings::new(CentralSettings::from_env());
        assert_eq!(live.load().ssh_port, 22);
        let mut next = (*live.load()).clone();
        next.ssh_port = 2200;
        live.store(next);
        assert_eq!(live.load().ssh_port, 2200);
    }
}
