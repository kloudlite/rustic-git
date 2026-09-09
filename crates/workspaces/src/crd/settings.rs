//! `ClusterSettings`: the per-region agent tunables, their bootstrap defaults (`defaults`), and the
//! `CLUSTER_SETTING_META` table that says which reader each knob rolls.

use super::*;


/// Built-in defaults for `ClusterSettingsSpec` fields, one `fn` per field so `#[serde(default =
/// "...")]` has a path to name — kept separate from the values themselves so
/// `Settings::from_env` (Task 2) can call the same functions as the env-var fallback, and the CRD
/// default and the env default can never drift apart.
pub mod defaults {
    pub fn sync_secs() -> u64 {
        60
    }
    pub fn replica_secs() -> u64 {
        300
    }
    pub fn decommission_secs() -> u64 {
        30
    }
    pub fn node_dead_secs() -> u64 {
        180
    }
    pub fn peer_send_timeout_secs() -> u64 {
        3600
    }
    pub fn peer_serve_timeout_secs() -> u64 {
        900
    }
    pub fn peer_receive_slack() -> u64 {
        3
    }
    pub fn stop_flush_timeout_secs() -> u64 {
        30
    }
    pub fn nix_timeout_secs() -> u64 {
        1200
    }
    /// Mirrors `bins/agent/src/nix.rs`'s `DEFAULT_BASE_PACKAGES` — duplicated, not imported,
    /// because `crates/workspaces` cannot depend on `bins/agent` (the dependency runs the other
    /// way). Keep the two strings in sync by hand; a mismatch is silent, not a compile error.
    pub fn base_packages() -> String {
        "bashInteractive zsh fish starship coreutils git openssh curl less which gnugrep gnused findutils".to_string()
    }
    pub fn default_replicas() -> u32 {
        crate::crd::DEFAULT_REPLICAS
    }
    pub fn max_per_owner() -> u32 {
        50
    }
    pub fn home_cache_gb() -> u32 {
        20
    }
    pub fn quota_gb_ceiling() -> u32 {
        500
    }
    pub fn git_init_image() -> String {
        // Matches the agent's own pre-settings fallback (`bins/agent/src/controller/mod.rs`) —
        // this is a required init container image, not an optional one, so the built-in default
        // cannot be empty the way an unset `runtime_class` legitimately is.
        "alpine/git:2.45.2".to_string()
    }
}


