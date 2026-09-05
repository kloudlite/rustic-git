//! Kubernetes watches turned into `kloudlite.events` rows: one reflector set per region, plus one for
//! central, in the admin process.
//!
//! Every mapper here is PURE — previous state, next state, out come rows — so the transitions are
//! unit-testable without a cluster, and the watcher plumbing below carries no rules of its own.
//!
//! Idempotence is the whole trick. A restart re-lists every object and a watch bookmark can replay
//! entries, so the id is `{uid}:{resourceVersion}:{transition}`: the same fact observed twice is
//! literally the same row, and `events`' ReplacingMergeTree collapses it. For the same reason `ts`
//! is DERIVED FROM THE OBJECT (a status timestamp, else the newest managed-field time, else the
//! creation time) and never stamped at insert — a replayed row must be byte-identical to the one it
//! replaces, and `chrono::Utc::now()` would make every restart write a second, differently-timed
//! copy of the same fact.
//!
//! `spec.owner` is truth (CLAUDE.md): every `owner` field below reads the spec, never the
//! `kloudlite.io/owner` label, which is a view maintained for label selectors.

use super::events::{write_events, EventRow};
use super::History;
use crate::crd::{self, Phase, RequestState};
use futures::StreamExt;
use k8s_openapi::api::core::v1::Node;
use kube::api::ResourceExt;
use std::collections::HashMap;
use std::sync::Arc;

/// Reconnect delays for `watch_kind`. Short enough that an ordinary watch expiry costs nothing,
/// capped so a watch that will never start (a missing RBAC verb) settles into a slow retry.
const MIN_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);
const MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(300);
/// How often a still-failing watch repeats its warn.
const LOUD_EVERY: u64 = 20;

/// The `region` value the admin process's OWN cluster writes, matching `EventRow::region`'s
/// convention everywhere else in this module.
pub const CENTRAL: &str = "central";

pub fn event_id(uid: &str, resource_version: &str, transition: &str) -> String {
    format!("{uid}:{resource_version}:{transition}")
}

/// When the API server last recorded a write to this object, as close as its metadata gets: the
/// newest `managedFields` entry's time, falling back to the creation time.
///
/// `None` for an object carrying neither — the caller decides what to do, and no mapper may quietly
/// substitute the wall clock (see the module doc on why `ts` must be a property of the object).
fn managed_at<K: ResourceExt>(o: &K) -> Option<chrono::DateTime<chrono::Utc>> {
    let m = o.meta();
    m.managed_fields
        .as_ref()
        .and_then(|f| {
            f.iter()
                .filter_map(|e| e.time.as_ref())
                .filter_map(k8s_time)
                .max()
        })
        .or_else(|| m.creation_timestamp.as_ref().and_then(k8s_time))
}

/// k8s-openapi 0.28 stamps times as `jiff::Timestamp`; `EventRow` is chrono, like every other row
/// this tier writes. Millisecond precision, which is exactly what the `DateTime64(3)` column keeps.
fn k8s_time(
    t: &k8s_openapi::apimachinery::pkg::apis::meta::v1::Time,
) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::from_timestamp_millis(t.0.as_millisecond())
}

/// The timestamp a row gets when the object offers nothing better. Epoch, not `now`: a row with an
/// obviously wrong-but-stable time still deduplicates on replay, while `now` would not — and an
/// object with no `managedFields` and no `creationTimestamp` is a synthetic one from a test, never
/// something an API server produced.
fn epoch() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::UNIX_EPOCH
}

/// A create's timestamp is `creationTimestamp`, for the same reason its id drops the
/// resourceVersion: both must be properties of the create itself, or a restart re-emits the row
/// with a later time. Any other transition takes the object's last write.
fn transition_at<K: ResourceExt>(o: &K, first_sight: bool) -> chrono::DateTime<chrono::Utc> {
    match first_sight {
        true => o.meta().creation_timestamp.as_ref().and_then(k8s_time),
        false => managed_at(o),
    }
    .unwrap_or_else(epoch)
}

