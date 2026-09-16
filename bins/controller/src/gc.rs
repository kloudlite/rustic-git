//! The removal GC: the ONE thing that deletes a departed team member's Bench, team Workspaces and
//! SpaceEnvironment choice.
//!
//! The api's keys beat (`api::membership`) only MARKS a removed pair with
//! `kloudlite.io/delete-after` (removed-at + 7 days, or now for an admin's delete-now); this pass
//! deletes what is due. Split so the irreversible step runs only in the process holding the
//! region's lease, and a re-add undoes a removal by clearing an annotation instead of racing a
//! delete. Every ~60 s, leader only:
//!
//! - list the three kinds; ANY listing failure skips the whole pass (keep-biased — a partial view
//!   is never a reason to act);
//! - an object is due only if `delete-after` was applied by the api's `kloudlite-membership`
//!   manager (the admission policy `kloudlite-removal-stamps-are-the-apis` is the real fence) and
//!   parses to a time already past; a Bench also waits for the agent's `kloudlite.io/bench-folder`
//!   finalizer, without which its on-disk folder would be stranded;
//! - the api's keys beat Lease (`kloudlite-keys-beat`) older than two beats, missing or unreadable
//!   holds every delete (`gc.held_beat_stale`): the slack assumes that beat is clearing re-adds;
//! - the regional `ClusterSettings.memberRemovalDeletes` off (the default) logs `gc.would_delete`
//!   and deletes nothing; on, each due object is deleted under a uid + resourceVersion
//!   precondition, re-checking the lease first, and a 404 or 409 is a skip for the next tick.
//!
//! Never lists or touches a Snapshot, Volume, Environment, repo or image: a workspace's pushed
//! snapshots outlive it through its Volume (`WORKTREE_FINALIZER`).

use crate::Ctx;
use k8s_openapi::api::coordination::v1::Lease;
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kloudlite_workspaces::api::keys::{KEYS_BEAT_LEASE, KEYS_BEAT_NAMESPACE, KEYS_RESYNC_SECS};
use k8s_openapi::jiff::Timestamp;
use kloudlite_workspaces::api::membership::{system_annotation, DELETE_AFTER, GC_DELETE_SLACK_SECS, GC_TICK_SECS};
use kloudlite_workspaces::{crd, k8s};
use kube::api::{Api, DeleteParams, Preconditions};
use kube::{Resource, ResourceExt};
use std::sync::Arc;
use std::time::Duration;

pub const TICK: Duration = Duration::from_secs(GC_TICK_SECS);
/// `GC_DELETE_SLACK_SECS`; its reason lives beside it.
pub const DELETE_SLACK_SECS: i64 = GC_DELETE_SLACK_SECS as i64;

pub async fn run(ctx: Arc<Ctx>) {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if ctx.leading() {
            pass(&ctx, Timestamp::now().as_second()).await;
        }
    }
}

struct Due {
    kind: &'static str,
    name: String,
    uid: Option<String>,
    rv: Option<String>,
    owner: String,
    team: String,
    due: String,
    env: Option<String>,
}

fn due<K: Resource>(x: &K, kind: &'static str, owner: &str, team: &str, now: i64) -> Option<Due> {
    let m = x.meta();
    if m.deletion_timestamp.is_some() {
        return None;
    }
    let at = system_annotation(m, DELETE_AFTER)?;
    // Unparsable is not due: only ever pushes a delete later.
    (at.parse::<Timestamp>().ok()?.as_second() + DELETE_SLACK_SECS <= now).then(|| Due {
        kind,
        name: m.name.clone().unwrap_or_default(),
        uid: m.uid.clone(),
        rv: m.resource_version.clone(),
        owner: owner.to_string(),
        team: team.to_string(),
        due: at,
        env: None,
    })
}

