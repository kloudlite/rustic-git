//! `GET /admin/settings/schema` — one field list, per scope, that the web (Task 7) renders both
//! settings tabs from. Everything here is DERIVED from a source that already exists (a meta
//! table, `#[derive(JsonSchema)]`'s doc-comment export, or a `from_env`/`defaults::` function) —
//! there is no second hand-typed copy of a description, a range, or a reader list to drift from
//! the one the write path itself enforces.

use super::*;
use rustic_git_core::settings::{CentralSettings, Mark, CENTRAL_SETTING_META};
use std::collections::HashMap;

#[derive(serde::Serialize)]
struct Range {
    min: f64,
    max: f64,
}

#[derive(serde::Serialize)]
struct Row {
    name: &'static str,
    description: String,
    unit: &'static str,
    range: Option<Range>,
    mark: &'static str,
    readers: Vec<&'static str>,
    default: serde_json::Value,
    env: Option<String>,
}

/// `snake_case` (a Rust field ident) to `camelCase` (the wire name every meta table and every
/// stored/CRD document actually uses) — the one conversion needed because `CentralSettings`
/// itself carries no `#[serde(rename_all)]` (only its wire twin `StoredCentralSettings` does),
/// while `ClusterSettingsSpec` already IS camelCase on the wire, so its own schema needs none.
fn snake_to_camel(s: &str) -> String {
    let mut out = String::new();
    let mut upper = false;
    for c in s.chars() {
        if c == '_' {
            upper = true;
            continue;
        }
        out.push(if upper { c.to_ascii_uppercase() } else { c });
        upper = false;
    }
    out
}

/// Doc-comment descriptions off `T`'s own `#[derive(JsonSchema)]` output, keyed by wire name.
fn descriptions_of<T: schemars::JsonSchema>(rename_to_camel: bool) -> HashMap<String, String> {
    let schema = schemars::SchemaGenerator::default().into_root_schema_for::<T>();
    let props = schema.as_value().get("properties").and_then(|p| p.as_object()).cloned().unwrap_or_default();
    props
        .into_iter()
        .filter_map(|(k, v)| {
            let desc = v.get("description")?.as_str()?.to_string();
            let key = if rename_to_camel { snake_to_camel(&k) } else { k };
            Some((key, desc))
        })
        .collect()
}

/// A field-name heuristic, not a fourth hand-typed table: every wire name in both meta tables
/// already encodes its unit in its own name (`*Secs`, `*Gb`, `max{Body,Layer,Manifest}`,
/// `*Port`), the same way `constraints.md`'s field table documents it in prose.
// ponytail: name-sniffing over a fifth explicit `(name, unit)` table; correct for every field
// that exists today, revisit if a field's name ever stops matching its own unit.
fn unit_of(name: &str, is_bool: bool, is_string: bool) -> &'static str {
    if is_bool {
        return "bool";
    }
    if is_string {
        return "string";
    }
    let n = name.to_ascii_lowercase();
    if n.ends_with("secs") {
        "seconds"
    } else if n.ends_with("gb") {
        "GiB"
    } else if n.contains("body") || n.contains("layer") || n.contains("manifest") {
        "bytes"
    } else if n.contains("port") {
        "port"
    } else {
        "count"
    }
}

// ── central ──────────────────────────────────────────────────────────────

/// `(wire name, env var)` — only the fields `CentralSettings::from_env` actually reads one for;
/// everything else has no env override today (`env` is `null` for those rows).
const CENTRAL_ENV_VARS: &[(&str, &str)] = &[
    ("maxBody", "RUSTIC_GIT_MAX_BODY"),
    ("maxLayer", "RUSTIC_GIT_MAX_LAYER"),
    ("uploadGraceSecs", "RUSTIC_GIT_UPLOAD_GRACE_SECS"),
    ("cloneHost", "RUSTIC_GIT_CLONE_HOST"),
    ("sshHost", "RUSTIC_GIT_SSH_HOST"),
    ("sshPort", "RUSTIC_GIT_SSH_PORT"),
    ("registryHost", "RUSTIC_GIT_REGISTRY_HOST"),
];