/// The one row constructor every mapper goes through, so no mapper can forget the id scheme.
#[allow(clippy::too_many_arguments)]
fn row(
    ts: chrono::DateTime<chrono::Utc>,
    uid: &str,
    resource_version: &str,
    transition: &str,
    kind: &str,
    actor: &str,
    owner: &str,
    target: &str,
    region: &str,
    attrs: serde_json::Value,
) -> EventRow {
    EventRow {
        ts,
        id: event_id(uid, resource_version, transition),
        kind: kind.to_string(),
        actor: actor.to_string(),
        owner: owner.to_string(),
        target: target.to_string(),
        region: region.to_string(),
        attrs,
    }
}

/// `Pending`/`Creating` → `Ready`/`Running` is a start; anything → `Stopped` is a stop. Everything
/// else is a status rewrite, and a reconcile does hundreds of those — emitting them would bury the
/// timeline in noise.
fn phase_transition(prev: Option<Phase>, next: Phase) -> Option<&'static str> {
    let started = matches!(next, Phase::Ready | Phase::Running);
    let was_started = matches!(prev, Some(Phase::Ready) | Some(Phase::Running));
    match (prev, next) {
        (None, _) => None, // first sight is `created`, handled by the caller
        (Some(p), n) if p == n => None,
        (_, Phase::Stopped) => Some("stopped"),
        _ if started && !was_started => Some("started"),
        _ => None,
    }
}

fn uid_rv<K: ResourceExt>(o: &K) -> (String, String) {
    (
        o.uid().unwrap_or_default(),
        o.resource_version().unwrap_or_default(),
    )
}

/// The two rows every parent kind produces: `created` on first sight, then phase transitions.
/// Factored because `Workspace` and `Environment` differ only in the kind word and the field paths.
#[allow(clippy::too_many_arguments)]
fn parent_rows(
    ts: chrono::DateTime<chrono::Utc>,
    uid: &str,
    rv: &str,
    kind_prefix: &str,
    owner: &str,
    target: &str,
    region: &str,
    prev_phase: Option<Phase>,
    next_phase: Phase,
    first_sight: bool,
    attrs: serde_json::Value,
) -> Vec<EventRow> {
    if first_sight {
        // `rv` is deliberately NOT part of a create's id. First sight is not an observed
        // transition: it is "this object exists", and a restart re-lists the object at whatever
        // resourceVersion it has reached by then. Keying on rv would mint a brand-new id on every
        // restart and the ReplacingMergeTree would keep them all — one `workspace.created` per
        // restart, forever. A uid is unique to a create, so `0` stands in for "no transition".
        return vec![row(
            ts,
            uid,
            "0",
            "created",
            &format!("{kind_prefix}.created"),
            "",
            owner,
            target,
            region,
            attrs,
        )];
    }
    match phase_transition(prev_phase, next_phase) {
        // A controller transition has no human actor; `/v1`'s audit rows carry those.
        Some(t) => vec![row(
            ts,
            uid,
            rv,
            t,
            &format!("{kind_prefix}.{t}"),
            "",
            owner,
            target,
            region,
            attrs,
        )],
        None => Vec::new(),
    }
}

pub fn workspace_events(
    prev: Option<&crd::Workspace>,
    next: &crd::Workspace,
    region: &str,
) -> Vec<EventRow> {
    let (uid, rv) = uid_rv(next);
    parent_rows(
        transition_at(next, prev.is_none()),
        &uid,
        &rv,
        "workspace",
        &next.spec.owner,
        &next.name_any(),
        region,
        prev.and_then(|p| p.status.as_ref()).map(|s| s.phase),
        next.status.as_ref().map(|s| s.phase).unwrap_or_default(),
        prev.is_none(),
        serde_json::json!({ "image": next.spec.image }),
    )
}

