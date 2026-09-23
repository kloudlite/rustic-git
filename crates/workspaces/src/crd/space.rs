//! `SpaceEnvironment`: the one environment a person's SPACE follows — every pod the platform runs
//! for that person in that team (their workspaces, their bench, any kind added later).
//!
//! A space already has a name, `ws_namespace(owner, team)`, and that is this object's name too, so
//! both tiers find it without a lookup. Written only by `/v1` (`PUT /v1/me/environments/{team}`);
//! the agent reads it from a cluster-wide cache and never writes it. Absent means "no environment".
//! It replaces per-workspace attach (`Workspace.spec.attachedEnvironment`, retired) — see
//! `docs/superpowers/specs/2026-09-14-person-environment-design.md`.

use super::*;


#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "kloudlite.io",
    version = "v1alpha1",
    kind = "SpaceEnvironment",
    plural = "spaceenvironments",
    status = "SpaceEnvironmentStatus",
    printcolumn = r#"{"name":"Owner","type":"string","jsonPath":".spec.owner"}"#,
    printcolumn = r#"{"name":"Team","type":"string","jsonPath":".spec.team"}"#,
    printcolumn = r#"{"name":"Environment","type":"string","jsonPath":".spec.environment"}"#,
    derive = "PartialEq"
)]
#[serde(rename_all = "camelCase")]
pub struct SpaceEnvironmentSpec {
    /// The PERSON's handle. Never a team: a choice is always the caller's own.
    pub owner: String,
    /// The team, or the owner's own handle for their personal space.
    pub team: String,
    /// The environment id every pod of the space resolves by bare name.
    pub environment: String,
}


/// Empty on purpose: the observed fact stays on each pod's parent as its `Attached` condition,
/// where every reader already looks. It exists only so the kind has the `/status` split every
/// sibling has.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SpaceEnvironmentStatus {}


/// Stamped by the api's migration on a Workspace whose retired `attachedEnvironment` it has
/// settled (a choice exists, or the attach can never become one). From then on the field is ignored
/// even while it is still set — so a clear that failed, or one deferred until every agent reads
/// choices, can never resurrect an attach the person has since cleared.
pub const SPACE_MIGRATED_ANNOTATION: &str = "kloudlite.io/space-migrated";


/// The retired field, unless the migration has settled it.
pub fn retired_attach<'a>(meta: &k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta, field: Option<&'a str>) -> Option<&'a str> {
    if meta.annotations.as_ref().is_some_and(|a| a.contains_key(SPACE_MIGRATED_ANNOTATION)) {
        return None;
    }
    field.filter(|f| !f.is_empty())
}


/// A listing view of `spec.environment`, what `delete_env` selects on. Never authorization.
pub const ENVIRONMENT_LABEL: &str = "kloudlite.io/environment";


/// The object name for a space: its namespace's name. `team` empty or equal to `owner` is the
/// personal space, exactly as `ws_namespace` folds it.
pub fn space_name(owner: &str, team: &str) -> String {
    ws_namespace(owner, team)
}


/// The space's SLUG — what a caller names it by: the team, or the owner's own handle for their
/// personal space, folded exactly as `ws_namespace` folds the pair. This is the `{team}` segment
/// of `/v1/me/environments/{team}`, the workspace-token's `space` claim and a pod's `KL_TEAM`.
pub fn space_slug(owner: &str, team: &str) -> String {
    let owner = owner.to_lowercase();
    if team.is_empty() || team.eq_ignore_ascii_case(&owner) { owner } else { team.to_lowercase() }
}


/// The `SpaceEnvironment` `/v1` writes for one choice, labels included.
pub fn space_environment(owner: &str, team: &str, environment: &str) -> SpaceEnvironment {
    let mut s = SpaceEnvironment::new(
        &space_name(owner, team),
        SpaceEnvironmentSpec { owner: owner.to_lowercase(), team: team.to_lowercase(), environment: environment.to_string() },
    );
    s.metadata.labels = Some(BTreeMap::from([
        ("kloudlite.io/owner".to_string(), owner.to_lowercase()),
        ("kloudlite.io/team".to_string(), team.to_lowercase()),
        (ENVIRONMENT_LABEL.to_string(), environment.to_string()),
    ]));
    s
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_space_is_named_by_its_namespace_and_personal_folds_like_ws_namespace() {
        assert_eq!(space_name("alice", "acme"), ws_namespace("alice", "acme"));
        assert_eq!(space_name("alice", ""), "ws-alice");
        assert_eq!(space_name("alice", "alice"), "ws-alice");
        let s = space_environment("Alice", "acme", "env-1");
        assert_eq!(s.metadata.name.as_deref(), Some(ws_namespace("alice", "acme").as_str()));
        assert_eq!(s.metadata.labels.unwrap()[ENVIRONMENT_LABEL], "env-1");
        assert_eq!(s.spec.owner, "alice");
    }

    #[test]
    fn a_space_slug_folds_personal_to_the_handle() {
        assert_eq!(space_slug("Alice", ""), "alice");
        assert_eq!(space_slug("alice", "Alice"), "alice");
        assert_eq!(space_slug("alice", "ACME"), "acme");
    }

    #[test]
    fn objects_written_with_the_retired_attach_field_still_parse() {
        let w: WorkspaceSpec = serde_json::from_value(serde_json::json!({
            "owner": "a", "name": "n", "region": "r", "image": "i", "desiredState": "running",
            "attachedEnvironment": "env-1"
        }))
        .unwrap();
        assert_eq!(w.attached_environment.as_deref(), Some("env-1"));
    }
}
