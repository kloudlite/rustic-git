//! Watch transitions → event rows. These mappers are the whole value: the watcher plumbing around
//! them is kube-rs's. The property that matters is idempotence — a restart replays the watch, and
//! every row it re-emits must carry the id it carried the first time, and the timestamp it carried
//! the first time, since both come from the object and never from the clock.

use k8s_openapi::api::core::v1::{Node, NodeCondition, NodeSpec, NodeStatus};
use kube::api::ObjectMeta;
use rustic_git_workspaces::crd::{self, DesiredState, Phase, RequestState};
use rustic_git_workspaces::history::watch::{
    environment_events, event_id, node_events, quota_request_events, region_events,
    snapshot_deleted, snapshot_events, volume_events, workspace_events,
};

fn meta(name: &str, uid: &str, rv: &str) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.into()),
        uid: Some(uid.into()),
        resource_version: Some(rv.into()),
        ..Default::default()
    }
}

fn ws(uid: &str, rv: &str, phase: Phase) -> crd::Workspace {
    let mut w = crd::Workspace::new(
        "ws-abc",
        crd::WorkspaceSpec {
            owner: "acme".into(),
            team: String::new(),
            name: "abc".into(),
            region: "eu".into(),
            image: "img:1".into(),
            storage: None,
            desired_state: DesiredState::Running,
            resources: Default::default(),
            packages: Vec::new(),
            attached_environment: None,
        },
    );
    w.metadata = meta("ws-abc", uid, rv);
    w.status = Some(crd::WorkspaceStatus {
        phase,
        ..Default::default()
    });
    w
}

#[test]
fn the_id_is_uid_resource_version_and_transition() {
    assert_eq!(event_id("uid-1", "4711", "created"), "uid-1:4711:created");
}

/// First sight of an object is `created` — a fresh watch has no previous state, and the id makes
/// the replay after a restart collapse onto the same row rather than double-counting.
#[test]
fn first_sight_of_a_workspace_is_created() {
    let rows = workspace_events(None, &ws("uid-1", "1", Phase::Pending), "westeurope-k3s");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "workspace.created");
    assert_eq!(rows[0].id, "uid-1:1:created");
    // spec.owner is truth — never a label.
    assert_eq!(rows[0].owner, "acme");
    assert_eq!(rows[0].target, "ws-abc");
    assert_eq!(rows[0].region, "westeurope-k3s");
}

#[test]
fn a_phase_change_into_ready_is_started_and_into_stopped_is_stopped() {
    let before = ws("uid-1", "1", Phase::Creating);
    let started = workspace_events(Some(&before), &ws("uid-1", "2", Phase::Ready), "eu");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].kind, "workspace.started");
    assert_eq!(started[0].id, "uid-1:2:started");

    let running = ws("uid-1", "2", Phase::Ready);
    let stopped = workspace_events(Some(&running), &ws("uid-1", "3", Phase::Stopped), "eu");
    assert_eq!(stopped.len(), 1);
    assert_eq!(stopped[0].kind, "workspace.stopped");
}

/// A reconcile that rewrites status without changing the phase is the overwhelmingly common event.
/// It must produce nothing, or the table fills with noise and every timeline becomes unreadable.
#[test]
fn an_unchanged_phase_produces_no_event() {
    let before = ws("uid-1", "2", Phase::Ready);
    assert!(workspace_events(Some(&before), &ws("uid-1", "3", Phase::Ready), "eu").is_empty());
}

/// The whole reason `ts` is not `Utc::now()`: a re-listed object must produce a byte-identical row.
#[test]
fn the_same_object_seen_twice_produces_the_same_row_including_its_timestamp() {
    let mut w = ws("uid-1", "1", Phase::Pending);
    w.metadata.creation_timestamp = Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
        "2026-09-04T10:00:00Z".parse().unwrap(),
    ));
    let a = workspace_events(None, &w, "eu");
    let b = workspace_events(None, &w, "eu");
    assert_eq!(a[0].id, b[0].id);
    assert_eq!(a[0].ts, b[0].ts);
    assert_eq!(a[0].ts.to_rfc3339(), "2026-09-04T10:00:00+00:00");
}

fn env(uid: &str, rv: &str, phase: Phase) -> crd::Environment {
    let mut e = crd::Environment::new(
        "env-1",
        crd::EnvironmentSpec {
            owner: "acme".into(),
            name: "prod".into(),
            region: "eu".into(),
            services: Vec::new(),
            storage: None,
            desired_state: DesiredState::Running,
            restore: None,
        },
    );
    e.metadata = meta("env-1", uid, rv);
    e.status = Some(crd::EnvironmentStatus {
        phase,
        ..Default::default()
    });
    e
}