pub fn environment_events(
    prev: Option<&crd::Environment>,
    next: &crd::Environment,
    region: &str,
) -> Vec<EventRow> {
    let (uid, rv) = uid_rv(next);
    parent_rows(
        transition_at(next, prev.is_none()),
        &uid,
        &rv,
        "environment",
        &next.spec.owner,
        &next.name_any(),
        region,
        prev.and_then(|p| p.status.as_ref()).map(|s| s.phase),
        next.status.as_ref().map(|s| s.phase).unwrap_or_default(),
        prev.is_none(),
        serde_json::json!({ "services": next.spec.services.len() }),
    )
}

/// A snapshot's only interesting transition is becoming `Ready` — that is the instant its bytes
/// exist and it becomes a restore target. `created` is not emitted: a sync-point cut every beat
/// would drown every other event in the table.
pub fn snapshot_events(
    prev: Option<&crd::Snapshot>,
    next: &crd::Snapshot,
    region: &str,
) -> Vec<EventRow> {
    let (uid, rv) = uid_rv(next);
    let was_ready = matches!(
        prev.and_then(|p| p.status.as_ref()).map(|s| s.phase),
        Some(Phase::Ready)
    );
    let st = next.status.as_ref();
    if !matches!(st.map(|s| s.phase), Some(Phase::Ready)) || was_ready {
        return Vec::new();
    }
    // `readyAt` is the exact instant the cut landed, written by the node that took it — a better
    // timestamp than any metadata time, and the reason `Snapshot` carries it at all.
    let ts = st
        .and_then(|s| s.ready_at.as_deref())
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&chrono::Utc))
        .or_else(|| managed_at(next))
        .unwrap_or_else(epoch);
    vec![row(
        ts,
        &uid,
        &rv,
        "ready",
        "snapshot.ready",
        "",
        &next.spec.owner,
        &next.name_any(),
        region,
        serde_json::json!({
            "volume": next.spec.volume,
            "worktree": next.spec.worktree,
            "transient": next.spec.transient,
        }),
    )]
}

/// A volume's events are the ones an operator investigates an incident with: it moved node, it was
/// released, or it went `Unavailable` because its node died.
///
/// The pin is `spec.nodeName` — `VolumeStatus` deliberately carries no node (the spec field is the
/// one place allowed to name a node, so two places can never disagree about where the data is).
pub fn volume_events(
    prev: Option<&crd::Volume>,
    next: &crd::Volume,
    region: &str,
) -> Vec<EventRow> {
    let (uid, rv) = uid_rv(next);
    let ts = managed_at(next).unwrap_or_else(epoch);
    let mut out = Vec::new();
    let prev_node = prev.map(|p| p.spec.node_name.as_str()).unwrap_or_default();
    let next_node = next.spec.node_name.as_str();
    if prev.is_some() && prev_node != next_node {
        // Losing the pin and gaining one are different facts to an operator reading a timeline.
        let (t, kind) = match next_node.is_empty() {
            true => ("released", "volume.released"),
            false => ("moved", "volume.moved"),
        };
        out.push(row(
            ts,
            &uid,
            &rv,
            t,
            kind,
            "",
            &next.spec.owner,
            &next.name_any(),
            region,
            serde_json::json!({ "from": prev_node, "to": next_node }),
        ));
    }
    let was = prev.and_then(|p| p.status.as_ref()).map(|s| s.phase);
    let is = next.status.as_ref().map(|s| s.phase).unwrap_or_default();
    if is == Phase::Unavailable && was != Some(Phase::Unavailable) {
        out.push(row(
            ts,
            &uid,
            &rv,
            "unavailable",
            "volume.unavailable",
            "",
            &next.spec.owner,
            &next.name_any(),
            region,
            serde_json::json!({ "node": next_node }),
        ));
    }
    out
}