/// One per region, named `default` — the cluster-scoped tunables every agent in that cluster
/// reads on its refresh beat. `spec` is desired (admin-written); `status.observedGeneration`
/// is the last generation an agent actually applied, so the UI's "pending" marker has
/// something to compare against. Cluster-scoped like every other kind here: there is one
/// object per region's k3s, not per namespace.
#[derive(CustomResource, Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "kloudlite.io",
    version = "v1alpha1",
    kind = "ClusterSettings",
    plural = "clustersettings",
    status = "ClusterSettingsStatus"
    // selectable = "": deliberately no selectableFields — agents watch the single `default`
    // object by name, not by node, so there is no per-node axis to select on.
)]
#[serde(rename_all = "camelCase")]
pub struct ClusterSettingsSpec {
    /// Sync-point cut beat interval. 10..=3600 seconds. `None` = admin never set it — the reader
    /// falls back to env, then the built-in default (`AgentSettings::merged_with`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_secs: Option<u64>,
    /// Replication pull beat interval. 30..=3600 seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replica_secs: Option<u64>,
    /// Decommission-beat interval. 5..=600 seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decommission_secs: Option<u64>,
    /// How long a node must be observed NotReady before it is declared dead for placement.
    /// 60..=3600 seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_dead_secs: Option<u64>,
    /// `btrfs send`-over-HTTP client timeout. 60..=21600 seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_send_timeout_secs: Option<u64>,
    /// The send side's own deadline, deliberately shorter than the client's. 60..=21600 seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_serve_timeout_secs: Option<u64>,
    /// Slack added to the receive-side timeout over the serve-side one. 0..=60 seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_receive_slack: Option<u64>,
    /// Deadline for a stop's flush before the pod is torn down anyway. 5..=300 seconds.
    // ponytail: no caller reads this yet; ships for the admin UI ahead of the enforcement it
    // is meant for. Add the read when a stop-flush deadline is actually implemented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_flush_timeout_secs: Option<u64>,
    /// Nix build timeout. 60..=7200 seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nix_timeout_secs: Option<u64>,
    /// Nixpkgs revision pin (`github:NixOS/nixpkgs/<rev>`). `None` = whatever the agent's own
    /// env default is — this field does not carry a built-in default of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nixpkgs: Option<String>,
    /// Packages prepended to every workspace's profile, space-separated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_packages: Option<String>,
    /// Default `Volume.spec.replicas` for a newly created volume. 1..=5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_replicas: Option<u32>,
    /// Max workspaces+environments per owner in this region, until `Quota` fully replaces it.
    /// 1..=1000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_per_owner: Option<u32>,
    /// Home-cache local subvolume quota per (owner, node). 1..=500 GiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_cache_gb: Option<u32>,
    /// Ceiling `clamp_quota` enforces on a requested quota. 10..=5000 GiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_gb_ceiling: Option<u32>,
    /// Tenant workspace pod image. **Boot** — the agent reads this at pod-template render
    /// time, not per reconcile; a change rolls `kloudlite-agent` (Task 5). `None` = keep
    /// today's env value, so an admin who never opens this row cannot blank a required image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_image: Option<String>,
    /// The init container that clones a workspace's seed repo over SSH. **Boot**, same reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_init_image: Option<String>,
    /// k8s `runtimeClassName` for tenant pods (e.g. `gvisor`); `None` = host kernel. **Boot**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_class: Option<String>,
}


#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClusterSettingsStatus {
    /// The generation an agent last successfully applied. Compared against
    /// `metadata.generation` by the admin UI's pending marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
}


/// Which mechanism carries each `ClusterSettingsSpec` field: `Live` (next refresh beat picks it
/// up) or `Boot` (only a pod-template rebuild reads it, so it needs a roll of the readers named
/// here). A test (`cluster_setting_meta_is_exhaustive`) asserts this table's field names equal
/// `ClusterSettingsSpec`'s schemars property names, so a field added to the struct without an
/// entry here fails loudly instead of shipping unreadable.
pub const CLUSTER_SETTING_META: &[(&str, kloudlite_core::settings::Mark, &[&str])] = &[
    ("syncSecs", kloudlite_core::settings::Mark::Live, &[]),
    ("replicaSecs", kloudlite_core::settings::Mark::Live, &[]),
    ("decommissionSecs", kloudlite_core::settings::Mark::Live, &[]),
    ("nodeDeadSecs", kloudlite_core::settings::Mark::Live, &[]),
    ("peerSendTimeoutSecs", kloudlite_core::settings::Mark::Live, &[]),
    ("peerServeTimeoutSecs", kloudlite_core::settings::Mark::Live, &[]),
    ("peerReceiveSlack", kloudlite_core::settings::Mark::Live, &[]),
    ("stopFlushTimeoutSecs", kloudlite_core::settings::Mark::Live, &[]),
    ("nixTimeoutSecs", kloudlite_core::settings::Mark::Live, &[]),
    ("nixpkgs", kloudlite_core::settings::Mark::Live, &[]),
    ("basePackages", kloudlite_core::settings::Mark::Live, &[]),
    ("defaultReplicas", kloudlite_core::settings::Mark::Live, &[]),
    ("maxPerOwner", kloudlite_core::settings::Mark::Live, &[]),
    ("homeCacheGb", kloudlite_core::settings::Mark::Live, &[]),
    ("quotaGbCeiling", kloudlite_core::settings::Mark::Live, &[]),
    ("defaultImage", kloudlite_core::settings::Mark::Boot, &["kloudlite-agent"]),
    ("gitInitImage", kloudlite_core::settings::Mark::Boot, &["kloudlite-agent"]),
    ("runtimeClass", kloudlite_core::settings::Mark::Boot, &["kloudlite-agent"]),
];