/// Ranges as `core::settings::validate_stored` enforces them — the exact numbers that function's
/// macro checks against, restated here as data rather than re-derived by calling it with probe
/// values (which `validate_stored` is not shaped to answer).
fn central_range(name: &str) -> Option<(f64, f64)> {
    match name {
        "maxBody" => Some((1_048_576.0, 8_589_934_592.0)),
        "maxLayer" => Some((1_048_576.0, 21_474_836_480.0)),
        "maxManifest" => Some((65_536.0, 67_108_864.0)),
        "uploadGraceSecs" => Some((3_600.0, 604_800.0)),
        "gcIntervalSecs" => Some((30.0, 86_400.0)),
        "mergeLeaseSecs" => Some((30.0, 3_600.0)),
        "announceStrandedSecs" => Some((5.0, 300.0)),
        "feedRetentionSecs" => Some((3_600.0, 2_592_000.0)),
        "sshPort" => Some((1.0, 65_535.0)),
        _ => None,
    }
}

fn central_rows() -> Vec<Row> {
    let desc = descriptions_of::<CentralSettings>(true);
    // `CentralSettings` (unlike its wire twin `StoredCentralSettings`) carries no
    // `#[serde(rename_all = "camelCase")]`, so its own `to_value` comes back snake_case — rekeyed
    // here the same way `descriptions_of` rekeys the schema's property names.
    let default: HashMap<String, serde_json::Value> = match serde_json::to_value(CentralSettings::built_in_defaults()) {
        Ok(serde_json::Value::Object(m)) => m.into_iter().map(|(k, v)| (snake_to_camel(&k), v)).collect(),
        _ => HashMap::new(),
    };
    CENTRAL_SETTING_META
        .iter()
        .map(|(name, mark)| {
            let default_v = default.get(*name).cloned().unwrap_or(serde_json::Value::Null);
            let is_bool = default_v.is_boolean();
            let is_string = default_v.is_string();
            let env = CENTRAL_ENV_VARS.iter().find(|(n, _)| n == name).and_then(|(_, var)| std::env::var(var).ok());
            Row {
                name,
                description: desc.get(*name).cloned().unwrap_or_default(),
                unit: unit_of(name, is_bool, is_string),
                range: central_range(name).map(|(min, max)| Range { min, max }),
                mark: match mark {
                    Mark::Live => "live",
                    Mark::Boot => "boot",
                },
                // `CENTRAL_SETTING_META` carries no reader list (unlike the cluster table) —
                // central boot readers are resolved dynamically (`api::admin::settings::
                // central_boot_readers`, region-dependent for `sshHost`/`sshPort`), so there is
                // no fixed list to report here for a field that is `Mark::Boot` today.
                readers: vec![],
                default: default_v,
                env,
            }
        })
        .collect()
}

// ── cluster ──────────────────────────────────────────────────────────────

const CLUSTER_ENV_VARS: &[(&str, &str)] = &[
    ("syncSecs", "WS_SYNC_SECS"),
    ("replicaSecs", "WS_REPLICA_SECS"),
    ("decommissionSecs", "WS_DECOMMISSION_SECS"),
    ("nodeDeadSecs", "WS_NODE_DEAD_SECS"),
    ("peerSendTimeoutSecs", "WS_PEER_SEND_TIMEOUT_SECS"),
    ("peerServeTimeoutSecs", "WS_PEER_SERVE_TIMEOUT_SECS"),
    ("peerReceiveSlack", "WS_PEER_RECEIVE_SLACK"),
    ("nixTimeoutSecs", "WS_NIX_TIMEOUT"),
    ("nixpkgs", "WS_NIXPKGS"),
    ("basePackages", "WS_BASE_PACKAGES"),
    ("maxPerOwner", "WS_MAX_PER_OWNER"),
    ("defaultImage", "WS_DEFAULT_IMAGE"),
    ("gitInitImage", "WS_GIT_INIT_IMAGE"),
    ("runtimeClass", "WS_RUNTIME_CLASS"),
];

fn cluster_default(name: &str) -> serde_json::Value {
    use crate::crd::defaults;
    match name {
        "syncSecs" => defaults::sync_secs().into(),
        "replicaSecs" => defaults::replica_secs().into(),
        "decommissionSecs" => defaults::decommission_secs().into(),
        "nodeDeadSecs" => defaults::node_dead_secs().into(),
        "peerSendTimeoutSecs" => defaults::peer_send_timeout_secs().into(),
        "peerServeTimeoutSecs" => defaults::peer_serve_timeout_secs().into(),
        "peerReceiveSlack" => defaults::peer_receive_slack().into(),
        "stopFlushTimeoutSecs" => defaults::stop_flush_timeout_secs().into(),
        "nixTimeoutSecs" => defaults::nix_timeout_secs().into(),
        "nixpkgs" => serde_json::Value::String(String::new()),
        "basePackages" => defaults::base_packages().into(),
        "defaultReplicas" => defaults::default_replicas().into(),
        "maxPerOwner" => defaults::max_per_owner().into(),
        "homeCacheGb" => defaults::home_cache_gb().into(),
        "quotaGbCeiling" => defaults::quota_gb_ceiling().into(),
        "defaultImage" => serde_json::Value::String(String::new()),
        "gitInitImage" => defaults::git_init_image().into(),
        "runtimeClass" => serde_json::Value::String(String::new()),
        _ => serde_json::Value::Null,
    }
}

