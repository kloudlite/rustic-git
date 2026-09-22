//! `Bench`: a person's bench in one team; no sshd, no region of its own (a team is bound to one);
//! its own Volume holds `/home/kl` (2026-09-22, same shape as a workspace's); it sleeps when idle;
//! see the spec.

use super::*;


#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "kloudlite.io",
    version = "v1alpha1",
    kind = "Bench",
    plural = "benches",
    status = "BenchStatus",
    selectable = ".status.nodeName",
    printcolumn = r#"{"name":"Owner","type":"string","jsonPath":".spec.owner"}"#,
    printcolumn = r#"{"name":"Team","type":"string","jsonPath":".spec.team"}"#,
    printcolumn = r#"{"name":"Node","type":"string","jsonPath":".status.nodeName"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    derive = "PartialEq"
)]
#[serde(rename_all = "camelCase")]
pub struct BenchSpec {
    pub owner: String,
    pub team: String,
    // No region: a team is bound to one (the directory's record), and /v1 reads it there.
    pub image: String,
    #[serde(default)]
    pub model: String,
    pub desired_state: DesiredState,
    /// `Full` for a member; `ReadOnly` once the owner has left `team`. Written only by /v1.
    #[serde(default)]
    pub access: BenchAccess,
    /// RFC 3339, written by /v1 when a client asks for a tunnel to an idle bench. A pod is wanted
    /// again only while this is later than `status.idleSince`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_at: Option<String>,
    #[serde(default)]
    pub resources: PodResources,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached_environment: Option<String>,
}


#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum BenchAccess {
    #[default]
    Full,
    ReadOnly,
}


#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BenchStatus {
    pub phase: Phase,
    #[serde(default)]
    pub node_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_ref: Option<String>,
    /// The `finishedAt` of the pod that exited idle; cleared when a pod is created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_since: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}


pub const FOLDER_LOCKED: &str = "FolderLocked";
pub const BENCH_IDLE: &str = "Idle";


/// Whether a pod should exist now: Running, and not asleep unless a wake came after it slept.
pub fn bench_wants_pod(b: &Bench) -> bool {
    if b.spec.desired_state != DesiredState::Running {
        return false;
    }
    let Some(idle_since) = b.status.as_ref().and_then(|s| s.idle_since.as_deref()) else {
        return true;
    };
    let Some(idle_since) = chrono::DateTime::parse_from_rfc3339(idle_since).ok() else {
        return true;
    };
    match b.spec.wake_at.as_deref().and_then(|w| chrono::DateTime::parse_from_rfc3339(w).ok()) {
        Some(wake_at) => wake_at > idle_since,
        None => false,
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bench_is_one_name_per_owner_and_team_and_parses_without_optional_fields() {
        assert_eq!(bench_id("Alice", "acme"), bench_id("alice", "acme"));
        assert_ne!(bench_id("alice", "acme"), bench_id("alice", "alice"));
        assert_ne!(bench_id("ab", "c"), bench_id("a", "bc"), "the separator keeps pairs apart");
        assert!(bench_id("alice", "acme").len() <= 63);
        let v = serde_json::json!({"owner":"alice","team":"acme","image":"i","desiredState":"running"});
        let s: BenchSpec = serde_json::from_value(v).unwrap();
        assert_eq!(s.resources, PodResources::default());
        assert_eq!(s.access, BenchAccess::Full, "an absent access is a member's bench");
        assert!(s.model.is_empty() && s.attached_environment.is_none() && s.wake_at.is_none());
    }

    #[test]
    fn a_pod_is_wanted_while_running_and_awake_or_woken_after_it_slept() {
        let mut b = Bench::new(
            "bench-1",
            serde_json::from_value(
                serde_json::json!({"owner":"alice","team":"acme","image":"i","desiredState":"running"}),
            )
            .unwrap(),
        );
        assert!(bench_wants_pod(&b), "never slept");
        b.status = Some(BenchStatus { idle_since: Some("2026-09-13T10:00:00Z".into()), ..Default::default() });
        assert!(!bench_wants_pod(&b), "asleep and nobody asked");
        b.spec.wake_at = Some("2026-09-13T09:59:59Z".into());
        assert!(!bench_wants_pod(&b), "a wake from before it slept is spent");
        b.spec.wake_at = Some("2026-09-13T10:00:01Z".into());
        assert!(bench_wants_pod(&b));
        b.spec.desired_state = DesiredState::Stopped;
        assert!(!bench_wants_pod(&b), "stopped refuses a wake");
    }
}