/// `QuotaRequest` only, deliberately — `request_events` below covers the generic `Request`. Both
/// emit the same `request.*` kinds, so the console's timeline spans the two CRDs.
// ponytail: two near-identical mappers until the one-shot migration retires `QuotaRequest`; delete
// this one then rather than unifying them, since a union type would have to be unpicked again.
pub fn quota_request_events(
    prev: Option<&crd::QuotaRequest>,
    next: &crd::QuotaRequest,
    region: &str,
) -> Vec<EventRow> {
    let (uid, rv) = uid_rv(next);
    let ts = managed_at(next).unwrap_or_else(epoch);
    if prev.is_none() {
        // `QuotaRequestSpec` has no `requestedBy`: until `Request` lands, the asker is the owner
        // the raise is for. Inventing a person from the decision fields would name the wrong one.
        return vec![row(
            ts,
            &uid,
            &rv,
            "opened",
            "request.opened",
            &next.spec.owner,
            &next.spec.owner,
            &next.name_any(),
            region,
            serde_json::json!({ "reason": next.spec.reason }),
        )];
    }
    let state = next
        .status
        .as_ref()
        .map(|s| s.state)
        .unwrap_or(RequestState::Pending);
    let prev_state = prev
        .and_then(|p| p.status.as_ref())
        .map(|s| s.state)
        .unwrap_or(RequestState::Pending);
    let word = match state {
        _ if state == prev_state => return Vec::new(),
        RequestState::Approved => "approved",
        RequestState::Denied => "denied",
        // A decided request going back to pending is not a thing `/v1` can do; nothing to record.
        RequestState::Pending => return Vec::new(),
    };
    let decided_by = next
        .status
        .as_ref()
        .and_then(|s| s.decided_by.clone())
        .unwrap_or_default();
    vec![row(
        ts,
        &uid,
        &rv,
        word,
        &format!("request.{word}"),
        &decided_by,
        &next.spec.owner,
        &next.name_any(),
        region,
        serde_json::json!({ "note": next.status.as_ref().and_then(|s| s.note.clone()) }),
    )]
}

/// The generic `Request` CRD, beside `quota_request_events` rather than replacing it: the two
/// coexist until the one-shot migration retires `QuotaRequest`, so legacy objects still stream and
/// still need their mapper. Same `request.*` kinds, so the console's timeline spans both.
pub fn request_events(
    prev: Option<&crd::Request>,
    next: &crd::Request,
    region: &str,
) -> Vec<EventRow> {
    let (uid, rv) = uid_rv(next);
    let ts = managed_at(next).unwrap_or_else(epoch);
    // In `attrs` and not in the kind, so a reader filtering `request.opened` still sees all four —
    // and so a new `RequestKind` needs no new event kind.
    let ask = next.spec.kind.as_str();
    if prev.is_none() {
        // Unlike `QuotaRequest`, the asker is a spec field here and never has to be inferred.
        return vec![row(
            ts,
            &uid,
            &rv,
            "opened",
            "request.opened",
            &next.spec.requested_by,
            &next.spec.owner,
            &next.name_any(),
            region,
            serde_json::json!({ "kind": ask, "reason": next.spec.reason }),
        )];
    }
    let state = next
        .status
        .as_ref()
        .map(|s| s.state)
        .unwrap_or(RequestState::Pending);
    let prev_state = prev
        .and_then(|p| p.status.as_ref())
        .map(|s| s.state)
        .unwrap_or(RequestState::Pending);
    let word = match state {
        _ if state == prev_state => return Vec::new(),
        RequestState::Approved => "approved",
        RequestState::Denied => "denied",
        // A decided request going back to pending is not a thing `/v1` can do; nothing to record.
        RequestState::Pending => return Vec::new(),
    };
    let decided_by = next
        .status
        .as_ref()
        .and_then(|s| s.decided_by.clone())
        .unwrap_or_default();
    vec![row(
        ts,
        &uid,
        &rv,
        word,
        &format!("request.{word}"),
        &decided_by,
        &next.spec.owner,
        &next.name_any(),
        region,
        serde_json::json!({
            "kind": ask,
            "note": next.status.as_ref().and_then(|s| s.note.clone()),
        }),
    )]
}

