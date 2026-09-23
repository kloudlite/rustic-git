//! The membership beat's tests: one `Directory` fake, one canned API server, and the routes
//! that read them. Kept whole because every case shares those fixtures (`k8s/tests.rs`'s rule).

use super::*;
use crate::api::{OwnerMaterial, TeamRole};
use crate::kube_test::{get, mock_client, patch, Recorder};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const API: &str = "/apis/kloudlite.io/v1alpha1";

/// `down` is unreadable, `gone` does not exist, `paula` is paused, `alice` is in `acme`.
#[derive(Default)]
struct Fake {
    asked: AtomicUsize,
}
#[async_trait::async_trait]
impl Directory for Fake {
    async fn teams_for(&self, _u: &str) -> Vec<String> {
        Vec::new()
    }
    async fn is_live(&self, _j: &str) -> bool {
        false
    }
    async fn for_owner(&self, _o: &str) -> Option<OwnerMaterial> {
        None
    }
    async fn authorized_keys_for_owner(&self, _o: &str) -> Option<String> {
        None
    }
    async fn owners_of(&self, _e: &str) -> Vec<String> {
        Vec::new()
    }
    async fn team_role(&self, u: &str, t: &str) -> Option<TeamRole> {
        match (u, t) {
            ("ann", "acme") => Some(TeamRole::Admin),
            ("mem", "acme") => Some(TeamRole::Member),
            _ => None,
        }
    }
    async fn is_team(&self, _s: &str) -> bool {
        false
    }
    async fn ensure_user(&self, _e: &str, _n: &str, _u: &str) -> Result<(), String> {
        Err("no".into())
    }
    async fn add_superadmin(&self, _e: &str, _b: &str) -> Result<(), String> {
        Err("no".into())
    }
    async fn owner_kind(&self, slug: &str) -> Result<crate::api::OwnerKind, String> {
        use crate::api::OwnerKind;
        match slug {
            "blind" => Err("directory unreachable".into()),
            s if s.contains('@') => Ok(OwnerKind::Gone),
            _ => Ok(OwnerKind::Person),
        }
    }

    async fn is_superadmin(&self, u: &str) -> Result<bool, String> {
        match u {
            "root" => Ok(true),
            "flaky" => Err("directory unreachable".into()),
            _ => Ok(false),
        }
    }
    async fn membership(&self, team: &str, user: &str) -> Result<Judged, String> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        match (team, user) {
            ("down", _) => Err("directory unreachable".into()),
            // What the real directory answers for an argument that is no handle (an email).
            (_, u) if u.contains('@') => Err(format!("no person {u}")),
            (_, "blind") => Err("directory unreachable".into()),
            ("gone", _) => Ok(Judged::TeamGone),
            (_, "paula") => Ok(Judged::Member(MemberState::Paused)),
            ("acme", "alice") => Ok(Judged::Member(MemberState::Active)),
            // Removed at the first ask, back by the second: the re-judge must see it.
            (_, "rex") if self.asked.load(Ordering::SeqCst) > 1 => Ok(Judged::Member(MemberState::Active)),
            _ => Ok(Judged::NotMember),
        }
    }
}

/// A bench: a Workspace with `spec.bench`, named `bench_id`, like every other object of the pair.
fn bench(owner: &str, team: &str, access: &str, stamp: Option<&str>) -> serde_json::Value {
    let mut b = json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": crd::bench_id(owner, team)},
        "spec": {"owner": owner, "team": team, "name": "bench", "region": "r", "image": "i",
                 "desiredState": "running", "access": access, "bench": {"model": "m"}}
    });
    if let Some(t) = stamp {
        b["metadata"]["annotations"] = json!({REMOVED_AT: t});
        b["metadata"]["managedFields"] = owned(&[REMOVED_AT]);
    }
    b
}

fn owned(keys: &[&str]) -> serde_json::Value {
    let ann: serde_json::Map<_, _> = keys.iter().map(|k| (format!("f:{k}"), json!({}))).collect();
    json!([{"manager": MEMBERSHIP_FIELD_MANAGER, "operation": "Apply", "apiVersion": "kloudlite.io/v1alpha1", "fieldsType": "FieldsV1", "fieldsV1": {"f:metadata": {"f:annotations": ann}}}])
}

