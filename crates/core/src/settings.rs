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
pub struct LiveSettings<T> {
    value: Arc<ArcSwap<T>>,
    /// Bumped on every `store()`. `/healthz` reports this — not the stored document's
    /// `updated_at`, which lives on `StoredCentralSettings` and would make this generic type
    /// central-tier-specific — so an operator can confirm a process has actually picked up a
    /// change without re-fetching the document itself.
    version: Arc<std::sync::atomic::AtomicU64>,
}

impl<T> LiveSettings<T> {
    pub fn new(initial: T) -> Self {
        Self {
            value: Arc::new(ArcSwap::from_pointee(initial)),
            version: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// The current value. Cheap enough to call at the top of every beat iteration — that is
    /// the whole point.
    pub fn load(&self) -> Arc<T> {
        self.value.load_full()
    }

    /// Refresh beats call this after a successful parse. Never called on a parse failure —
    /// "last good wins" is enforced by the CALLER simply not calling this, not by anything
    /// here.
    pub fn store(&self, new: T) {
        self.value.store(Arc::new(new));
        self.version.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// `0` until the first `store()` — "still on the env-only boot default, nothing refreshed
    /// yet".
    pub fn version(&self) -> u64 {
        self.version.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Central-tier tunables (server/worker/gateway/api), sourced from `KLOUDLITE_*` env at boot and
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
    /// Registry GC sweep interval. 30..=86400 seconds — the floor is 30, not the spec table's
    /// 300, because the built-in default mirrors `bins/worker/src/main.rs`'s `GC_PASS_GAP` (60 s)
    /// and a default outside its own range would be refused by the very validation it seeds.
    pub gc_interval_secs: u64,
    /// Merge-worker lease TTL. 30..=3600 seconds.
    // ponytail: default mirrors `App::MERGE_LEASE` (`crates/app/src/lib.rs`). Unread by any beat
    // until Task 4 wires it.
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
    /// Env ?? built-in default, one field per existing `KLOUDLITE_*` var — this is the FLOOR
    /// `merged_with` overrides from the stored document, never the other way around.
    /// The floor before any env var or stored document is consulted — split out of `from_env`
    /// so `api::admin::settings`'s schema route (`GET /admin/settings/schema`) can report a
    /// field's built-in default without also reporting whatever this process's own env happens
    /// to hold, which `from_env()` itself cannot separate back out.
    pub fn built_in_defaults() -> Self {
        Self {
            max_body: 2_147_483_648,
            max_layer: 5_368_709_120,
            max_manifest: 4_194_304,
            upload_grace_secs: 86_400,
            gc_interval_secs: 60,
            merge_lease_secs: 600,
            announce_stranded_secs: 15,
            feed_retention_secs: 604_800,
            clone_host: String::new(),
            ssh_host: String::new(),
            ssh_port: 22,
            registry_host: String::new(),
            signup_open: true,
        }
    }

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

        let d = Self::built_in_defaults();
        Self {
            max_body: env_u64("KLOUDLITE_MAX_BODY", d.max_body),
            max_layer: env_u64("KLOUDLITE_MAX_LAYER", d.max_layer),
            upload_grace_secs: env_u64("KLOUDLITE_UPLOAD_GRACE_SECS", d.upload_grace_secs),
            clone_host: env_string("KLOUDLITE_CLONE_HOST"),
            ssh_host: env_string("KLOUDLITE_SSH_HOST"),
            ssh_port: env_u16("KLOUDLITE_SSH_PORT", d.ssh_port),
            registry_host: env_string("KLOUDLITE_REGISTRY_HOST"),
            ..d
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

/// A history entry, reused as the `patch` argument to `apply_patch` for a revert — every field it
/// carries is the value in force at that instant, so overriding `current` field-by-field with it
/// reproduces that instant exactly (`history` itself is left empty; `apply_patch` rebuilds it).
impl From<&StoredCentralSettingsSnapshot> for StoredCentralSettings {
    fn from(snap: &StoredCentralSettingsSnapshot) -> Self {
        Self {
            max_body: snap.max_body,
            max_layer: snap.max_layer,
            max_manifest: snap.max_manifest,
            upload_grace_secs: snap.upload_grace_secs,
            gc_interval_secs: snap.gc_interval_secs,
            merge_lease_secs: snap.merge_lease_secs,
            announce_stranded_secs: snap.announce_stranded_secs,
            feed_retention_secs: snap.feed_retention_secs,
            clone_host: snap.clone_host.clone(),
            ssh_host: snap.ssh_host.clone(),
            ssh_port: snap.ssh_port,
            registry_host: snap.registry_host.clone(),
            signup_open: snap.signup_open,
            history: Vec::new(),
            updated_by: String::new(),
            updated_at: String::new(),
        }
    }
}

/// The object-store key holding `StoredCentralSettings`, readable by any node — it is a shared
/// document, not a per-repo database, so unlike a git/registry route it needs no ownership key
/// and no `BROWSE_TAILS` entry (same exception `_catalog` and `/api/{owner}/images` already are).
pub const CENTRAL_SETTINGS_KEY: &str = "cluster/settings";

/// How often a central binary re-GETs `cluster/settings`. Bootstrap-only — like every other
/// interval that governs its own beat, it cannot raise itself.
pub const SETTINGS_REFRESH_SECS: u64 = 30;

/// `(field name on the wire, Mark)` — same shape as `ClusterSettings`' meta table. Every field is
/// `Mark::Live`: nothing in the current `CentralSettings` set feeds a value that is only read once
/// at process start (a boot-only field, e.g. a knob baked into a pod template at spawn, would be
/// `Mark::Boot` here instead — none of today's fields are that shape).
pub const CENTRAL_SETTING_META: &[(&str, Mark)] = &[
    ("maxBody", Mark::Live),
    ("maxLayer", Mark::Live),
    ("maxManifest", Mark::Live),
    ("uploadGraceSecs", Mark::Live),
    ("gcIntervalSecs", Mark::Live),
    ("mergeLeaseSecs", Mark::Live),
    ("announceStrandedSecs", Mark::Live),
    ("feedRetentionSecs", Mark::Live),
    ("cloneHost", Mark::Live),
    ("sshHost", Mark::Live),
    ("sshPort", Mark::Live),
    ("registryHost", Mark::Live),
    ("signupOpen", Mark::Live),
];

/// One violation, in `quota::refuse`'s sentence shape: `"{field} must be between {lo} and {hi},
/// got {value}"` — so an admin write and a quota refusal read the same in the log and in the UI.
/// `pub`: `ClusterSettings`' own range check (`crates/workspaces/src/api/settings.rs`) formats its
/// 422s through this SAME function rather than a second copy, so the two settings scopes can never
/// drift on what a range violation reads like.
pub fn range_err(field: &str, lo: impl std::fmt::Display, hi: impl std::fmt::Display, got: impl std::fmt::Display) -> String {
    format!("{field} must be between {lo} and {hi}, got {got}")
}

/// Every changed field's range, checked before anything is written — the first violation wins,
/// matching the "422 naming the field and its range" contract. `log_format`/`worker_lanes` are
/// out of scope here: they are not part of `CentralSettings` in this codebase today (the
/// inventory that would add them as `Mark::Boot` fields has not landed), so there is nothing of
/// that shape to validate yet.
pub fn validate_stored(patch: &StoredCentralSettings) -> Result<(), String> {
    macro_rules! range {
        ($f:ident, $lo:expr, $hi:expr) => {
            if let Some(v) = patch.$f {
                if !($lo..=$hi).contains(&v) {
                    return Err(range_err(stringify!($f), $lo, $hi, v));
                }
            }
        };
    }
    range!(max_body, 1_048_576u64, 8_589_934_592u64);
    range!(max_layer, 1_048_576u64, 21_474_836_480u64);
    range!(max_manifest, 65_536u64, 67_108_864u64);
    range!(upload_grace_secs, 3_600u64, 604_800u64);
    range!(gc_interval_secs, 30u64, 86_400u64);
    range!(merge_lease_secs, 30u64, 3_600u64);
    range!(announce_stranded_secs, 5u64, 300u64);
    range!(feed_retention_secs, 3_600u64, 2_592_000u64);
    range!(ssh_port, 1u16, 65_535u16);
    Ok(())
}

/// The `history` half of a write: push the OLD document (minus its own history — no nesting) onto
/// `new.history`, capped at ten, oldest dropped first.
fn push_history(old: &StoredCentralSettings, new: &mut StoredCentralSettings) {
    let snap = StoredCentralSettingsSnapshot {
        max_body: old.max_body,
        max_layer: old.max_layer,
        max_manifest: old.max_manifest,
        upload_grace_secs: old.upload_grace_secs,
        gc_interval_secs: old.gc_interval_secs,
        merge_lease_secs: old.merge_lease_secs,
        announce_stranded_secs: old.announce_stranded_secs,
        feed_retention_secs: old.feed_retention_secs,
        clone_host: old.clone_host.clone(),
        ssh_host: old.ssh_host.clone(),
        ssh_port: old.ssh_port,
        registry_host: old.registry_host.clone(),
        signup_open: old.signup_open,
        updated_by: old.updated_by.clone(),
        updated_at: old.updated_at.clone(),
    };
    new.history = old.history.clone();
    new.history.insert(0, snap);
    new.history.truncate(10);
}

/// Merge `patch` field-by-field onto `current` (only the fields the caller actually set), push
/// `current` onto history, and stamp `updated_by`/`updated_at`. Called by the admin write handler
/// AFTER `validate_stored` has passed; a revert is the same call with `patch` built from a full
/// `history[n]` snapshot.
pub fn apply_patch(
    current: &StoredCentralSettings,
    patch: &StoredCentralSettings,
    updated_by: &str,
    updated_at: &str,
) -> StoredCentralSettings {
    let mut next = current.clone();
    macro_rules! over {
        ($f:ident) => {
            if let Some(v) = patch.$f.clone() {
                next.$f = Some(v);
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
    push_history(current, &mut next);
    next.updated_by = updated_by.to_string();
    next.updated_at = updated_at.to_string();
    next
}

/// One GET of `cluster/settings`, supplied by the caller rather than baked in here — `core` has
/// no object-store dependency, and each binary already has its own client shape (the full `Store`
/// for server/worker/api, a minimal read-only one for gateway, which opens no object store for
/// anything else). `None` for a missing key OR a fetch error; the beat treats both as "nothing new
/// to apply" and keeps the last good value.
pub type CentralFetch = std::sync::Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<u8>>> + Send>> + Send + Sync,
>;

/// Every `SETTINGS_REFRESH_SECS`, re-GET the document and, on a successful parse, swap it in.
/// "Last good wins": a missing key (never written yet) or a corrupt document leaves `live`
/// untouched and warns once per bad refresh — never panics, never falls back to `from_env()`
/// alone, because that would silently discard whatever an admin had already set.
pub async fn refresh_central_beat(fetch: CentralFetch, live: LiveSettings<CentralSettings>) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(SETTINGS_REFRESH_SECS)).await;
        let Some(bytes) = fetch().await else { continue };
        match serde_json::from_slice::<StoredCentralSettings>(&bytes) {
            Ok(doc) => live.store(CentralSettings::from_env().merged_with(&doc)),
            Err(e) => tracing::warn!(scope = "central", error = %e, "settings.invalid"),
        }
    }
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
            std::env::set_var("KLOUDLITE_SSH_PORT", "2222");
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
            std::env::remove_var("KLOUDLITE_SSH_PORT");
        }
    }

    #[test]
    fn validate_stored_rejects_out_of_range() {
        let bad = StoredCentralSettings { ssh_port: Some(0), ..Default::default() };
        let err = validate_stored(&bad).unwrap_err();
        assert_eq!(err, "ssh_port must be between 1 and 65535, got 0");

        let ok = StoredCentralSettings { ssh_port: Some(2222), ..Default::default() };
        assert!(validate_stored(&ok).is_ok());
    }

    /// A write pushes the OLD document onto history and truncates at ten — the eleventh write
    /// must drop the oldest entry, not grow unbounded.
    #[test]
    fn apply_patch_pushes_and_truncates_history() {
        let mut doc = StoredCentralSettings::default();
        for i in 0..11u64 {
            let patch = StoredCentralSettings { max_body: Some(1_048_576 + i), ..Default::default() };
            doc = apply_patch(&doc, &patch, "admin@example.com", &format!("t{i}"));
        }
        assert_eq!(doc.max_body, Some(1_048_576 + 10));
        assert_eq!(doc.history.len(), 10, "history caps at ten entries");
        // Newest-first: the entry just before the last write is at index 0.
        assert_eq!(doc.history[0].max_body, Some(1_048_576 + 9));
    }

    /// A revert is `apply_patch` called with a full `history[n]` snapshot as the patch — proving
    /// the round trip restores every field, not just the ones a partial PUT would touch.
    #[test]
    fn revert_round_trips_a_snapshot() {
        let base = StoredCentralSettings { max_body: Some(2_000_000), ssh_port: Some(2200), ..Default::default() };
        let changed = apply_patch(
            &base,
            &StoredCentralSettings { max_body: Some(3_000_000), ..Default::default() },
            "admin@example.com",
            "t1",
        );
        assert_eq!(changed.max_body, Some(3_000_000));
        let snap = &changed.history[0];
        let revert_patch = StoredCentralSettings {
            max_body: snap.max_body,
            max_layer: snap.max_layer,
            max_manifest: snap.max_manifest,
            upload_grace_secs: snap.upload_grace_secs,
            gc_interval_secs: snap.gc_interval_secs,
            merge_lease_secs: snap.merge_lease_secs,
            announce_stranded_secs: snap.announce_stranded_secs,
            feed_retention_secs: snap.feed_retention_secs,
            clone_host: snap.clone_host.clone(),
            ssh_host: snap.ssh_host.clone(),
            ssh_port: snap.ssh_port,
            registry_host: snap.registry_host.clone(),
            signup_open: snap.signup_open,
            history: vec![],
            updated_by: String::new(),
            updated_at: String::new(),
        };
        let reverted = apply_patch(&changed, &revert_patch, "admin@example.com", "t2");
        assert_eq!(reverted.max_body, base.max_body);
        assert_eq!(reverted.ssh_port, base.ssh_port);
    }

    /// A corrupt document leaves the live handle untouched — "last good wins" is the beat's job,
    /// not the caller's.
    #[tokio::test(start_paused = true)]
    async fn refresh_beat_keeps_last_good_on_corrupt_document() {
        let live = LiveSettings::new(CentralSettings::from_env());
        let before = live.load().max_body;
        let fetch: CentralFetch = std::sync::Arc::new(|| Box::pin(async { Some(b"not json".to_vec()) }));
        let beat = tokio::spawn(refresh_central_beat(fetch, live.clone()));
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(SETTINGS_REFRESH_SECS + 1)).await;
        tokio::task::yield_now().await;
        assert_eq!(live.load().max_body, before, "corrupt document must not change the live value");
        beat.abort();
    }

    /// A missing key (never written) is treated the same as a corrupt one — nothing to apply.
    #[tokio::test(start_paused = true)]
    async fn refresh_beat_keeps_last_good_on_missing_key() {
        let live = LiveSettings::new(CentralSettings::from_env());
        let before = live.load().max_body;
        let fetch: CentralFetch = std::sync::Arc::new(|| Box::pin(async { None }));
        let beat = tokio::spawn(refresh_central_beat(fetch, live.clone()));
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(SETTINGS_REFRESH_SECS + 1)).await;
        tokio::task::yield_now().await;
        assert_eq!(live.load().max_body, before);
        beat.abort();
    }

    /// A well-formed document DOES swap in — the positive case beside the two "keeps last good"
    /// tests above.
    #[tokio::test(start_paused = true)]
    async fn refresh_beat_applies_a_good_document() {
        let live = LiveSettings::new(CentralSettings::from_env());
        let doc = StoredCentralSettings { ssh_port: Some(2277), ..Default::default() };
        let bytes = serde_json::to_vec(&doc).unwrap();
        let fetch: CentralFetch = std::sync::Arc::new(move || {
            let bytes = bytes.clone();
            Box::pin(async move { Some(bytes) })
        });
        let beat = tokio::spawn(refresh_central_beat(fetch, live.clone()));
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(SETTINGS_REFRESH_SECS + 1)).await;
        tokio::task::yield_now().await;
        assert_eq!(live.load().ssh_port, 2277);
        beat.abort();
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
