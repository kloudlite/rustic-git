//! Per-owner projections: `OwnerBinding` (the namespace and its replica count) and `OwnerKeys`
//! (the authorized_keys file the api projects and the agent writes), plus the decommission labels
//! a node carries while it drains.

use super::*;


/// Which owner has namespaces reconciled, per region. One object per `{region, owner}`, and every
/// node reconciles every binding — the home is a region-shared NFS directory, so a binding is not
/// node-scoped (it once pinned an owner to a node; that pin is gone).
#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "kloudlite.io",
    version = "v1alpha1",
    kind = "OwnerBinding",
    plural = "ownerbindings",
    shortname = "ob",
    status = "OwnerBindingStatus",
    printcolumn = r#"{"name":"Owner","type":"string","jsonPath":".spec.owner"}"#,
    printcolumn = r#"{"name":"Region","type":"string","jsonPath":".spec.region"}"#,
    derive = "PartialEq"
)]
#[serde(rename_all = "camelCase")]
pub struct OwnerBindingSpec {
    pub owner: String,
    pub region: String,
}


/// Two: one active copy plus one standby, the smallest number that survives a single node loss.
pub const DEFAULT_REPLICAS: u32 = 2;
pub(super) fn default_replicas() -> u32 {
    DEFAULT_REPLICAS
}


#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OwnerBindingStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    /// Whether `spec.owner` names a team rather than a person — a FACT the binding reconciler
    /// derives (no personal Workspace anywhere carries this owner's handle, see
    /// `binding::is_team_owner`), not a directory lookup the agent cannot make. Readers with no
    /// directory of their own (`controller/environment.rs`'s quota sizing) read this instead of
    /// guessing; defaults false because a binding not yet reconciled is more often a person's
    /// first workspace than a team's first environment.
    #[serde(default)]
    pub team: bool,
}


/// One owner namespace's `authorized_keys`, PROJECTED from the directory: keys belong to people,
/// a namespace sees the union of its members' keys. Cluster-scoped, named by the owner handle,
/// written only by `bins/api` (server-side apply, on every key or membership change and on a
/// resync beat), read by every node's agent, which converges it into the file its pods mount
/// and reports `Synced`. Never the record: deleting it locks the owner out until the next beat
/// rewrites it, and nothing in a cluster can add a key.
#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "kloudlite.io",
    version = "v1alpha1",
    kind = "OwnerKeys",
    plural = "ownerkeys",
    shortname = "ok",
    status = "OwnerKeysStatus",
    printcolumn = r#"{"name":"Generation","type":"integer","jsonPath":".spec.generation"}"#,
    printcolumn = r#"{"name":"Synced","type":"string","jsonPath":".status.conditions[?(@.type==\"Synced\")].status"}"#,
    derive = "PartialEq"
)]
#[serde(rename_all = "camelCase")]
pub struct OwnerKeysSpec {
    /// The directory's version of this set: monotonic, so a reader can tell "same bytes,
    /// rewritten" from "changed". Unix millis at the api's write.
    pub generation: i64,
    /// The file, verbatim: one OpenSSH public line per key, sorted, `\n`-terminated. Empty is a
    /// namespace whose members have no keys — written as an empty file, never skipped.
    #[serde(default)]
    pub authorized_keys: String,
}


#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OwnerKeysStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}


/// `OwnerKeys` condition: this node has written `spec.generation` to the file its pods mount.
pub const KEYS_SYNCED: &str = "Synced";


/// Set on a `Node` by an operator (`kubectl label node <n> kloudlite.io/decommission=true`) to
/// retire it. A LABEL and not an annotation because it is a selector-worthy fact about the node,
/// and because removing it is the documented abort. Only the exact value `"true"` counts: a
/// half-typed label must never drain a node.
pub const DECOMMISSION_LABEL: &str = "kloudlite.io/decommission";


/// The drain's one progress window, written by the draining node's own agent and read by the
/// admin console's decommission gate. Lives here, next to the label, so the tier that WRITES it
/// and the tier that READS it can never spell it differently.
pub const DECOMMISSION_STATUS: &str = "kloudlite.io/decommission-status";


/// The sticky stamp `DECOMMISSION_STATUS` carries once a node holds nothing — the prefix the
/// console gates decommission on.
pub const DRAINED_PREFIX: &str = "drained ";