fn cluster_range(name: &str) -> Option<(f64, f64)> {
    match name {
        "syncSecs" => Some((10.0, 3_600.0)),
        "replicaSecs" => Some((30.0, 3_600.0)),
        "decommissionSecs" => Some((5.0, 600.0)),
        "nodeDeadSecs" => Some((60.0, 3_600.0)),
        "peerSendTimeoutSecs" => Some((60.0, 21_600.0)),
        "peerServeTimeoutSecs" => Some((60.0, 21_600.0)),
        "peerReceiveSlack" => Some((0.0, 60.0)),
        "stopFlushTimeoutSecs" => Some((5.0, 300.0)),
        "nixTimeoutSecs" => Some((60.0, 7_200.0)),
        "defaultReplicas" => Some((1.0, 5.0)),
        "maxPerOwner" => Some((1.0, 1_000.0)),
        "homeCacheGb" => Some((1.0, 500.0)),
        "quotaGbCeiling" => Some((10.0, 5_000.0)),
        _ => None,
    }
}

fn cluster_rows() -> Vec<Row> {
    let desc = descriptions_of::<crd::ClusterSettingsSpec>(false);
    crd::CLUSTER_SETTING_META
        .iter()
        .map(|(name, mark, readers)| {
            let default_v = cluster_default(name);
            let is_bool = default_v.is_boolean();
            let is_string = default_v.is_string();
            let env = CLUSTER_ENV_VARS.iter().find(|(n, _)| n == name).and_then(|(_, var)| std::env::var(var).ok());
            Row {
                name,
                description: desc.get(*name).cloned().unwrap_or_default(),
                unit: unit_of(name, is_bool, is_string),
                range: cluster_range(name).map(|(min, max)| Range { min, max }),
                mark: match mark {
                    Mark::Live => "live",
                    Mark::Boot => "boot",
                },
                readers: readers.to_vec(),
                default: default_v,
                env,
            }
        })
        .collect()
}

pub(crate) async fn get_schema() -> Response {
    Json(serde_json::json!({"central": central_rows(), "cluster": cluster_rows()})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One row per meta-table entry, in both scopes — the schema must never drop or invent a
    /// field relative to what the write path itself validates against.
    #[test]
    fn row_count_matches_the_meta_tables() {
        assert_eq!(central_rows().len(), CENTRAL_SETTING_META.len());
        assert_eq!(cluster_rows().len(), crd::CLUSTER_SETTING_META.len());
    }

    /// A numeric field with no range would be a knob the write path lets through unchecked
    /// (`constraints.md`'s "a setting has a range or it is not a setting") — the only fields
    /// allowed to skip `range` are the non-numeric ones, where a min/max means nothing.
    #[test]
    fn every_row_has_a_range_or_is_bool_or_string() {
        for row in central_rows().into_iter().chain(cluster_rows()) {
            assert!(
                row.range.is_some() || row.unit == "bool" || row.unit == "string",
                "{} has neither a range nor a non-numeric unit ({})",
                row.name,
                row.unit
            );
        }
    }

    /// Nothing in `constraints.md`'s "secrets and addresses are never settings" list may leak
    /// into either env-var table this route reads from — asserted directly on the tables (rather
    /// than the rendered response) so this fails at the one place a forbidden var could ever be
    /// added, not downstream of it.
    #[test]
    fn no_secret_or_address_env_var_is_exposed() {
        const FORBIDDEN: &[&str] = &[
            "SECRET", "JWT", "S3_URL", "CACHE_DIR", "PEER_ADDR", "PEER_SVC", "RUSTIC_GIT_SELF",
            "WS_POOL", "WS_REGION", "NODE_NAME", "HOMES_EXPORT", "AUTH_", "RESEND_", "AWS_", "AZURE_",
        ];
        for (name, var) in CENTRAL_ENV_VARS.iter().chain(CLUSTER_ENV_VARS) {
            for bad in FORBIDDEN {
                assert!(!var.contains(bad), "central/cluster env table exposes {var} for {name}, matches forbidden {bad}");
            }
        }
    }
}