#[test]
fn an_environment_reaching_running_is_started() {
    let before = env("uid-e", "1", Phase::Creating);
    let rows = environment_events(Some(&before), &env("uid-e", "2", Phase::Running), "eu");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "environment.started");
    assert_eq!(rows[0].id, "uid-e:2:started");
    assert!(
        environment_events(None, &env("uid-e", "1", Phase::Pending), "eu")[0]
            .kind
            .ends_with(".created")
    );
}

fn snap(rv: &str, phase: Phase, transient: bool) -> crd::Snapshot {
    let mut s = crd::Snapshot::new(
        "snap-1",
        crd::SnapshotSpec {
            volume: "vol-1".into(),
            owner: "acme".into(),
            worktree: "ws-abc".into(),
            parent: String::new(),
            message: None,
            transient,
            state: None,
        },
    );
    s.metadata = meta("snap-1", "uid-s", rv);
    s.status = Some(crd::SnapshotStatus {
        phase,
        ready_at: match phase {
            Phase::Ready => Some("2026-09-04T10:00:00Z".into()),
            _ => None,
        },
    });
    s
}

#[test]
fn a_snapshot_becoming_ready_is_one_event() {
    let before = snap("1", Phase::Working, false);
    let rows = snapshot_events(Some(&before), &snap("2", Phase::Ready, false), "eu");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "snapshot.ready");
    assert_eq!(rows[0].owner, "acme");
    // `readyAt` is the cut's own instant, and it is what the row carries.
    assert_eq!(rows[0].ts.to_rfc3339(), "2026-09-04T10:00:00+00:00");

    // Already ready: a re-observed object is not a second cut.
    let ready = snap("2", Phase::Ready, false);
    assert!(snapshot_events(Some(&ready), &snap("3", Phase::Ready, false), "eu").is_empty());
}

/// Sync points are cut and pruned every beat; only a real push's deletion is history.
#[test]
fn deleting_a_sync_point_is_not_an_event_but_deleting_a_snapshot_is() {
    assert!(snapshot_deleted(&snap("4", Phase::Ready, true), "eu").is_empty());
    let rows = snapshot_deleted(&snap("4", Phase::Ready, false), "eu");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "snapshot.deleted");
    assert_eq!(rows[0].id, "uid-s:4:deleted");
}

fn vol(rv: &str, node: &str, phase: Phase) -> crd::Volume {
    let mut v = crd::Volume::new(
        "vol-1",
        crd::VolumeSpec {
            owner: "acme".into(),
            team: String::new(),
            node_name: node.into(),
            region: "eu".into(),
            quota_gb: 20,
            replicas: 2,
            source: None,
            restore_to: None,
        },
    );
    v.metadata = meta("vol-1", "uid-v", rv);
    v.status = Some(crd::VolumeStatus {
        phase,
        ..Default::default()
    });
    v
}

/// The pin lives in `spec.nodeName` — `VolumeStatus` deliberately names no node.
#[test]
fn a_volume_changing_node_is_moved_and_losing_it_is_released() {
    let on_a = vol("1", "node-a", Phase::Ready);
    let moved = volume_events(Some(&on_a), &vol("2", "node-b", Phase::Ready), "eu");
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0].kind, "volume.moved");
    assert_eq!(moved[0].id, "uid-v:2:moved");

    let released = volume_events(Some(&on_a), &vol("3", "", Phase::Ready), "eu");
    assert_eq!(released.len(), 1);
    assert_eq!(released[0].kind, "volume.released");

    let dead = volume_events(Some(&on_a), &vol("4", "", Phase::Unavailable), "eu");
    let kinds: Vec<&str> = dead.iter().map(|r| r.kind.as_str()).collect();
    assert_eq!(kinds, vec!["volume.released", "volume.unavailable"]);
    assert_ne!(dead[0].id, dead[1].id);

    assert!(volume_events(Some(&on_a), &vol("5", "node-a", Phase::Ready), "eu").is_empty());
}

fn qreq(rv: &str, state: Option<RequestState>) -> crd::QuotaRequest {
    let mut q = crd::QuotaRequest::new(
        "qr-1",
        crd::QuotaRequestSpec {
            owner: "acme".into(),
            requested: crd::RequestedQuota {
                workspaces: Some(10),
                ..Default::default()
            },
            reason: "more room".into(),
        },
    );
    q.metadata = meta("qr-1", "uid-q", rv);
    q.status = state.map(|state| crd::QuotaRequestStatus {
        state,
        decided_by: Some("root@acme.test".into()),
        decided_at: None,
        note: None,
    });
    q
}