async fn list<K>(c: &kube::Client) -> Result<Vec<K>, kube::Error>
where
    K: Resource<Scope = kube::core::ClusterResourceScope, DynamicType = ()> + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    match Api::<K>::all(c.clone()).list(&Default::default()).await {
        Ok(l) => Ok(l.items),
        // The CRD not applied: genuinely no objects of that kind.
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Whether the api's keys beat renewed `KEYS_BEAT_LEASE` within two beats. Missing, unreadable or
/// unstamped all answer no: the slack only protects a re-add while that beat is clearing marks.
pub fn beat_fresh(lease: Option<&Lease>, now: i64) -> bool {
    let max = 2 * KEYS_RESYNC_SECS as i64;
    lease.and_then(|l| l.spec.as_ref()?.renew_time.as_ref()).is_some_and(|t| now - t.0.as_second() <= max)
}

async fn beat_alive(ctx: &Ctx, now: i64) -> bool {
    let api: Api<Lease> = Api::namespaced(ctx.client.clone(), KEYS_BEAT_NAMESPACE);
    match api.get_opt(KEYS_BEAT_LEASE).await {
        Ok(l) => beat_fresh(l.as_ref(), now),
        Err(error) => {
            tracing::warn!(%error, "gc.beat_lease.unreadable");
            false
        }
    }
}

pub async fn pass(ctx: &Ctx, now: i64) {
    let c = &ctx.client;
    let listed = async { Ok::<_, kube::Error>((list::<crd::Bench>(c).await?, list::<crd::Workspace>(c).await?, list::<crd::SpaceEnvironment>(c).await?)) };
    let (benches, workspaces, spaces) = match listed.await {
        Ok(l) => l,
        Err(error) => return tracing::warn!(%error, "gc.listing.failed"),
    };
    let mut todo = Vec::new();
    for b in &benches {
        let Some(d) = due(b, "Bench", &b.spec.owner, &b.spec.team, now) else { continue };
        if b.finalizers().iter().any(|f| f == crd::BENCH_FOLDER_FINALIZER) {
            todo.push(d);
        } else {
            tracing::warn!(name = %d.name, owner = %d.owner, team = %d.team, "gc.bench_waits_finalizer");
        }
    }
    todo.extend(workspaces.iter().filter_map(|w| Some(Due { env: crd::attached_environment(w), ..due(w, "Workspace", &w.spec.owner, &w.spec.team, now)? })));
    todo.extend(spaces.iter().filter_map(|s| due(s, "SpaceEnvironment", &s.spec.owner, &s.spec.team, now)));

    let on = ctx.settings.load().member_removal_deletes;
    if on && !todo.is_empty() && !beat_alive(ctx, now).await {
        return tracing::warn!(due = todo.len(), "gc.held_beat_stale");
    }
    let due_total = todo.len();
    for (done, d) in todo.into_iter().enumerate() {
        if !on {
            tracing::info!(kind = d.kind, name = %d.name, owner = %d.owner, team = %d.team, due = %d.due, "gc.would_delete");
            continue;
        }
        if !crate::space::may_write(ctx).await {
            // Say so: a lease lost halfway through a pass and a pass that found nothing to do look
            // identical in the log otherwise, and the difference is whether anything was skipped.
            return tracing::info!(remaining = due_total - done, "gc.lease_lost");
        }
        let dp = DeleteParams { preconditions: Some(Preconditions { uid: d.uid.clone(), resource_version: d.rv.clone() }), ..Default::default() };
        let res = match d.kind {
            "Bench" => Api::<crd::Bench>::all(c.clone()).delete(&d.name, &dp).await.map(|_| ()),
            "Workspace" => Api::<crd::Workspace>::all(c.clone()).delete(&d.name, &dp).await.map(|_| ()),
            _ => Api::<crd::SpaceEnvironment>::all(c.clone()).delete(&d.name, &dp).await.map(|_| ()),
        };
        match res {
            Ok(()) => {
                tracing::info!(kind = d.kind, name = %d.name, owner = %d.owner, team = %d.team, due = %d.due, "gc.deleted");
                // The env-side attach policy lives in another namespace, so no ownerReference
                // collects it; `/v1`'s own workspace delete drops it the same way.
                if let Some(env) = &d.env {
                    let api: Api<NetworkPolicy> = Api::namespaced(c.clone(), &crd::env_namespace(env));
                    match api.delete(&k8s::attach_policy_name(&d.name), &Default::default()).await {
                        Ok(_) => {}
                        Err(kube::Error::Api(e)) if e.code == 404 => {}
                        Err(error) => tracing::warn!(name = %d.name, environment = %env, %error, "gc.attach_policy.failed"),
                    }
                }
            }
            // Gone already, or changed since the list (a re-add's clear, a restart): next tick decides.
            Err(kube::Error::Api(e)) if e.code == 404 || e.code == 409 => {
                tracing::info!(kind = d.kind, name = %d.name, code = e.code, "gc.skipped");
            }
            Err(error) => tracing::warn!(kind = d.kind, name = %d.name, %error, "gc.delete.failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kloudlite_workspaces::api::membership::MEMBERSHIP_FIELD_MANAGER;
    use kloudlite_workspaces::kube_test::{self, get, Recorder, Route};
    use kloudlite_workspaces::settings::AgentSettings;
    use serde_json::{json, Value};

    const API: &str = "/apis/kloudlite.io/v1alpha1";
    const LEASE: &str = "/apis/coordination.k8s.io/v1/namespaces/kube-system/leases/kloudlite-controller";
    const PAST: &str = "2020-01-08T00:00:00Z";
    const FUTURE: &str = "2999-01-01T00:00:00Z";

    fn meta(name: &str, after: Option<&str>, system: bool) -> Value {
        let mut m = json!({"name": name, "uid": format!("uid-{name}"), "resourceVersion": "7"});
        if let Some(a) = after {
            m["annotations"] = json!({DELETE_AFTER: a});
            if system {
                m["managedFields"] = json!([{"manager": MEMBERSHIP_FIELD_MANAGER, "operation": "Apply", "apiVersion": "kloudlite.io/v1alpha1", "fieldsType": "FieldsV1",
                    "fieldsV1": {"f:metadata": {"f:annotations": {format!("f:{DELETE_AFTER}"): {}}}}}]);
            }
        }
        m
    }

    fn bench(name: &str, after: Option<&str>, finalizer: bool) -> Value {
        let mut m = meta(name, after, true);
        if finalizer {
            m["finalizers"] = json!([crd::BENCH_FOLDER_FINALIZER]);
        }
        json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Bench", "metadata": m,
               "spec": {"owner": "bob", "team": "acme", "image": "i", "desiredState": "stopped", "access": "paused",
                        "resources": {"cpuRequest": "1", "cpuLimit": "1", "memoryRequest": "1Gi", "memoryLimit": "1Gi"}}})
    }

    fn ws(name: &str, after: Option<&str>, system: bool) -> Value {
        json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace", "metadata": meta(name, after, system),
               "spec": {"owner": "bob", "team": "acme", "name": name, "region": "r", "image": "i", "desiredState": "stopped", "attachedEnvironment": "env-7"}})
    }

    fn space(after: Option<&str>) -> Value {
        let mut v = serde_json::to_value(crd::space_environment("bob", "acme", "env-1")).unwrap();
        v["metadata"] = meta(&crd::space_name("bob", "acme"), after, true);
        v
    }

    fn list(kind: &str, plural: &str, items: Vec<Value>) -> Route {
        get(format!("{API}/{plural}"), json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": format!("{kind}List"), "metadata": {}, "items": items}))
    }

    fn ok_delete(path: String) -> Route {
        Route { method: "DELETE", path, status: 200, body: json!({"kind": "Status", "apiVersion": "v1", "status": "Success", "code": 200}) }
    }

    const BEAT: &str = "/apis/coordination.k8s.io/v1/namespaces/kube-system/leases/kloudlite-keys-beat";

    fn beat(renewed: i64) -> Route {
        get(BEAT, serde_json::to_value(kloudlite_workspaces::api::keys::beat_lease(Timestamp::from_second(renewed).unwrap())).unwrap())
    }

    fn cluster() -> Vec<Route> {
        vec![
            list("Bench", "benches", vec![bench("b-due", Some(PAST), true), bench("b-later", Some(FUTURE), true), bench("b-nofin", Some(PAST), false), bench("b-plain", None, true)]),
            list("Workspace", "workspaces", vec![ws("w-due", Some(PAST), true), ws("w-foreign", Some(PAST), false)]),
            list("SpaceEnvironment", "spaceenvironments", vec![space(Some(PAST))]),
            get(LEASE, json!({"apiVersion": "coordination.k8s.io/v1", "kind": "Lease", "metadata": {"name": "kloudlite-controller", "namespace": "kube-system", "resourceVersion": "1"},
                              "spec": {"holderIdentity": "ctl-test", "leaseTransitions": 4}})),
            beat(Timestamp::now().as_second()),
            ok_delete(format!("{API}/benches/b-due")),
            ok_delete(format!("{API}/workspaces/w-due")),
            ok_delete(format!("{API}/spaceenvironments/{}", crd::space_name("bob", "acme"))),
        ]
    }

    async fn run_pass(routes: Vec<Route>, deletes_on: bool, leading: bool) -> Recorder {
        let (client, rec) = kube_test::mock_client(routes);
        let ctx = Ctx::for_test_with(client);
        ctx.settings.store(AgentSettings { member_removal_deletes: deletes_on, ..AgentSettings::from_env() });
        if leading {
            ctx.promote(4);
        }
        pass(&ctx, Timestamp::now().as_second()).await;
        rec
    }

    fn deletes(rec: &Recorder) -> Vec<String> {
        rec.calls().into_iter().filter(|c| c.starts_with("DELETE ")).collect()
    }

    #[tokio::test]
    async fn only_due_system_marked_objects_are_deleted_with_preconditions() {
        let rec = run_pass(cluster(), true, true).await;
        let policy = format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/{}", crd::env_namespace("env-7"), k8s::attach_policy_name("w-due"));
        assert_eq!(
            deletes(&rec),
            vec![
                format!("DELETE {API}/benches/b-due"),
                format!("DELETE {API}/workspaces/w-due"),
                format!("DELETE {policy}"),
                format!("DELETE {API}/spaceenvironments/{}", crd::space_name("bob", "acme")),
            ],
            "not due, no finalizer, unmarked and foreign-marked objects are kept"
        );
        let pre = &rec.sent("DELETE", &format!("{API}/workspaces/w-due"))[0]["preconditions"];
        assert_eq!((pre["uid"].as_str(), pre["resourceVersion"].as_str()), (Some("uid-w-due"), Some("7")));
    }

    #[tokio::test]
    async fn only_the_three_kinds_are_ever_listed_or_touched() {
        let rec = run_pass(cluster(), true, true).await;
        for c in rec.calls() {
            assert!(!["/snapshots", "/volumes", "/environments"].iter().any(|k| c.contains(k)), "{c}");
        }
    }

    #[tokio::test]
    async fn with_the_switch_off_nothing_is_deleted() {
        let rec = run_pass(cluster(), false, true).await;
        assert!(deletes(&rec).is_empty(), "{:?}", rec.calls());
    }

    #[tokio::test]
    async fn a_listing_failure_skips_the_whole_pass() {
        let mut routes = cluster();
        routes[1] = Route { method: "GET", path: format!("{API}/workspaces"), status: 500, body: json!({}) };
        let rec = run_pass(routes, true, true).await;
        assert!(deletes(&rec).is_empty(), "the due bench is kept too: {:?}", rec.calls());
    }

    #[tokio::test]
    async fn a_follower_deletes_nothing() {
        let rec = run_pass(cluster(), true, false).await;
        assert!(deletes(&rec).is_empty(), "{:?}", rec.calls());
    }

    #[tokio::test]
    async fn a_mark_due_but_within_the_slack_waits_and_past_it_goes() {
        let now = Timestamp::now().as_second();
        let at = |secs: i64| Timestamp::from_second(now - secs).unwrap().to_string();
        let routes = |secs: i64| {
            let mut r = cluster();
            r[0] = list("Bench", "benches", vec![]);
            r[1] = list("Workspace", "workspaces", vec![ws("w-due", Some(&at(secs)), true)]);
            r[2] = list("SpaceEnvironment", "spaceenvironments", vec![]);
            r
        };
        let rec = run_pass(routes(DELETE_SLACK_SECS - 30), true, true).await;
        assert!(deletes(&rec).is_empty(), "a re-add's beat may still clear it: {:?}", rec.calls());
        let rec = run_pass(routes(DELETE_SLACK_SECS + 30), true, true).await;
        assert!(deletes(&rec).contains(&format!("DELETE {API}/workspaces/w-due")), "{:?}", rec.calls());
    }

    #[tokio::test]
    async fn a_stale_or_missing_keys_beat_holds_every_delete() {
        let now = Timestamp::now().as_second();
        let mut routes = cluster();
        routes[4] = beat(now - 2 * KEYS_RESYNC_SECS as i64 - 30);
        let rec = run_pass(routes, true, true).await;
        assert!(deletes(&rec).is_empty(), "stale: {:?}", rec.calls());
        let mut routes = cluster();
        routes.remove(4);
        let rec = run_pass(routes, true, true).await;
        assert!(deletes(&rec).is_empty(), "missing: {:?}", rec.calls());
    }

    #[test]
    fn a_beat_within_two_resyncs_is_fresh() {
        let lease = |at: i64| kloudlite_workspaces::api::keys::beat_lease(Timestamp::from_second(at).unwrap());
        let (now, max) = (10_000, 2 * KEYS_RESYNC_SECS as i64);
        assert!(beat_fresh(Some(&lease(now - max)), now));
        assert!(!beat_fresh(Some(&lease(now - max - 1)), now));
        assert!(!beat_fresh(None, now));
    }

    #[tokio::test]
    async fn a_conflict_is_skipped_and_the_rest_still_run() {
        let mut routes = cluster();
        routes[5] = kube_test::conflict("DELETE", format!("{API}/benches/b-due"));
        let rec = run_pass(routes, true, true).await;
        assert_eq!(deletes(&rec).len(), 4, "{:?}", rec.calls());
    }
}
