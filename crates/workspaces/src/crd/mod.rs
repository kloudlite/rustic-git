//! The `kloudlite.io/v1alpha1` custom resources — the reconcile substrate for workspaces and
//! environments.
//!
//! These types ARE the source of truth. `/v1` writes spec, each node's controller reconciles the
//! objects bound to it and writes status back through the `/status` subresource. Cosmos keeps only
//! cross-cluster `Region` metadata; where the two could disagree, the CRD wins, always.
//!
//! Two attributes on every kind are load-bearing and both fail SILENTLY when dropped, which is why
//! `tests/crd_yaml.rs` asserts them rather than trusting review:
//!
//! * `status = "…"` emits the `/status` subresource. Without it a status write folds into spec, and
//!   the RBAC split that stops a controller editing its own desired state becomes decorative. The
//!   split is half of that guarantee: the agent still holds `patch` on the main resources (for
//!   labels, finalizers and `VolumeSpec::restore_to`), and it is the
//!   ValidatingAdmissionPolicy in
//!   `deploy/k3s/agent-admission.yaml` that refuses it any other spec change.
//! * `selectable = "…nodeName"` emits `selectableFields`, which is what lets a controller watch
//!   only its own node's objects. Without it every node sees every object and two agents race the
//!   same subvolume. WHICH path is selectable differs by kind: placement is a fact the controllers
//!   establish, so a parent (`Workspace`, `Environment`) selects on `.status.nodeName` while a
//!   controller-written child (`Volume`) selects on `.spec.nodeName`.
//!
//! All five kinds are CLUSTER-scoped (no `namespaced` attribute): they name node-local storage, and
//! a namespace would imply a tenancy boundary the btrfs pool does not have. The pods and services
//! they produce are namespaced; the objects describing them are not.
//!
//! One file per CR kind: `volume` (Volume, VolumeReplica), `snapshot`, `workspace`, `environment`,
//! `owner` (OwnerBinding, OwnerKeys), `quota` (Quota, QuotaRequest, Request), `region`,
//! `settings` (ClusterSettings and `defaults`); `names` holds the naming rules. What every kind
//! shares — the group and version, the field managers, the finalizers, `Phase`, `DesiredState`,
//! `PodResources`, the condition helpers and `all_crds` — stays here.

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::{CustomResource, CustomResourceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;

mod volume;
mod snapshot;
mod workspace;
mod environment;
mod owner;
mod quota;
mod region;
mod settings;
pub use volume::*;
pub use snapshot::*;
pub use workspace::*;
pub use environment::*;
pub use owner::*;
pub use quota::*;
pub use region::*;
pub use settings::*;


pub(super) mod names;
pub use names::*;


pub const GROUP: &str = "kloudlite.io";

pub const VERSION: &str = "v1alpha1";
/// The controller writes status under its own manager; a server-side-apply conflict against it
/// therefore means another controller, not `/v1`.
pub const AGENT_FIELD_MANAGER: &str = "kloudlite-agent";
/// The admin process's own field manager on the settings routes — distinct from
/// `AGENT_FIELD_MANAGER` so a settings write and the agent's own status writes are never
/// attributed to the same manager in a server-side-apply conflict.
pub const AGENT_FIELD_MANAGER_ADMIN: &str = "kloudlite-admin";
/// `bins/api`'s field manager: the api owns spec on the objects it applies, so a conflict against
/// it is another writer of DESIRED state, never a controller's status write.
pub const API_FIELD_MANAGER: &str = "kloudlite-api";
/// `ClusterSettings`' history annotation: the previous ten specs, newest first, JSON — parallel to
/// `StoredCentralSettings.history` but as an annotation rather than a struct field, since the CRD
/// spec is what server-side apply owns field-by-field and a growing history array there would be a
/// moving target for every other writer of the spec (there are none today, but the annotation
/// keeps the spec itself exactly the shape `ClusterSettingsSpec` declares).
pub const SETTINGS_HISTORY_ANNOTATION: &str = "kloudlite.io/settings-history";

pub const SETTINGS_UPDATED_BY_ANNOTATION: &str = "kloudlite.io/updated-by";

pub const SETTINGS_UPDATED_AT_ANNOTATION: &str = "kloudlite.io/updated-at";
/// Held while a subvolume exists on a node. The object must outlive the delete request until the
/// controller has actually reclaimed the bytes — otherwise the record of what to reclaim is gone
/// before the reclaim happens.
pub const SUBVOLUME_FINALIZER: &str = "kloudlite.io/subvolume";
/// Held on a shared-volume clone workspace. A workspace that is a
/// shared-volume clone (`spec.storage.source` is `CloneOf { commit: Some(_), .. }`) checks out a
/// worktree under the SOURCE volume's `live/`, not its own — it owns no `Volume` child, so
/// nothing's ownerReference GC ever reclaims that worktree. This finalizer is what makes the
/// delete drop it. An owned-volume workspace also carries this finalizer (added uniformly to
/// avoid distinguishing the two cases before the spec's `source` is known to be gone at delete
/// time), but its cleanup is a no-op: the owned `Volume`'s own `SUBVOLUME_FINALIZER` already
/// deletes the whole voldir, worktree included.
pub const WORKTREE_FINALIZER: &str = "kloudlite.io/worktree";


/// What the operator asked for, independent of what is currently true.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum DesiredState {
    Running,
    Stopped,
}


