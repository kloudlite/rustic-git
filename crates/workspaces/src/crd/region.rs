//! `Region`: a cluster the api can place into, written only through `/v1/regions`.

use super::*;


/// A region a workspace may run in — cluster-scoped, like every other kind here.
///
/// Cross-cluster metadata by nature, and it used to live in Cosmos for exactly that reason. It does
/// not need to: a region is registered by an admin, read on every create, and changed almost never,
/// so the cheapest correct home is the API server this tier already talks to. `spec.status` is
/// DESIRED state (`active`/`inactive`) — re-registering is the only way to retire one, and a
/// retired region stops being offered while its existing workspaces keep running.
#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "kloudlite.io",
    version = "v1alpha1",
    kind = "Region",
    plural = "regions",
    status = "RegionStatus",
    printcolumn = r#"{"name":"Status","type":"string","jsonPath":".spec.status"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct RegionSpec {
    /// What a person sees in the region picker. The object's NAME is the id.
    pub name: String,
    /// `active` or `inactive`.
    pub status: String,
}


/// Empty on purpose: no controller observes a region. It exists so the kind has the same
/// `/status` subresource split every sibling has, rather than being the one kind where a status
/// write would fold into spec.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RegionStatus {}