/// Every kind of workspace, bench included, comes off the ONE list now.
fn benches(items: Vec<serde_json::Value>) -> crate::kube_test::Route {
    list_of("Workspace", "workspaces", items)
}

fn path(owner: &str, team: &str) -> String {
    format!("{API}/workspaces/{}", crd::bench_id(owner, team))
}

fn setup(mut routes: Vec<crate::kube_test::Route>, patched: &[(&str, &str)]) -> (ApiState, Recorder, Arc<Fake>) {
    routes.extend(patched.iter().map(|(o, t)| patch(path(o, t), bench(o, t, "full", None))));
    let (client, rec) = mock_client(routes);
    let dir = Arc::new(Fake::default());
    let jwt = Arc::new(kloudlite_core::jwt::Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    (ApiState::new(jwt).with_kube(client).with_directory(dir.clone()), rec, dir)
}

fn writes(rec: &Recorder) -> Vec<String> {
    rec.calls().into_iter().filter(|c| !c.starts_with("GET ")).collect()
}

const OLD: &str = "2020-01-01T00:00:00Z";

fn with_meta(mut v: serde_json::Value, name: &str, stamp: Option<&str>, finalizer: bool) -> serde_json::Value {
    v["metadata"]["name"] = json!(name);
    v["metadata"]["uid"] = json!(format!("uid-{name}"));
    v["metadata"]["resourceVersion"] = json!("7");
    if let Some(t) = stamp {
        v["metadata"]["annotations"] = json!({REMOVED_AT: t});
        v["metadata"]["managedFields"] = owned(&[REMOVED_AT]);
    }
    if finalizer {
        v["metadata"]["finalizers"] = json!(["kloudlite.io/worktree"]);
    }
    v
}

fn ws(name: &str, owner: &str, team: &str) -> serde_json::Value {
    let spec = json!({"owner": owner, "team": team, "name": name, "region": "r", "image": "i", "desiredState": "running"});
    with_meta(json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace", "metadata": {}, "spec": spec}), name, None, false)
}

fn space(owner: &str, team: &str) -> serde_json::Value {
    let v = serde_json::to_value(crd::space_environment(owner, team, "env-1")).unwrap();
    with_meta(v, &crd::space_name(owner, team), None, false)
}

fn list_of(kind: &str, plural: &str, items: Vec<serde_json::Value>) -> crate::kube_test::Route {
    get(format!("{API}/{plural}"), json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": format!("{kind}List"), "metadata": {}, "items": items}))
}

fn del(path: String, status: u16) -> crate::kube_test::Route {
    let body = if status == 200 { json!({"kind": "Status", "apiVersion": "v1", "status": "Success", "code": 200}) } else { serde_json::to_value(kube::core::Status::failure("x", "Conflict").with_code(status)).unwrap() };
    crate::kube_test::Route { method: "DELETE", path, status, body }
}

/// A removed `(owner, acme)` past the grace: a bench, two workspaces, a space choice.
fn removed(owner: &str) -> Vec<crate::kube_test::Route> {
    let b = with_meta(bench(owner, "acme", "paused", None), &crd::bench_id(owner, "acme"), Some(OLD), false);
    vec![
        list_of("Workspace", "workspaces", vec![b, ws("w1", owner, "acme"), ws("w2", owner, "acme"), ws("w9", owner, "")]),
        list_of("SpaceEnvironment", "spaceenvironments", vec![space(owner, "acme")]),
    ]
}

fn deletes(rec: &Recorder) -> Vec<String> {
    rec.calls().into_iter().filter(|c| c.starts_with("DELETE ")).collect()
}

const BEAT: &str = "/apis/coordination.k8s.io/v1/namespaces/kube-system/leases/kloudlite-keys-beat";

fn beat_patches(rec: &Recorder) -> usize {
    rec.calls().iter().filter(|c| *c == &format!("PATCH {BEAT}")).count()
}

/// The heartbeat means "re-adds are being cleared": renewed after a whole judged pass, never
/// after a pass that could not read the directory, the listing, or any one pair.
#[tokio::test]
async fn the_keys_beat_lease_is_renewed_only_after_a_fully_judged_pass() {
    let lease = || patch(BEAT, json!({"apiVersion": "coordination.k8s.io/v1", "kind": "Lease", "metadata": {"name": "kloudlite-keys-beat"}}));
    let (s, rec, _) = setup(vec![benches(vec![bench("alice", "acme", "full", None)]), lease()], &[]);
    crate::api::keys::membership_beat(&s).await;
    assert_eq!(beat_patches(&rec), 1, "success: {:?}", rec.calls());

    let (s, rec, _) = setup(vec![benches(vec![bench("alice", "down", "full", None)]), lease()], &[]);
    crate::api::keys::membership_beat(&s).await;
    assert_eq!(beat_patches(&rec), 0, "every judge erroring: {:?}", rec.calls());

    let failing = crate::kube_test::Route { method: "GET", path: format!("{API}/workspaces"), status: 500, body: json!({}) };
    let (s, rec, _) = setup(vec![failing, lease()], &[]);
    crate::api::keys::membership_beat(&s).await;
    assert_eq!(beat_patches(&rec), 0, "listing failed: {:?}", rec.calls());

    let (client, rec) = mock_client(vec![benches(vec![]), lease()]);
    let jwt = Arc::new(kloudlite_core::jwt::Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    crate::api::keys::membership_beat(&ApiState::new(jwt).with_kube(client)).await;
    assert_eq!(beat_patches(&rec), 0, "no directory: {:?}", rec.calls());
}

#[tokio::test]
async fn a_mixed_case_team_is_judged_by_its_slug() {
    let (s, rec, dir) = setup(vec![benches(vec![bench("alice", " Acme", "full", None)])], &[]);
    let _ = reconcile(&s).await;
    assert_eq!(dir.asked.load(Ordering::SeqCst), 1);
    assert!(writes(&rec).is_empty(), "a live member of acme is kept: {:?}", writes(&rec));
}

#[tokio::test]
async fn a_stamp_this_system_did_not_write_restarts_the_grace() {
    let mut routes = removed("bob");
    let mut b = bench("bob", "acme", "paused", None);
    b["metadata"]["annotations"] = json!({REMOVED_AT: OLD, DELETE_NOW: "yes"});
    routes[0] = benches(vec![b]);
    let (s, rec, _) = setup(routes, &[("bob", "acme")]);
    let _ = reconcile(&s).await;
    assert!(deletes(&rec).is_empty(), "{:?}", deletes(&rec));
    let sent = rec.sent("PATCH", &path("bob", "acme"));
    assert_ne!(sent[0]["metadata"]["annotations"][REMOVED_AT], json!(OLD), "a fresh stamp");
}

#[tokio::test]
async fn a_workspace_only_pair_is_stamped_on_its_workspaces() {
    let routes = vec![list_of("Workspace", "workspaces", vec![ws("w1", "bob", "acme")]), patch(format!("{API}/workspaces/w1"), ws("w1", "bob", "acme"))];
    let (s, rec, _) = setup(routes, &[]);
    let _ = reconcile(&s).await;
    let sent = rec.sent("PATCH", &format!("{API}/workspaces/w1"));
    assert_eq!(sent.len(), 2, "the stamp, then the pause: {sent:?}");
    assert_eq!(sent[0]["kind"], "Workspace");
    assert!(sent[0]["metadata"]["annotations"][REMOVED_AT].is_string());
    assert_eq!(sent[1], json!({"spec": {"access": "paused"}}), "access is on every workspace now");
}

#[tokio::test]
async fn reconciling_a_pair_forgets_its_cached_verdict() {
    let (s, _, _) = setup(vec![], &[]);
    let put = |o: &str| s.member_verdicts.lock().unwrap().insert(("acme".into(), o.into()), (std::time::Instant::now(), Judged::NotMember));
    put("bob");
    put("alice");
    reconcile_pair(&s, "Bob", "ACME").await;
    let left: Vec<_> = s.member_verdicts.lock().unwrap().keys().cloned().collect();
    assert_eq!(left, vec![("acme".to_string(), "alice".to_string())]);
}

#[tokio::test]
async fn every_object_of_the_pair_is_stamped_so_a_deleted_bench_keeps_the_clock() {
    let routes = vec![
        benches(vec![bench("bob", "acme", "full", None), ws("w1", "bob", "acme")]),
        patch(format!("{API}/workspaces/w1"), ws("w1", "bob", "acme")),
    ];
    let (s, rec, _) = setup(routes, &[("bob", "acme")]);
    let _ = reconcile(&s).await;
    assert!(rec.sent("PATCH", &path("bob", "acme"))[0]["metadata"]["annotations"][REMOVED_AT].is_string());
    let w = rec.sent("PATCH", &format!("{API}/workspaces/w1"));
    assert_eq!(w.len(), 2, "the stamp, then the pause: {w:?}");
    assert!(w[0]["metadata"]["annotations"][REMOVED_AT].is_string());
    // The STAMP is server-side applied under the membership manager; the pause beside it is a
    // plain merge patch, so that manager never owns `spec.access`.
    assert_eq!(rec.requests().iter().filter(|r| r.contains("/workspaces/w1") && r.contains("fieldManager=kloudlite-membership")).count(), 1, "{:?}", rec.requests());
}

#[tokio::test]
async fn a_bench_set_back_to_full_during_the_grace_is_paused_again() {
    let fresh = chrono::Utc::now().to_rfc3339();
    let (s, rec, _) = setup(vec![benches(vec![bench("bob", "acme", "full", Some(&fresh))])], &[("bob", "acme")]);
    let _ = reconcile(&s).await;
    assert_eq!(rec.sent("PATCH", &path("bob", "acme")), vec![json!({"spec": {"access": "paused"}})]);
}

#[tokio::test]
async fn a_due_pair_is_marked_on_every_object_and_nothing_is_deleted() {
    let (s, rec, _) = setup(removed("bob"), &[]);
    let _ = reconcile(&s).await;
    assert!(deletes(&rec).is_empty(), "{:?}", deletes(&rec));
    let paths = [path("bob", "acme"), format!("{API}/workspaces/w1"), format!("{API}/workspaces/w2"), format!("{API}/spaceenvironments/{}", crd::space_name("bob", "acme"))];
    for p in &paths {
        let sent = rec.sent("PATCH", p);
        assert_eq!(sent.len(), 1, "{p}: {:?}", rec.calls());
        assert_eq!(sent[0]["metadata"]["annotations"], json!({REMOVED_AT: "2020-01-01T00:00:00+00:00", DELETE_AFTER: "2020-01-08T00:00:00+00:00"}), "{p}");
    }
    assert!(!writes(&rec).iter().any(|c| c.contains("/workspaces/w9")), "a personal workspace is never marked");
}

#[tokio::test]
async fn a_due_pair_already_marked_writes_nothing() {
    let mut b = with_meta(bench("bob", "acme", "paused", None), &crd::bench_id("bob", "acme"), None, false);
    b["metadata"]["annotations"] = json!({REMOVED_AT: OLD, DELETE_AFTER: "2020-01-08T00:00:00Z"});
    b["metadata"]["managedFields"] = owned(&[REMOVED_AT, DELETE_AFTER]);
    let (s, rec, _) = setup(vec![benches(vec![b])], &[]);
    let _ = reconcile(&s).await;
    assert!(writes(&rec).is_empty(), "{:?}", writes(&rec));
}


#[test]
fn decide_covers_every_row() {
    use Judged::*;
    use MemberState::*;
    let g = MEMBER_REMOVAL_GRACE.as_secs() as i64;
    let now = 10 * g;
    let err: Result<Judged, String> = Err("x".into());
    assert_eq!(decide(&err, Some(0), true, now, false), Verdict::Keep);
    assert_eq!(decide(&Ok(Member(Active)), Some(now), false, now, false), Verdict::Clear);
    assert_eq!(decide(&Ok(Member(Active)), None, true, now, false), Verdict::Clear);
    assert_eq!(decide(&Ok(Member(Active)), None, false, now, true), Verdict::Unpause);
    assert_eq!(decide(&Ok(Member(Active)), None, false, now, false), Verdict::Keep);
    assert_eq!(decide(&Ok(Member(Paused)), None, false, now, false), Verdict::Pause);
    assert_eq!(decide(&Ok(Member(Paused)), None, false, now, true), Verdict::Keep);
    assert_eq!(decide(&Ok(Member(Paused)), Some(0), false, now, true), Verdict::Clear);
    for j in [NotMember, TeamGone] {
        assert_eq!(decide(&Ok(j), None, false, now, false), Verdict::Stamp);
        assert_eq!(decide(&Ok(j), Some(now - g + 1), false, now, true), Verdict::Keep);
        assert_eq!(decide(&Ok(j), Some(now - g), false, now, true), Verdict::Due);
        assert_eq!(decide(&Ok(j), Some(now), true, now, true), Verdict::Due);
        assert_eq!(decide(&Ok(j), None, true, now, false), Verdict::Due);
    }
}

#[tokio::test]
async fn a_directory_error_changes_nothing() {
    let (s, rec, _) = setup(vec![benches(vec![bench("bob", "down", "full", Some("2020-01-01T00:00:00Z"))])], &[("bob", "down")]);
    let _ = reconcile(&s).await;
    assert!(writes(&rec).is_empty(), "{:?}", writes(&rec));
}

#[tokio::test]
async fn a_personal_pair_is_never_a_candidate() {
    let (s, rec, dir) = setup(vec![benches(vec![bench("alice", "Alice", "full", None), bench("carol", "", "full", None)])], &[]);
    let _ = reconcile(&s).await;
    assert_eq!(dir.asked.load(Ordering::SeqCst), 0);
    assert!(writes(&rec).is_empty(), "{:?}", writes(&rec));
}

#[tokio::test]
async fn a_removed_pair_is_stamped_once_and_the_second_beat_writes_nothing() {
    // Stamped NOW, never a fixed date: a literal went past the seven-day grace on the calendar and
    // the second beat rightly wrote the due `delete-after` (2026-09-23).
    let stamped = chrono::Utc::now().to_rfc3339();
    let routes = vec![benches(vec![bench("bob", "acme", "full", None)]), benches(vec![bench("bob", "acme", "paused", Some(&stamped))])];
    let (s, rec, _) = setup(routes, &[("bob", "acme")]);
    let _ = reconcile(&s).await;
    let sent = rec.sent("PATCH", &path("bob", "acme"));
    assert_eq!(sent.len(), 2);
    let a = &sent[0]["metadata"]["annotations"];
    let at = chrono::DateTime::parse_from_rfc3339(a[REMOVED_AT].as_str().unwrap()).unwrap();
    assert_eq!(a[DELETE_AFTER], json!((at + MEMBER_REMOVAL_GRACE).to_rfc3339()), "grace starts with the stamp");
    assert!(rec.requests().iter().any(|r| r.contains("fieldManager=kloudlite-membership")), "{:?}", rec.requests());
    assert_eq!(sent[1], json!({"spec": {"access": "paused"}}));
    let _ = reconcile(&s).await;
    assert_eq!(rec.sent("PATCH", &path("bob", "acme")).len(), 2, "the second beat writes nothing");
}

#[tokio::test]
async fn a_deleted_team_waits_out_the_grace_too() {
    let (s, rec, _) = setup(vec![benches(vec![bench("bob", "gone", "paused", Some(&chrono::Utc::now().to_rfc3339()))])], &[("bob", "gone")]);
    let _ = reconcile(&s).await;
    assert!(writes(&rec).is_empty(), "{:?}", writes(&rec));
}

#[tokio::test]
async fn a_readd_during_the_grace_clears_the_stamp() {
    let (s, rec, _) = setup(vec![benches(vec![bench("alice", "acme", "full", Some("2026-09-15T00:00:00Z"))])], &[("alice", "acme")]);
    let _ = reconcile(&s).await;
    assert_eq!(rec.sent("PATCH", &path("alice", "acme")), vec![json!({"metadata": {"annotations": {REMOVED_AT: null, DELETE_NOW: null, DELETE_AFTER: null}}})]);
}

#[tokio::test]
async fn a_paused_member_is_never_stamped() {
    let (s, rec, _) = setup(vec![benches(vec![bench("paula", "acme", "full", None)])], &[("paula", "acme")]);
    let _ = reconcile(&s).await;
    assert_eq!(rec.sent("PATCH", &path("paula", "acme")), vec![json!({"spec": {"access": "paused", "desiredState": "stopped"}})]);
    assert!(rec.calls().iter().all(|c| !c.contains("removed-at")));
}

fn secret_path(owner: &str, team: &str) -> String {
    format!("/api/v1/namespaces/{}/secrets/{}", crd::ws_namespace(owner, team), crate::k8s::BENCH_TOOL_SECRET)
}

fn paused_pair(bench_json: serde_json::Value, team_ws: serde_json::Value) -> Vec<crate::kube_test::Route> {
    vec![
        benches(vec![bench_json, team_ws, ws("w9", "paula", "")]),
        patch(format!("{API}/workspaces/w1"), ws("w1", "paula", "acme")),
        del(secret_path("paula", "acme"), 200),
    ]
}

#[tokio::test]
async fn pause_stops_bench_and_team_workspaces_and_marks_access() {
    let (s, rec, _) = setup(paused_pair(bench("paula", "acme", "full", None), ws("w1", "paula", "acme")), &[("paula", "acme")]);
    let _ = reconcile(&s).await;
    assert_eq!(rec.sent("PATCH", &path("paula", "acme")), vec![json!({"spec": {"access": "paused", "desiredState": "stopped"}})]);
    assert_eq!(rec.sent("PATCH", &format!("{API}/workspaces/w1")), vec![json!({"spec": {"access": "paused", "desiredState": "stopped"}})], "every workspace of the pair, not only the bench");
    assert_eq!(deletes(&rec), vec![format!("DELETE {}", secret_path("paula", "acme"))], "only the tool token is deleted");
}

/// Removed, re-added and paused inside one beat: one pass both clears the stamps and pauses.
#[tokio::test]
async fn a_paused_member_still_carrying_a_stamp_is_cleared_and_paused_in_one_pass() {
    let fresh = chrono::Utc::now().to_rfc3339();
    let (s, rec, _) = setup(paused_pair(bench("paula", "acme", "full", Some(&fresh)), ws("w1", "paula", "acme")), &[("paula", "acme")]);
    let _ = reconcile(&s).await;
    assert_eq!(
        rec.sent("PATCH", &path("paula", "acme")),
        vec![
            json!({"metadata": {"annotations": {REMOVED_AT: null, DELETE_NOW: null, DELETE_AFTER: null}}}),
            json!({"spec": {"access": "paused", "desiredState": "stopped"}}),
        ]
    );
    assert_eq!(rec.sent("PATCH", &format!("{API}/workspaces/w1")), vec![json!({"spec": {"access": "paused", "desiredState": "stopped"}})]);
    assert_eq!(deletes(&rec), vec![format!("DELETE {}", secret_path("paula", "acme"))]);
}

#[tokio::test]
async fn pause_twice_writes_nothing() {
    let mut b = bench("paula", "acme", "paused", None);
    b["spec"]["desiredState"] = json!("stopped");
    let mut w = ws("w1", "paula", "acme");
    w["spec"]["desiredState"] = json!("stopped");
    w["spec"]["access"] = json!("paused");
    let (s, rec, _) = setup(paused_pair(b, w), &[]);
    let _ = reconcile(&s).await;
    assert!(writes(&rec).is_empty(), "{:?}", writes(&rec));
}

#[tokio::test]
async fn unpause_sets_full_and_starts_nothing() {
    let mut b = bench("alice", "acme", "paused", None);
    b["spec"]["desiredState"] = json!("stopped");
    let mut w = ws("w1", "alice", "acme");
    w["spec"]["desiredState"] = json!("stopped");
    w["spec"]["access"] = json!("paused");
    let (s, rec, _) = setup(vec![benches(vec![b, w])], &[("alice", "acme")]);
    let _ = reconcile(&s).await;
    assert_eq!(writes(&rec), vec![format!("PATCH {}", path("alice", "acme")), format!("PATCH {API}/workspaces/w1")], "every workspace of the pair");
    assert_eq!(rec.sent("PATCH", &path("alice", "acme")), vec![json!({"spec": {"access": "full"}})]);
}

#[tokio::test]
async fn a_readd_after_removal_gets_full_access_back() {
    let (s, rec, _) = setup(vec![benches(vec![bench("alice", "acme", "paused", Some("2026-09-15T00:00:00Z"))])], &[("alice", "acme")]);
    let _ = reconcile(&s).await;
    assert!(rec.sent("PATCH", &path("alice", "acme")).contains(&json!({"spec": {"access": "full"}})));
}

#[tokio::test]
async fn a_personal_workspace_of_a_paused_member_is_untouched() {
    let (s, rec, _) = setup(paused_pair(bench("paula", "acme", "full", None), ws("w1", "paula", "acme")), &[("paula", "acme")]);
    let _ = reconcile(&s).await;
    assert!(!writes(&rec).iter().any(|c| c.contains("/workspaces/w9")), "{:?}", writes(&rec));
}

fn who(name: &str) -> crate::api::Caller {
    crate::api::Caller { name: name.into(), superadmin: false, parent: None, scope: None, jti8: None }
}

fn confirm(person: &str, team: &str) -> crate::api::removals::Confirm {
    crate::api::removals::Confirm { person: person.into(), team: team.into() }
}

#[tokio::test]
async fn delete_now_requires_admin_and_both_names() {
    use crate::api::removals::delete_now;
    let (s, rec, _) = setup(vec![], &[]);
    assert_eq!(delete_now(&s, &who("ann"), "acme", "bob", &confirm("bob", "other")).await.status(), 400);
    assert_eq!(delete_now(&s, &who("ann"), "acme", "bob", &confirm("bo", "acme")).await.status(), 400);
    assert_eq!(delete_now(&s, &who("mem"), "acme", "bob", &confirm("bob", "acme")).await.status(), 403);
    assert_eq!(delete_now(&s, &who("eve"), "acme", "bob", &confirm("bob", "acme")).await.status(), 404);
    assert!(rec.calls().is_empty(), "{:?}", rec.calls());
}

#[tokio::test]
async fn delete_now_trusts_the_superadmin_row_not_the_claim() {
    use crate::api::removals::delete_now;
    let (s, rec, _) = setup(vec![], &[]);
    let claim = |n: &str| crate::api::Caller { superadmin: true, ..who(n) };
    assert_eq!(delete_now(&s, &claim("eve"), "acme", "bob", &confirm("bob", "acme")).await.status(), 404, "a claim with no row");
    assert_eq!(delete_now(&s, &claim("flaky"), "acme", "bob", &confirm("bob", "acme")).await.status(), 503, "fails closed");
    assert!(rec.calls().is_empty(), "{:?}", rec.calls());
}

#[tokio::test]
async fn delete_now_refuses_an_unstamped_pair() {
    let (s, rec, _) = setup(vec![benches(vec![bench("alice", "acme", "full", None)])], &[]);
    let r = crate::api::removals::delete_now(&s, &who("ann"), "acme", "alice", &confirm("alice", "acme")).await;
    assert_eq!(r.status(), 409);
    assert!(writes(&rec).is_empty(), "{:?}", writes(&rec));
}

#[tokio::test]
async fn delete_now_refuses_a_readded_member_with_a_stale_stamp() {
    let (s, rec, _) = setup(vec![benches(vec![bench("alice", "acme", "full", Some(OLD))])], &[]);
    let r = crate::api::removals::delete_now(&s, &who("ann"), "acme", "alice", &confirm("alice", "acme")).await;
    assert_eq!(r.status(), 409);
    assert!(rec.calls().is_empty(), "judged before any read or write: {:?}", rec.calls());
}

#[tokio::test]
async fn delete_now_on_an_unreadable_directory_is_503_and_writes_nothing() {
    let (s, rec, _) = setup(vec![benches(vec![bench("bob", "down", "paused", Some(OLD))])], &[]);
    let admin = crate::api::Caller { superadmin: true, ..who("root") };
    let r = crate::api::removals::delete_now(&s, &admin, "down", "bob", &confirm("bob", "down")).await;
    assert_eq!(r.status(), 503);
    assert!(rec.calls().is_empty(), "{:?}", rec.calls());
}

/// The path segment is a handle: an email resolves to no person, which is a bad argument, while
/// a directory that cannot be read is still an outage.
#[tokio::test]
async fn delete_now_refuses_an_email_with_400_and_an_unreadable_directory_with_503() {
    use crate::api::removals::delete_now;
    let (s, rec, _) = setup(vec![], &[]);
    let r = delete_now(&s, &who("ann"), "acme", "bob@x.io", &confirm("bob@x.io", "acme")).await;
    assert_eq!(r.status(), 400);
    let r = delete_now(&s, &who("ann"), "acme", "blind", &confirm("blind", "acme")).await;
    assert_eq!(r.status(), 503, "membership readable? no — and the owner cannot be classified either");
    assert!(rec.calls().is_empty(), "nothing written either way: {:?}", rec.calls());
}

#[tokio::test]
async fn delete_now_marks_delete_after_now_and_deletes_nothing() {
    let fresh = chrono::Utc::now().to_rfc3339();
    let name = crd::bench_id("bob", "acme");
    let b = with_meta(bench("bob", "acme", "paused", None), &name, Some(&fresh), false);
    let routes = vec![benches(vec![b]), patch(path("bob", "acme"), bench("bob", "acme", "paused", None))];
    let (s, rec, _) = setup(routes, &[]);
    let before = chrono::Utc::now().timestamp();
    let r = crate::api::removals::delete_now(&s, &who("ann"), "Acme", "bob", &confirm("bob", "acme")).await;
    assert_eq!(r.status(), 202);
    let sent = rec.sent("PATCH", &path("bob", "acme"));
    let a = &sent[0]["metadata"]["annotations"];
    assert_eq!((&a[REMOVED_AT], &a[DELETE_NOW]), (&json!(fresh), &json!("true")), "removed-at is kept");
    let after = chrono::DateTime::parse_from_rfc3339(a[DELETE_AFTER].as_str().unwrap()).unwrap().timestamp();
    assert!((before..=chrono::Utc::now().timestamp()).contains(&after), "due now, not after the grace");
    assert!(rec.requests().iter().any(|r| r.contains("fieldManager=kloudlite-membership")));
    assert!(deletes(&rec).is_empty(), "{:?}", deletes(&rec));
}


/// The real path: a removal the beat has not stamped yet is judged, stamped by `reconcile_pair`
/// and then marked due now — one call, no waiting for the next beat.
#[tokio::test]
async fn delete_now_stamps_an_unjudged_removal_and_marks_it_due() {
    let name = crd::bench_id("bob", "acme");
    let stamped = with_meta(bench("bob", "acme", "paused", None), &name, Some(&chrono::Utc::now().to_rfc3339()), false);
    let routes = vec![benches(vec![bench("bob", "acme", "full", None)]), benches(vec![stamped]), patch(path("bob", "acme"), bench("bob", "acme", "paused", None))];
    let (s, rec, _) = setup(routes, &[]);
    let before = chrono::Utc::now().timestamp();
    let r = crate::api::removals::delete_now(&s, &who("ann"), "acme", "bob", &confirm("bob", "acme")).await;
    assert_eq!(r.status(), 202);
    let sent = rec.sent("PATCH", &path("bob", "acme"));
    assert!(sent[0]["metadata"]["annotations"][REMOVED_AT].is_string(), "the beat's stamp first: {sent:?}");
    let last = sent.last().unwrap();
    assert_eq!(last["metadata"]["annotations"][DELETE_NOW], json!("true"));
    let after = chrono::DateTime::parse_from_rfc3339(last["metadata"]["annotations"][DELETE_AFTER].as_str().unwrap()).unwrap().timestamp();
    assert!((before..=chrono::Utc::now().timestamp()).contains(&after), "due now: {last}");
    assert!(deletes(&rec).is_empty(), "{:?}", deletes(&rec));
}

#[test]
fn removals_lists_stamped_pairs_only() {
    let parse = |v: serde_json::Value| serde_json::from_value::<crd::Workspace>(v).unwrap();
    let mut foreign = bench("carl", "acme", "paused", None);
    foreign["metadata"]["annotations"] = json!({REMOVED_AT: OLD});
    let o = Objects {
        workspaces: vec![parse(bench("bob", "acme", "paused", Some(OLD))), parse(bench("alice", "acme", "full", None)), parse(foreign)],
        spaces: vec![],
    };
    let r = removals(&o);
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!((r[0].owner.as_str(), r[0].team.as_str()), ("bob", "acme"));
    assert_eq!(r[0].delete_at, "2020-01-08T00:00:00+00:00");
}