/// Requests and limits for a workspace pod, as plain strings in Kubernetes quantity form.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PodResources {
    pub cpu_request: String,
    pub cpu_limit: String,
    pub memory_request: String,
    pub memory_limit: String,
}


impl Default for PodResources {
    /// The workspace slot from the capacity model — the sheet's "M session" row, and `session` is
    /// its word for a workspace: guarantee 4 GB / 2 vCPU, limit 8 GB / 4 vCPU. The model and its
    /// provenance are tabulated in `docs/capacity-model.md`; change a number there and here
    /// together, never one alone.
    ///
    /// The REQUEST is the load-bearing half. It is what the scheduler packs against, so it — not
    /// the limit — decides how many sessions a node holds and therefore what a session costs. The
    /// previous 512Mi/250m request was a small floor "with room to burst", which let a 128 GB node
    /// accept roughly 235 sessions against the model's ~30, and made the model's "guaranteed CPU is
    /// NOT oversubscribed on session nodes" false: 235 × 2 vCPU of promised capacity on 64 vCPU.
    ///
    /// The arithmetic these numbers have to satisfy, on a 32-OCPU / 128 GB session node at the
    /// model's 94% usable-memory headroom: 120 GB ÷ 4 GB = 30 sessions, needing 30 × 2 = 60 vCPU of
    /// 64. Memory-bound, CPU fits, guarantee honoured.
    fn default() -> Self {
        Self {
            cpu_request: "2".into(),
            cpu_limit: "4".into(),
            memory_request: "4Gi".into(),
            memory_limit: "8Gi".into(),
        }
    }
}


/// Every lifecycle state any of the five kinds reports, as ONE enum.
///
/// An enum rather than a `String` so schemars emits `enum` and the API server rejects a typo with a
/// 422. A free-form string is how `running` reached a `WsState` that spells that state `Ready`: the
/// projection's `serde_json::from_value` fell back to its default, so a healthy workspace showed
/// "Creating" in the UI indefinitely, with nothing failing and nothing logged.
///
/// One enum for five kinds rather than five, because the alternative is five near-identical types
/// and a `phase` field whose type a reader has to look up per kind. Which variants are legal for
/// which kind is the reconciler's business; the schema's job is to refuse a word nobody defined.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    /// Created, not yet claimed by a node.
    #[default]
    Pending,
    Creating,
    /// A workspace whose pod is Ready, or a Volume whose subvolume is materialized.
    Ready,
    /// An environment whose services are up. (`WsState` has no `Running`; `EnvState` has no
    /// `Ready` — the two projections disagree, and this enum is the union.)
    Running,
    Stopped,
    /// A btrfs operation is in flight.
    Working,
    /// The owning node is dead and the pin has been cleared: no node may write this subvolume
    /// until one takes it (`resolve_volume`'s takeover arm). Distinct from `Error` so an
    /// operator can tell "waiting for a Synced survivor" from "something is broken".
    Unavailable,
    /// Historical: a pre-cutover `SnapshotRequest` whose record was in the registry, never
    /// re-run past this. The kind is gone; the variant stays for CRs written before the cutover.
    Done,
    Error,
}