/// `QuotaRequestSpec` has no `requestedBy`, so the asker IS the owner until `Request` lands; the
/// decider is a superadmin and owns nothing, so it is the `actor`, never the `owner`.
#[test]
fn a_quota_request_is_opened_then_decided_once() {
    let opened = quota_request_events(None, &qreq("1", None), "eu");
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].kind, "request.opened");
    assert_eq!(opened[0].actor, "acme");
    assert_eq!(opened[0].owner, "acme");

    let pending = qreq("1", Some(RequestState::Pending));
    let approved = quota_request_events(
        Some(&pending),
        &qreq("2", Some(RequestState::Approved)),
        "eu",
    );
    assert_eq!(approved.len(), 1);
    assert_eq!(approved[0].kind, "request.approved");
    assert_eq!(approved[0].id, "uid-q:2:approved");
    assert_eq!(approved[0].actor, "root@acme.test");
    assert_eq!(approved[0].owner, "acme");

    let denied = quota_request_events(Some(&pending), &qreq("3", Some(RequestState::Denied)), "eu");
    assert_eq!(denied[0].kind, "request.denied");

    // A status rewrite that does not change the state is not a second decision.
    let decided = qreq("2", Some(RequestState::Approved));
    assert!(quota_request_events(
        Some(&decided),
        &qreq("4", Some(RequestState::Approved)),
        "eu"
    )
    .is_empty());
}

fn region(rv: &str, status: &str) -> crd::Region {
    let mut r = crd::Region::new(
        "eu",
        crd::RegionSpec {
            name: "Europe".into(),
            status: status.into(),
        },
    );
    r.metadata = meta("eu", "uid-r", rv);
    r
}

#[test]
fn a_region_flipping_status_is_one_event_each_way() {
    let first = region_events(None, &region("1", "active"));
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].kind, "region.activated");
    // A region belongs to no owner.
    assert_eq!(first[0].owner, "");

    let active = region("1", "active");
    let off = region_events(Some(&active), &region("2", "inactive"));
    assert_eq!(off[0].kind, "region.deactivated");
    assert!(region_events(Some(&active), &region("3", "active")).is_empty());
}

fn node(name: &str, uid: &str, rv: &str, ready: &str, unschedulable: bool) -> Node {
    Node {
        metadata: meta(name, uid, rv),
        spec: Some(NodeSpec {
            unschedulable: Some(unschedulable),
            ..Default::default()
        }),
        status: Some(NodeStatus {
            conditions: Some(vec![NodeCondition {
                type_: "Ready".into(),
                status: ready.into(),
                ..Default::default()
            }]),
            ..Default::default()
        }),
    }
}

#[test]
fn a_node_going_notready_and_being_cordoned_are_separate_events() {
    let before = node("node-1", "uid-n", "1", "True", false);
    let rows = node_events(
        Some(&before),
        &node("node-1", "uid-n", "2", "False", false),
        "eu",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "node.notready");

    let rows = node_events(
        Some(&before),
        &node("node-1", "uid-n", "3", "True", true),
        "eu",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "node.cordoned");
    // A node belongs to a cluster, not to an owner: inventing one would attribute it to somebody.
    assert_eq!(rows[0].owner, "");
}

/// Both at once must produce both, not whichever the mapper checked first.
#[test]
fn a_node_that_goes_notready_and_cordoned_at_once_produces_both() {
    let before = node("node-1", "uid-n", "1", "True", false);
    let rows = node_events(
        Some(&before),
        &node("node-1", "uid-n", "2", "False", true),
        "eu",
    );
    let kinds: Vec<&str> = rows.iter().map(|r| r.kind.as_str()).collect();
    assert!(
        kinds.contains(&"node.notready") && kinds.contains(&"node.cordoned"),
        "{kinds:?}"
    );
    // Two transitions off one resourceVersion still need distinct ids.
    assert_ne!(rows[0].id, rows[1].id);
}

/// The agent's own drain stamp, whose first word is the state.
#[test]
fn the_decommission_stamp_produces_draining_then_drained() {
    let before = node("node-1", "uid-n", "1", "True", false);
    let mut draining = node("node-1", "uid-n", "2", "True", false);
    draining.metadata.labels = Some(
        [(
            crd::DECOMMISSION_STATUS.to_string(),
            "draining running=1 owned=2 copies=0 thin=0".to_string(),
        )]
        .into(),
    );
    let rows = node_events(Some(&before), &draining, "eu");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "node.draining");

    let mut drained = draining.clone();
    drained.metadata.resource_version = Some("3".into());
    drained.metadata.labels = Some(
        [(
            crd::DECOMMISSION_STATUS.to_string(),
            "drained 2026-09-04T10:00:00Z".to_string(),
        )]
        .into(),
    );
    let rows = node_events(Some(&draining), &drained, "eu");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "node.drained");
    assert_eq!(rows[0].id, "uid-n:3:drained");

    // An unchanged stamp is not a second drain.
    assert!(node_events(Some(&drained), &drained, "eu").is_empty());
}
