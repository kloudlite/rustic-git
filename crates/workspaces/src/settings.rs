//! The agent's live-settings mirror of `crd::ClusterSettingsSpec`, as a plain struct rather than
//! the CRD wrapper — a beat's unit test constructs one with `LiveSettings::new(AgentSettings {
//! .. })` and never needs a `kube::Client` to do it. Defaults are read from `crd::defaults` (not
//! re-typed here) so the CRD's `serde(default = ..)` and this struct's env fallback cannot drift
//! apart — Task 1's `defaults` module is the one place a built-in number lives.

use crate::crd::{self, ClusterSettingsSpec};

/// Mirrors `ClusterSettingsSpec` field-for-field. See that struct's doc comments for what each
/// field controls and its range; ranges are enforced by the admin write path (Task 4/5), not here.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentSettings {
    pub sync_secs: u64,
    pub replica_secs: u64,
    pub decommission_secs: u64,
    pub node_dead_secs: u64,
    pub peer_send_timeout_secs: u64,
    pub peer_serve_timeout_secs: u64,
    pub peer_receive_slack: u64,
    pub stop_flush_timeout_secs: u64,
    pub nix_timeout_secs: u64,
    pub nixpkgs: String,
    pub base_packages: String,
    pub default_replicas: u32,
    pub max_per_owner: u32,
    pub home_cache_gb: u32,
    pub quota_gb_ceiling: u32,
    /// Boot-marked (`CLUSTER_SETTING_META`) — read once at `Ctx` construction, not per reconcile.
    pub default_image: String,
    pub git_init_image: String,
    pub runtime_class: String,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self::from_env()
    }
}

impl AgentSettings {
    /// Env ?? built-in default (`crd::defaults`), one field per `WS_*` var the agent's beats
    /// read today. This is the FLOOR `merged_with` overrides from the stored `ClusterSettings`
    /// spec, never the other way around.
    pub fn from_env() -> Self {
        fn env_u64(key: &str, default: u64) -> u64 {
            std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
        }
        fn env_u32(key: &str, default: u32) -> u32 {
            std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
        }

        Self {
            sync_secs: env_u64("WS_SYNC_SECS", crd::defaults::sync_secs()),
            replica_secs: env_u64("WS_REPLICA_SECS", crd::defaults::replica_secs()),
            decommission_secs: env_u64("WS_DECOMMISSION_SECS", crd::defaults::decommission_secs()),
            node_dead_secs: env_u64("WS_NODE_DEAD_SECS", crd::defaults::node_dead_secs()),
            peer_send_timeout_secs: env_u64("WS_PEER_SEND_TIMEOUT_SECS", crd::defaults::peer_send_timeout_secs()),
            peer_serve_timeout_secs: env_u64("WS_PEER_SERVE_TIMEOUT_SECS", crd::defaults::peer_serve_timeout_secs()),
            peer_receive_slack: env_u64("WS_PEER_RECEIVE_SLACK", crd::defaults::peer_receive_slack()),
            // No env var reads a stop-flush deadline today — nothing enforces one yet.
            stop_flush_timeout_secs: crd::defaults::stop_flush_timeout_secs(),
            nix_timeout_secs: env_u64("WS_NIX_TIMEOUT", crd::defaults::nix_timeout_secs()),
            nixpkgs: std::env::var("WS_NIXPKGS").unwrap_or_default(),
            base_packages: std::env::var("WS_BASE_PACKAGES").unwrap_or_else(|_| crd::defaults::base_packages()),
            default_replicas: crd::DEFAULT_REPLICAS,
            max_per_owner: env_u32("WS_MAX_PER_OWNER", crd::defaults::max_per_owner()),
            home_cache_gb: crd::defaults::home_cache_gb(),
            quota_gb_ceiling: crd::defaults::quota_gb_ceiling(),
            default_image: std::env::var("WS_DEFAULT_IMAGE").unwrap_or_default(),
            git_init_image: std::env::var("WS_GIT_INIT_IMAGE").unwrap_or_else(|_| crd::defaults::git_init_image()),
            runtime_class: std::env::var("WS_RUNTIME_CLASS").unwrap_or_default(),
        }
    }

    /// `stored ?? env ?? default`: `self` (already env ?? default from `from_env`) is the floor;
    /// `spec`'s fields are `Option<_>`, so only what an admin actually set overrides it — a
    /// field the admin never touched stays at its env/default value instead of being silently
    /// overwritten by the CRD's own `serde` default.
    pub fn merged_with(mut self, spec: &ClusterSettingsSpec) -> Self {
        macro_rules! over {
            ($f:ident) => {
                if let Some(v) = spec.$f.clone() {
                    self.$f = v;
                }
            };
        }
        over!(sync_secs);
        over!(replica_secs);
        over!(decommission_secs);
        over!(node_dead_secs);
        over!(peer_send_timeout_secs);
        over!(peer_serve_timeout_secs);
        over!(peer_receive_slack);
        over!(stop_flush_timeout_secs);
        over!(nix_timeout_secs);
        over!(nixpkgs);
        over!(base_packages);
        over!(default_replicas);
        over!(max_per_owner);
        over!(home_cache_gb);
        over!(quota_gb_ceiling);
        over!(default_image);
        over!(git_init_image);
        over!(runtime_class);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `stored ?? env ?? default` precedence: an env var sets one field, the stored spec
    /// overrides a different field, a third field neither touches keeps the built-in default.
    #[test]
    fn merge_precedence() {
        // SAFETY: test-only, single-threaded within this test's scope.
        unsafe {
            std::env::set_var("WS_SYNC_SECS", "45");
        }
        let base = AgentSettings::from_env();
        assert_eq!(base.sync_secs, 45, "env must override the built-in default");

        let mut spec: ClusterSettingsSpec =
            serde_json::from_value(serde_json::json!({})).expect("every field is Option, so an empty object parses");
        spec.replica_secs = Some(900);
        let merged = base.clone().merged_with(&spec);
        assert_eq!(merged.replica_secs, 900, "an admin-set field in the stored spec must override env/default");
        assert_eq!(merged.sync_secs, 45, "a field the stored spec never touched (None) keeps env's value, not the CRD's own default");
        assert_eq!(
            merged.decommission_secs,
            crd::defaults::decommission_secs(),
            "a field neither env nor the stored spec touched keeps the built-in default"
        );

        unsafe {
            std::env::remove_var("WS_SYNC_SECS");
        }
    }
}