impl Phase {
    /// The wire word, so a projection can go on matching on `&str` and the `/v1` docs' own enums
    /// (`model::WsState`, `model::EnvState`) stay the separate vocabulary they are.
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Pending => "pending",
            Phase::Creating => "creating",
            Phase::Ready => "ready",
            Phase::Running => "running",
            Phase::Stopped => "stopped",
            Phase::Working => "working",
            Phase::Unavailable => "unavailable",
            Phase::Done => "done",
            Phase::Error => "error",
        }
    }
}


/// Every CRD this repo owns, for YAML generation and for a startup precondition check.
pub fn all_crds() -> Vec<CustomResourceDefinition> {
    vec![
        Volume::crd(),
        Workspace::crd(),
        Environment::crd(),
        OwnerBinding::crd(),
        OwnerKeys::crd(),
        Snapshot::crd(),
        VolumeReplica::crd(),
        Region::crd(),
        Quota::crd(),
        QuotaRequest::crd(),
        Request::crd(),
        ClusterSettings::crd(),
    ]
}


/// A standard condition with `observedGeneration` stamped.
///
/// `meta/v1.Condition` rather than a bespoke struct, because it is the shape
/// `kubectl wait --for=condition=Ready` already reads.
pub fn condition(kind: &str, status: bool, reason: &str, message: &str, generation: i64) -> Condition {
    condition_since(None, kind, status, reason, message, generation)
}


/// The same, keeping `prev`'s `lastTransitionTime` when nothing actually transitioned. The field
/// means "since when has it been in THIS state" — restamping it on every identical write turns
/// "failing for an hour" into "failing since a moment ago", which is exactly the signal a backoff
/// reads.
pub fn condition_since(
    prev: Option<&Condition>,
    kind: &str,
    status: bool,
    reason: &str,
    message: &str,
    generation: i64,
) -> Condition {
    let mut c = condition_now(kind, status, reason, message, generation);
    if let Some(p) = prev {
        if p.status == c.status && p.reason == c.reason {
            c.last_transition_time = p.last_transition_time.clone();
        }
    }
    c
}