/// Regions live in the central cluster and belong to no owner. `status` here is the region's own
/// `spec.status` (`active`/`inactive`), which `/v1/regions` writes — a spec field, so it is truth.
pub fn region_events(prev: Option<&crd::Region>, next: &crd::Region) -> Vec<EventRow> {
    let (uid, rv) = uid_rv(next);
    let is_active = next.spec.status == "active";
    if prev.map(|p| p.spec.status == "active") == Some(is_active) {
        return Vec::new();
    }
    let word = match is_active {
        true => "activated",
        false => "deactivated",
    };
    vec![row(
        managed_at(next).unwrap_or_else(epoch),
        &uid,
        &rv,
        word,
        &format!("region.{word}"),
        "",
        "",
        &next.name_any(),
        &next.name_any(),
        serde_json::json!({ "name": next.spec.name }),
    )]
}

fn ready_condition(n: &Node) -> Option<String> {
    n.status
        .as_ref()?
        .conditions
        .as_ref()?
        .iter()
        .find(|c| c.type_ == "Ready")
        .map(|c| c.status.clone())
}

/// Ready/NotReady, cordon and the two decommission stamps — the four things that explain why work
/// stopped landing on a node. Several may change in one update, so this returns a Vec and every
/// transition gets its own id suffix.
///
/// A node belongs to a cluster, not to an owner: `owner` stays empty rather than attributing a
/// drain to somebody.
pub fn node_events(prev: Option<&Node>, next: &Node, region: &str) -> Vec<EventRow> {
    let (uid, rv) = uid_rv(next);
    // A Ready condition carries its own `lastTransitionTime`, which is the moment the kubelet
    // stopped answering — far more useful than when the object happened to be written.
    let ts = next
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .and_then(|c| c.iter().find(|c| c.type_ == "Ready"))
        .and_then(|c| c.last_transition_time.as_ref())
        .and_then(k8s_time)
        .or_else(|| managed_at(next))
        .unwrap_or_else(epoch);
    let mut out = Vec::new();
    let is_ready = ready_condition(next);
    if prev.is_some() && prev.and_then(ready_condition) != is_ready {
        let t = match is_ready.as_deref() {
            Some("True") => "ready",
            _ => "notready",
        };
        out.push(row(
            ts,
            &uid,
            &rv,
            t,
            &format!("node.{t}"),
            "",
            "",
            &next.name_any(),
            region,
            serde_json::json!({ "ready": is_ready }),
        ));
    }
    let cordoned = |n: &Node| {
        n.spec
            .as_ref()
            .and_then(|s| s.unschedulable)
            .unwrap_or(false)
    };
    if prev.is_some_and(|p| !cordoned(p)) && cordoned(next) {
        out.push(row(
            ts,
            &uid,
            &rv,
            "cordoned",
            "node.cordoned",
            "",
            "",
            &next.name_any(),
            region,
            serde_json::json!({}),
        ));
    }
    // The agent stamps its own progress here (`draining running=…`, then `drained <RFC3339>`);
    // the first word is the state, and only the two we name are events.
    let status_word = |n: &Node| {
        n.labels()
            .get(crd::DECOMMISSION_STATUS)
            .or_else(|| n.annotations().get(crd::DECOMMISSION_STATUS))
            .and_then(|v| v.split_whitespace().next())
            .map(str::to_string)
    };
    let is = status_word(next);
    // `prev.is_some()` like the Ready branch: the initial list is state, not a transition, and a
    // node that already carries the stamp would otherwise re-emit it on every watch restart —
    // each under a fresh resourceVersion, so nothing collapses them.
    if prev.is_some() && prev.and_then(status_word) != is {
        if let Some(w) = is
            .as_deref()
            .filter(|w| *w == "draining" || *w == "drained")
        {
            out.push(row(
                ts,
                &uid,
                &rv,
                w,
                &format!("node.{w}"),
                "",
                "",
                &next.name_any(),
                region,
                serde_json::json!({}),
            ));
        }
    }
    out
}