pub(super) fn condition_now(kind: &str, status: bool, reason: &str, message: &str, generation: i64) -> Condition {
    Condition {
        type_: kind.to_string(),
        status: if status { "True" } else { "False" }.to_string(),
        reason: reason.to_string(),
        message: message.to_string(),
        observed_generation: Some(generation),
        // The API server rejects a condition with no transition time, and a reconcile has no
        // better clock than now. `jiff`, not chrono: k8s-openapi 0.28 wraps `jiff::Timestamp`
        // here, so this is the one place in the crate that does not use the workspace's chrono.
        last_transition_time: k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
            k8s_openapi::jiff::Timestamp::now(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The backoff on a repeatedly failing build reads `lastTransitionTime` to know how long it
    /// has been failing — so an identical condition written again must keep the earlier stamp,
    /// and a changed reason must not.
    #[test]
    fn a_repeated_condition_keeps_the_time_it_first_transitioned() {
        let mut first = condition("PackagesReady", false, "BuildFailed", "boom", 1);
        first.last_transition_time = k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
            k8s_openapi::jiff::Timestamp::UNIX_EPOCH,
        );
        let again = condition_since(Some(&first), "PackagesReady", false, "BuildFailed", "boom again", 2);
        assert_eq!(again.last_transition_time, first.last_transition_time);
        assert_eq!(again.message, "boom again");
        let changed = condition_since(Some(&first), "PackagesReady", true, "Built", "ok", 2);
        assert_ne!(changed.last_transition_time, first.last_transition_time);
    }

    #[test]
    fn workspace_status_carries_packages_and_omits_it_when_unset() {
        let st = WorkspaceStatus::default();
        assert!(!serde_json::to_string(&st).unwrap().contains("packages"));
        let st = WorkspaceStatus {
            packages: Some(PackagesStatus {
                base: vec![],
                observed: vec!["go".into()],
                observed_hash: Some("sha256:x".into()),
                profile: None,
                nixpkgs: None,
                locked: vec![],
            }),
            ..Default::default()
        };
        let v = serde_json::to_value(&st).unwrap();
        assert_eq!(v["packages"]["observed"][0], "go");
        assert_eq!(v["packages"]["observedHash"], "sha256:x");
    }

    #[test]
    fn workspace_spec_carries_packages_and_omits_it_when_empty() {
        let mut spec = WorkspaceSpec {
            owner: "o".into(),
            team: String::new(),
            name: "n".into(),
            region: "r".into(),
            image: "i".into(),
            storage: None,
            desired_state: DesiredState::Running,
            resources: PodResources::default(),
            packages: vec![],
            locks: vec![],
            attached_environment: None,
        };
        assert!(!serde_json::to_string(&spec).unwrap().contains("packages"));
        spec.packages = vec!["go".into(), "jq".into()];
        let v = serde_json::to_value(&spec).unwrap();
        assert_eq!(v["packages"][0], "go");
        let back: WorkspaceSpec = serde_json::from_value(v).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn a_volume_without_replicas_reads_the_default_of_two() {
        let v: VolumeSpec = serde_json::from_value(serde_json::json!({
            "owner": "alice", "nodeName": "n", "region": "r1", "quotaGb": 2
        }))
        .unwrap();
        assert_eq!(v.replicas, 2);
    }

    #[test]
    fn snapshot_name_is_volume_dash_eight_hex_and_varies_per_call() {
        let a = snapshot_name("myvol");
        let b = snapshot_name("myvol");
        assert!(a.starts_with("myvol-"), "{a}");
        let hex = a.strip_prefix("myvol-").unwrap();
        assert_eq!(hex.len(), 8);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two calls must not collide");
    }

    /// The one distinction there is: a push is a snapshot, a sync point is not, and a MIGRATION
    /// BASELINE is not — including the ones an older build wrote as ordinary records, which are
    /// already on the cluster and would otherwise keep their Volume alive forever.
    #[test]
    fn a_push_is_a_snapshot_but_a_sync_point_or_a_baseline_is_not() {
        let snap = |spec: serde_json::Value| -> Snapshot {
            serde_json::from_value(serde_json::json!({
                "apiVersion": "kloudlite.io/v1alpha1", "kind": "Snapshot",
                "metadata": {"name": "v-aaaaaaaa"}, "spec": spec,
            }))
            .unwrap()
        };
        let base = serde_json::json!({"volume": "v", "owner": "o", "worktree": "v", "parent": ""});

        assert!(snap(base.clone()).is_snapshot(), "a push");
        let mut with_msg = base.clone();
        with_msg["message"] = serde_json::json!("wip");
        assert!(snap(with_msg).is_snapshot(), "a push with a message is still a push");

        let mut transient = base.clone();
        transient["transient"] = serde_json::json!(true);
        assert!(!snap(transient).is_snapshot(), "a sync point");

        let mut legacy = base.clone();
        legacy["message"] = serde_json::json!("migration baseline");
        assert!(!snap(legacy).is_snapshot(), "a baseline an older build wrote as an ordinary record");

        // Shape, not text alone: a push that happens to carry that message but sits on a parent is
        // somebody's snapshot, and a baseline is always a root.
        let mut lookalike = base;
        lookalike["message"] = serde_json::json!("migration baseline");
        lookalike["parent"] = serde_json::json!("v-bbbbbbbb");
        assert!(snap(lookalike).is_snapshot(), "a rooted record is never a baseline");
    }

    #[test]
    fn replica_name_is_deterministic_per_volume_and_node() {
        assert_eq!(replica_name("myvol", "node-a"), "myvol.node-a");
        assert_eq!(replica_name("myvol", "node-a"), replica_name("myvol", "node-a"));
    }

    #[test]
    fn snapshot_spec_round_trips_with_empty_parent_and_no_message() {
        let spec = SnapshotSpec {
            volume: "v".into(), owner: "alice".into(), worktree: "ws-1".into(), parent: String::new(), message: None, transient: false, state: None,
        };
        let v = serde_json::to_value(&spec).unwrap();
        assert!(!v.as_object().unwrap().contains_key("message"));
        let back: SnapshotSpec = serde_json::from_value(v).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn snapshot_state_serializes_with_the_kind_tag_and_camel_case() {
        let st = SnapshotState::Workspace {
            image: "alpine:3.20".into(),
            packages: vec!["ripgrep".into()],
            locks: vec![],
            resources: PodResources::default(),
            quota_gb: 5,
            attached_environment: Some("env-1".into()),
        };
        let v = serde_json::to_value(&st).unwrap();
        assert_eq!(v["kind"], "workspace");
        assert_eq!(v["quotaGb"], 5);
        assert_eq!(v["attachedEnvironment"], "env-1");
        let back: SnapshotState = serde_json::from_value(v).unwrap();
        assert_eq!(back, st);
    }

    /// A frozen environment's services carry `resources` for free — `SnapshotState::Environment`
    /// holds `Vec<model::Service>` verbatim, so the field needed no plumbing here, only the round
    /// trip proving it.
    #[test]
    fn a_frozen_environments_service_resources_round_trip() {
        let st = SnapshotState::Environment {
            services: vec![crate::model::Service {
                name: "buildkit".into(),
                image: "moby/buildkit".into(),
                command: vec![],
                env: Default::default(),
                mounts: vec![],
                ports: vec![],
                resources: Some(PodResources::default()),
            }],
            quota_gb: 10,
        };
        let v = serde_json::to_value(&st).unwrap();
        let back: SnapshotState = serde_json::from_value(v).unwrap();
        assert_eq!(back, st);
    }

    #[test]
    fn a_snapshot_spec_without_state_still_deserializes() {
        let s: SnapshotSpec = serde_json::from_value(serde_json::json!({
            "volume": "v", "owner": "o", "worktree": "v", "parent": "", "transient": false
        }))
        .unwrap();
        assert!(s.state.is_none());
        // and a None state is not written at all
        assert!(serde_json::to_value(&s).unwrap().get("state").is_none());
    }

    #[test]
    fn a_malformed_snapshot_state_is_dropped_not_a_deserialize_error() {
        let s: SnapshotSpec = serde_json::from_value(serde_json::json!({
            "volume": "v", "owner": "o", "worktree": "v", "parent": "",
            "transient": false, "state": {"kind": "bogus"}
        }))
        .unwrap();
        assert!(s.state.is_none());
    }

    #[test]
    fn of_workspace_copies_the_spec_and_falls_back_to_the_default_quota() {
        let mut w = Workspace::new("ws-1", WorkspaceSpec {
            owner: "o".into(), team: String::new(), name: "n".into(), region: "r".into(),
            image: "alpine:3.20".into(), storage: None, desired_state: DesiredState::Running,
            resources: PodResources::default(), packages: vec!["jq".into()],
            locks: vec![Lock {
                entry: "nodejs@20".into(), version: "20.20.2".into(), attr_path: "nodejs_20".into(),
                rev: "abc".into(), store_path: "/nix/store/x".into(),
                resolved_at: "2026-09-08T00:00:00Z".into(), source: LockSource::Nixhub,
            }],
            attached_environment: None,
        });
        match SnapshotState::of_workspace(&w) {
            SnapshotState::Workspace { image, packages, locks, quota_gb, attached_environment, .. } => {
                assert_eq!(image, "alpine:3.20"); assert_eq!(packages, vec!["jq"]);
                // The lock is frozen with the list; a restore that lost it would rebuild a
                // different version of the same entry.
                assert_eq!(locks.len(), 1); assert_eq!(locks[0].version, "20.20.2");
                assert_eq!(quota_gb, DEFAULT_WS_QUOTA_GB); assert_eq!(attached_environment, None);
            }
            other => panic!("{other:?}"),
        }
        w.spec.storage = Some(WorkspaceStorage { quota_gb: 42, source: None });
        assert!(matches!(SnapshotState::of_workspace(&w), SnapshotState::Workspace { quota_gb: 42, .. }));
    }

    #[test]
    fn snapshot_status_round_trips_and_omits_absent_ready_at() {
        let st = SnapshotStatus { phase: Phase::Working, ready_at: None };
        let v = serde_json::to_value(&st).unwrap();
        assert!(!v.as_object().unwrap().contains_key("readyAt"));
        let back: SnapshotStatus = serde_json::from_value(v).unwrap();
        assert_eq!(back, st);
    }

    #[test]
    fn volume_replica_spec_and_status_round_trip() {
        let spec = VolumeReplicaSpec { volume: "v".into(), node: "n".into() };
        let back: VolumeReplicaSpec = serde_json::from_value(serde_json::to_value(&spec).unwrap()).unwrap();
        assert_eq!(back, spec);

        let st = VolumeReplicaStatus {
            phase: "Synced".into(),
            branches: std::collections::BTreeMap::from([("main".to_string(), "abc123".to_string())]),
        };
        let v = serde_json::to_value(&st).unwrap();
        assert_eq!(v["phase"], "Synced");
        let back: VolumeReplicaStatus = serde_json::from_value(v).unwrap();
        assert_eq!(back, st);

        let empty = VolumeReplicaStatus { phase: "Syncing".into(), branches: Default::default() };
        let v = serde_json::to_value(&empty).unwrap();
        assert!(!v.as_object().unwrap().contains_key("branches"));
    }

    /// Nothing sets `deny_unknown_fields`, so an object stored before this task's cutover — still
    /// carrying `durable`, `compatibleNodes`, or `lastSyncAt` — keeps parsing after those fields
    /// are dropped from the schema and the struct entirely (`compatible_nodes` is no longer a
    /// tolerated field either — it is gone from `WorkspaceStatus`/`EnvironmentStatus`, same as
    /// `durable`). The value just goes nowhere: it disappears on the object's next write and
    /// nothing ever reads it again.
    #[test]
    fn dropped_fields_are_tolerated_on_deserialize() {
        let ws_status = serde_json::json!({
            "phase": "running",
            "durable": "abc123",
            "compatibleNodes": ["node-a", "node-b"],
        });
        serde_json::from_value::<WorkspaceStatus>(ws_status).expect("durable/compatibleNodes must still parse");

        let replica_status = serde_json::json!({
            "phase": "Synced",
            "lastSyncAt": "2026-09-01T00:00:00Z",
        });
        serde_json::from_value::<VolumeReplicaStatus>(replica_status).expect("lastSyncAt must still parse");
    }

    /// `CLUSTER_SETTING_META` names every field the admin write path/UI must know about a
    /// `Live`/`Boot` split for. A field added to the struct without a matching entry here would
    /// silently ship with no mark — meaning no reader ever gets told to roll.
    #[test]
    fn cluster_setting_meta_is_exhaustive() {
        use kube::CustomResourceExt;
        let crd = ClusterSettings::crd();
        let schema = crd.spec.versions[0].schema.as_ref().unwrap().open_api_v3_schema.as_ref().unwrap();
        let mut props: Vec<&str> =
            schema.properties.as_ref().unwrap()["spec"].properties.as_ref().unwrap().keys().map(|k| k.as_str()).collect();
        props.sort_unstable();
        let mut meta: Vec<&str> = CLUSTER_SETTING_META.iter().map(|(name, _, _)| *name).collect();
        meta.sort_unstable();
        assert_eq!(props, meta, "CLUSTER_SETTING_META must name exactly ClusterSettingsSpec's fields");
    }
}

#[cfg(test)]
mod request_tests {
    use super::*;

    fn base(kind: RequestKind) -> RequestSpec {
        RequestSpec {
            owner: "acme".into(),
            kind,
            requested_by: "meera".into(),
            reason: "more room".into(),
            quota: None,
            access: None,
            region: None,
            other: None,
        }
    }

    /// The wire form is what an operator reads with `kubectl get request -o yaml`, and what a
    /// stored object parses back from — both directions, so a rename cannot pass unnoticed.
    #[test]
    fn a_request_round_trips_through_its_wire_form() {
        let mut spec = base(RequestKind::Access);
        spec.access = Some(AccessAsk { team: "acme".into(), role: "admin".into() });
        let v = serde_json::to_value(&spec).unwrap();
        assert_eq!(v["kind"], "access");
        assert_eq!(v["requestedBy"], "meera");
        assert_eq!(v["access"]["role"], "admin");
        // Blocks for the other three kinds are absent, not null: a null would advertise a field
        // the request never carried.
        assert!(v.get("quota").is_none() && v.get("region").is_none() && v.get("other").is_none());
        assert_eq!(serde_json::from_value::<RequestSpec>(v).unwrap(), spec);
    }

    /// A request carrying somebody else's block is not a typo to tolerate: `approve` dispatches on
    /// `kind` and would silently ignore the block that was actually filled in.
    #[test]
    fn exactly_the_block_for_its_kind_must_be_present() {
        let mut ok = base(RequestKind::Quota);
        ok.quota = Some(RequestedQuota { workspaces: Some(9), ..Default::default() });
        assert_eq!(ok.validate(), Ok(()));

        let missing = base(RequestKind::Quota);
        assert_eq!(missing.validate(), Err("kind quota needs a quota block".to_string()));

        let mut extra = base(RequestKind::Quota);
        extra.quota = Some(RequestedQuota::default());
        extra.other = Some(OtherAsk { title: "t".into(), body: "b".into() });
        assert_eq!(extra.validate(), Err("only the quota block belongs on a quota request".to_string()));

        let mut wrong = base(RequestKind::Region);
        wrong.access = Some(AccessAsk { team: "acme".into(), role: "admin".into() });
        assert_eq!(wrong.validate(), Err("kind region needs a region block".to_string()));
    }

    /// Only the three directory roles; anything else would reach `grant_access` as a role nothing
    /// can map, and a 500 on approve is a decision that half-happened.
    #[test]
    fn an_access_request_takes_only_a_real_role() {
        let mut spec = base(RequestKind::Access);
        spec.access = Some(AccessAsk { team: "acme".into(), role: "superuser".into() });
        assert_eq!(spec.validate(), Err("role must be member, admin or owner".to_string()));
    }

    /// `regions` is a granted list, and an empty one is omitted so a merge patch of a `QuotaSpec`
    /// that never mentions regions (every `PUT /admin/quota/{owner}` body) cannot erase a grant.
    #[test]
    fn an_empty_region_grant_is_omitted_from_a_quota_patch() {
        let v = serde_json::to_value(default_quota(false)).unwrap();
        assert!(v.get("regions").is_none());
    }

    #[test]
    fn a_port_with_no_mapping_is_answered_on_the_same_number() {
        let i = Intercept {
            service: "api".into(),
            workspace: "ws-1".into(),
            ports: vec![PortMap { service: 8080, workspace: 3000 }],
        };
        assert_eq!(i.workspace_port(8080), 3000, "the mapped one");
        assert_eq!(i.workspace_port(9090), 9090, "an unmapped port keeps its number");
    }

    /// Every stored Environment predates this field and must still parse.
    #[test]
    fn an_environment_without_intercepts_still_parses() {
        let v = serde_json::json!({"owner":"a","team":"","name":"n","region":"r","services":[],"desiredState":"running"});
        let s: EnvironmentSpec = serde_json::from_value(v).unwrap();
        assert!(s.intercepts.is_empty());
    }
}