/// The row a disappearing object leaves behind. `ts` is the deletion stamp when the API server set
/// one (a graceful delete goes through `deletionTimestamp` first), else the object's last write —
/// still a property of the object, so a replayed delete is the same row.
pub fn deleted_event<K: ResourceExt>(obj: &K, kind: &str, owner: &str, region: &str) -> EventRow {
    let (uid, rv) = uid_rv(obj);
    let ts = obj
        .meta()
        .deletion_timestamp
        .as_ref()
        .and_then(k8s_time)
        .or_else(|| managed_at(obj))
        .unwrap_or_else(epoch);
    row(
        ts,
        &uid,
        &rv,
        "deleted",
        &format!("{kind}.deleted"),
        "",
        owner,
        &obj.name_any(),
        region,
        serde_json::json!({}),
    )
}

pub fn workspace_deleted(o: &crd::Workspace, region: &str) -> Vec<EventRow> {
    vec![deleted_event(o, "workspace", &o.spec.owner, region)]
}

pub fn environment_deleted(o: &crd::Environment, region: &str) -> Vec<EventRow> {
    vec![deleted_event(o, "environment", &o.spec.owner, region)]
}

/// A sync point is cut and pruned every beat; only a real snapshot's deletion is history.
pub fn snapshot_deleted(o: &crd::Snapshot, region: &str) -> Vec<EventRow> {
    match o.spec.transient {
        true => Vec::new(),
        false => vec![deleted_event(o, "snapshot", &o.spec.owner, region)],
    }
}

/// A kind whose deletion is not in the event catalogue (`Volume`, `Region`, `Node`): the volume's
/// life is already told by `volume.released`, and a node object leaving is the cluster's business.
fn no_delete<K>(_: &K, _: &str) -> Vec<EventRow> {
    Vec::new()
}

/// One reflector-shaped loop per kind: keep the previous version of each object by uid, hand both
/// to the mapper, write whatever comes out.
///
/// A HashMap of previous state, not `kube::runtime::reflector`: the mappers need the PREVIOUS
/// object and a Store gives the current one. The map is bounded by the number of objects in the
/// cluster (the same bound the Store has) and is dropped whenever the watcher restarts, which
/// re-lists everything — hence the `created` rows on restart, which the id scheme collapses.
///
/// A watch that cannot start at all (no RBAC, no CRD, unreachable cluster) must never block or
/// fail boot, so it is retried forever — but a permanently missing RBAC verb would otherwise
/// reconnect every few seconds in silence, so the FIRST failure of a run is logged at warn (that
/// is the one an operator needs, and it names the kind), then every `LOUD_EVERY`th, and the delay
/// doubles to `MAX_BACKOFF` instead of hammering a cluster that is not coming back.
// ponytail: this loop is untested — `kube_test::mock_client` answers one canned JSON body per
// request and cannot stream a watch, so there is no harness for it. Every rule lives in the
// mappers above, which ARE tested; add a streaming mock the day the loop grows a decision.
async fn watch_kind<K>(
    client: kube::Client,
    region: String,
    history: Arc<History>,
    map: fn(Option<&K>, &K, &str) -> Vec<EventRow>,
    on_delete: fn(&K, &str) -> Vec<EventRow>,
) where
    K: kube::Resource<Scope = kube::core::ClusterResourceScope, DynamicType = ()>
        + Clone
        + std::fmt::Debug
        + serde::de::DeserializeOwned
        + Send
        + Sync
        + 'static,
{
    let api = kube::Api::<K>::all(client);
    let kind = K::kind(&()).to_string();
    let mut backoff = MIN_BACKOFF;
    let mut failures: u64 = 0;
    loop {
        let mut prev: HashMap<String, K> = HashMap::new();
        let mut stream =
            kube::runtime::watcher(api.clone(), kube::runtime::watcher::Config::default()).boxed();
        while let Some(ev) = stream.next().await {
            let rows = match ev {
                Ok(kube::runtime::watcher::Event::Apply(o))
                | Ok(kube::runtime::watcher::Event::InitApply(o)) => {
                    let Some(uid) = o.meta().uid.clone() else {
                        continue;
                    };
                    let rows = map(prev.get(&uid), &o, &region);
                    prev.insert(uid, o);
                    rows
                }
                // The previous state goes with it, so an object recreated under the same name (a
                // new uid) reads as a fresh `created` rather than a phantom transition.
                Ok(kube::runtime::watcher::Event::Delete(o)) => {
                    if let Some(uid) = o.meta().uid.as_ref() {
                        prev.remove(uid);
                    }
                    on_delete(&o, &region)
                }
                Ok(_) => continue,
                Err(e) => {
                    // Never fatal: the loop re-establishes the watch, and the ids make the re-list
                    // idempotent. Loud on the first failure of a run and then rarely, so a wedged
                    // watch is visible without every reconnect blip becoming its own noise source.
                    failures += 1;
                    if failures == 1 || failures.is_multiple_of(LOUD_EVERY) {
                        tracing::warn!(%kind, %region, attempt = failures, error = %e, "history.watch.restarted");
                    } else {
                        tracing::debug!(%kind, %region, error = %e, "history.watch.restarted");
                    }
                    break;
                }
            };
            // An event at all means the watch is healthy: a later outage is a fresh run and gets
            // its own warn and its own short first retry.
            failures = 0;
            backoff = MIN_BACKOFF;
            if rows.is_empty() {
                continue;
            }
            if let Err(e) = write_events(&history, &rows).await {
                tracing::warn!(%region, count = rows.len(), error = %e, "history.write.failed");
            }
        }
        // A watcher that ended restarts after a growing pause rather than spinning on a cluster
        // that is not answering.
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// The central cluster's one watch. `Region` is the only kind that lives there — every workspace,
/// environment, snapshot, volume and request belongs to a region cluster, so running the others
/// here would be watches against a cluster with none of those CRDs, each failing and retrying
/// forever.
pub async fn watch_central(client: kube::Client, history: Arc<History>) {
    watch_kind::<crd::Region>(
        client,
        CENTRAL.to_string(),
        history,
        |p, n, _| region_events(p, n),
        no_delete,
    )
    .await
}

/// Every watch for one region cluster — and `watch_central` alone for the central one, so the
/// caller keeps one entry point and cannot spawn a region's watches against a cluster that holds
/// none of those CRDs.
pub async fn watch_region(client: kube::Client, region: String, history: Arc<History>) {
    if region == CENTRAL {
        return watch_central(client, history).await;
    }
    let tasks = vec![
        tokio::spawn(watch_kind::<crd::Workspace>(
            client.clone(),
            region.clone(),
            history.clone(),
            workspace_events,
            workspace_deleted,
        )),
        tokio::spawn(watch_kind::<crd::Environment>(
            client.clone(),
            region.clone(),
            history.clone(),
            environment_events,
            environment_deleted,
        )),
        tokio::spawn(watch_kind::<crd::Snapshot>(
            client.clone(),
            region.clone(),
            history.clone(),
            snapshot_events,
            snapshot_deleted,
        )),
        tokio::spawn(watch_kind::<crd::Volume>(
            client.clone(),
            region.clone(),
            history.clone(),
            volume_events,
            no_delete,
        )),
        tokio::spawn(watch_kind::<crd::QuotaRequest>(
            client.clone(),
            region.clone(),
            history.clone(),
            quota_request_events,
            no_delete,
        )),
        tokio::spawn(watch_kind::<crd::Request>(
            client.clone(),
            region.clone(),
            history.clone(),
            request_events,
            no_delete,
        )),
        tokio::spawn(watch_kind::<Node>(
            client,
            region,
            history,
            node_events,
            no_delete,
        )),
    ];
    // Each task loops forever; awaiting them all only ends if the process does.
    for t in tasks {
        let _ = t.await;
    }
}
