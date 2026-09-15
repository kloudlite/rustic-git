//! The cluster's space grants: `space-env` in the space's namespace, `space-{ns}` in the
//! environment's, and nothing else in stage 1.
//!
//! ONE writer, which is what deletes a whole class of bug rather than patching instances of it.
//! Before this, every node hosting a pod of a space wrote both halves from its own cache: F1 (a
//! deleted policy was unrecreatable for 600 s because the delete path never called
//! `forget_applied`), F2 (the env-side prune read one node's cache against another's trigger), F3
//! (a per-pod condition gate deleted the namespace-wide `space-env`) and F6 (multi-writer flap
//! under cache lag) were all the same shape. Here there is no per-pod gate at all: the two halves
//! are a pure function of the `SpaceEnvironment`.
//!
//! Fan-out is bounded ON PURPOSE. One wish event re-renders that space's egress half and the
//! ingress half in at most two environments — the one it names now and the one it named before —
//! never "every environment", which is what `run.rs`'s `all_in_store` mapper did on every node.
//!
//! An unlisted cache is UNKNOWN: decide nothing, delete nothing. That rule matters more here than
//! in an agent, because this process decides for the whole cluster at once.

use crate::ctx::{drop_object, ensure, ReconcileErr};
use crate::{lease, Ctx};
use k8s_openapi::api::coordination::v1::Lease;
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kloudlite_workspaces::{crd, k8s};
use kube::runtime::controller::Action;
use kube::runtime::reflector::ObjectRef;
use kube::runtime::{watcher, Controller};
use kube::{Api, Resource, ResourceExt};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

/// A converged pass re-renders at this cadence with no event at all: `ensure`'s apply memory
/// expires at 600 s, so a policy deleted by hand comes back within roughly one beat after that.
const RESYNC: Duration = Duration::from_secs(300);

/// A pass refused only because this process is not (yet) the writer comes back inside one lease
/// TTL, not at `RESYNC`: the reconcilers start before the election is won, so every startup pass
/// lands here, and a 300 s wait turned a ~15 s failover into minutes (`ctl.failover`). Cheap for a
/// follower: `may_write` answers from `ctx.leading()` without a lease GET.
const NOT_LEADER_RETRY: Duration = Duration::from_secs(5);

/// A grant younger than this is never pruned by the env-side sweep.
///
/// With ONE writer the race this floor was written for (a new environment's node deleting a grant
/// the old node had just created) cannot happen at all. Kept as belt-and-braces for the one thing
/// still true here: the space cache and the policy LIST are read at different instants, so a grant
/// created between them would look orphaned. Cheap, and the cost of being wrong is a pod that
/// loses its environment.
const GRANT_MIN_AGE: Duration = Duration::from_secs(60);

/// Which environment a space follows, as far as this process knows.
pub enum Choice {
    /// The environment cache has not finished its first list. Render nothing, delete nothing.
    Unknown,
    /// Nothing to grant: the environment is gone, or it is in another region.
    None,
    Env(Arc<crd::Environment>),
}

/// The two halves a wish asks for, named. Pure — no cache, no API server.
#[derive(Debug, PartialEq)]
pub struct Desired {
    /// The space's own namespace, known whether or not the environment is: it is what lets the
    /// caller delete a stale egress half rather than leaving the space pointed at nothing.
    pub egress_ns: String,
    /// The environment's namespace, `Some` only when the environment exists AND is in this region
    /// — an environment elsewhere is another cluster, and its namespace does not exist here.
    pub env_ns: Option<String>,
}

pub fn desired(space: &crd::SpaceEnvironment, env: Option<&crd::Environment>, region: &str) -> Desired {
    Desired {
        egress_ns: crd::ws_namespace(&space.spec.owner, &space.spec.team),
        env_ns: env.filter(|e| e.spec.region == region).map(|e| crd::env_namespace(&e.name_any())),
    }
}

/// BOTH halves are owned by the `SpaceEnvironment`, which is cluster-scoped — the usual
/// "an ownerReference cannot cross namespaces" rule is about a NAMESPACED owner and does not apply
/// to it, so the ingress half in the environment's namespace is owned by the wish like the egress
/// half is. Not by the `Environment`: `ownerReferences` is a server-side-apply associative list
/// keyed by uid, so a forced apply naming a different owner would ADD a second `controller: true`
/// reference to the object the agents wrote (`bins/agent/src/controller/space.rs`) and the API
/// server refuses that with "only one reference can have Controller set to true" — forever, since
/// nothing after Task 6 would ever rewrite it.
fn owner_ref<K: Resource<DynamicType = ()>>(obj: &K) -> Result<OwnerReference, ReconcileErr> {
    obj.controller_owner_ref(&()).ok_or_else(|| ReconcileErr("object has no uid".into()))
}

/// The env-side derived set: which `space-*` names belong in this environment's namespace.
/// `None` in, `None` out — an unlisted cache prunes NOTHING, which is the whole rule.
fn kept_here(env_id: &str, spaces: Option<&[Arc<crd::SpaceEnvironment>]>) -> Option<HashSet<String>> {
    Some(
        spaces?
            .iter()
            .filter(|s| s.spec.environment == env_id)
            .map(|s| k8s::space_ingress_name(&crd::ws_namespace(&s.spec.owner, &s.spec.team)))
            .collect(),
    )
}

/// Every namespace one wish event may write in: the space's own, the environment it names now, and
/// the one it named last pass. Three, never "every environment".
fn touched_namespaces(space: &crd::SpaceEnvironment, prev: Option<&str>) -> Vec<String> {
    let mut ns = vec![crd::ws_namespace(&space.spec.owner, &space.spec.team)];
    if !space.spec.environment.is_empty() {
        ns.push(crd::env_namespace(&space.spec.environment));
    }
    if let Some(p) = prev.filter(|p| !p.is_empty() && *p != space.spec.environment) {
        ns.push(crd::env_namespace(p));
    }
    ns
}

/// THE fence: read the lease FRESH and compare its `leaseTransitions` to the term this process was
/// elected under. Anything else and we are not the writer any more, so we write nothing and demote
/// — a write that discovers a newer term abandons itself, it never re-elects.
///
/// Once per pass for the APPLIES, again immediately before every DELETE. The asymmetry is the
/// point: two processes applying the same content-identical object is a no-op, but a delete that
/// lands after the term was lost leaves the NEW leader's apply-memory fresh for `APPLY_RESYNC` —
/// F1 across two processes, and unrecreatable for ten minutes.
/// ponytail: a delete is rare (a transition, or a confirmed orphan), so the extra GET is paid only
/// there; per-apply reads would be a GET per space per beat for nothing.
pub(crate) async fn may_write(ctx: &Ctx) -> bool {
    if !ctx.leading() {
        return false;
    }
    let api: Api<Lease> = Api::namespaced(ctx.client.clone(), lease::LEASE_NAMESPACE);
    let cur = match lease::read(&api).await {
        Ok(c) => c,
        Err(e) => {
            // Cannot look ⇒ cannot claim. Keep-biased in the only safe direction: a follower
            // writes nothing, and every object here is level-triggered, so nothing degrades.
            ctx.demote("lease.unreadable");
            tracing::warn!(error = %e, reason = "unreadable", "write.fenced");
            return false;
        }
    };
    let ok = lease::may_write(ctx.epoch(), cur.as_ref().and_then(lease::view).as_ref());
    if !ok {
        tracing::warn!(holder = %ctx.holder, epoch = ctx.epoch(), reason = "newer term", "write.fenced");
        ctx.demote("fenced");
    }
    ok
}

fn resolve(ctx: &Ctx, space: &crd::SpaceEnvironment) -> Choice {
    let Some(envs) = ctx.environments() else { return Choice::Unknown };
    match envs.get(&ObjectRef::new(&space.spec.environment)) {
        Some(e) if e.spec.region == ctx.region => Choice::Env(e),
        _ => Choice::None,
    }
}

/// Drop the remembered choice of every space that is GONE. Only from a ready cache — an unlisted
/// one would forget the whole cluster's previous choices and strand each old ingress half until
/// that environment's own sweep. Unbounded growth is the other half: a process that ran for months
/// would otherwise remember every space ever deleted.
fn forget_gone_spaces(ctx: &Ctx) {
    let Some(store) = ctx.spaces() else { return };
    let live: HashSet<String> = store.state().iter().map(|s| s.name_any()).collect();
    ctx.last_choice.lock().unwrap_or_else(|p| p.into_inner()).retain(|k, _| live.contains(k));
}

/// Does the object exist? Asked only where a DELETE would otherwise be issued on every pass with
/// nothing to go on — a GET that answers "no" is what ends the sweep, permanently.
async fn exists(api: &Api<NetworkPolicy>, name: &str) -> Result<bool, ReconcileErr> {
    Ok(api.get_opt(name).await?.is_some())
}

/// One space's whole grant, rendered from the wish alone.
pub async fn reconcile_space(space: Arc<crd::SpaceEnvironment>, ctx: Arc<Ctx>) -> Result<Action, ReconcileErr> {
    let name = space.name_any();
    // The uid is half the memory key: a delete-and-recreate under the same name is a DIFFERENT
    // space, and a transition computed against the dead one's choice would delete a grant the new
    // one had just made.
    let uid = space.uid().unwrap_or_default();
    forget_gone_spaces(&ctx);
    let choice = resolve(&ctx, &space);
    if let Choice::Unknown = choice {
        tracing::debug!(space = %name, decision = "unknown", "reconcile.pass");
        return Ok(Action::requeue(Duration::from_secs(5)));
    }
    if !may_write(&ctx).await {
        return Ok(Action::requeue(NOT_LEADER_RETRY));
    }
    let env = match &choice {
        Choice::Env(e) => Some(e.as_ref()),
        _ => None,
    };
    let d = desired(&space, env, &ctx.region);
    let in_space: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), &d.egress_ns);
    let ingress_name = k8s::space_ingress_name(&d.egress_ns);
    // Both halves, the ingress one included: see `owner_ref`.
    let owner = owner_ref(space.as_ref())?;
    let prev = {
        let mem = ctx.last_choice.lock().unwrap_or_else(|p| p.into_inner());
        mem.get(&name).filter(|(u, _)| *u == uid).map(|(_, e)| e.clone())
    };

    match (&d.env_ns, env) {
        (Some(env_ns), Some(_)) => {
            ensure(&in_space, &k8s::space_egress(&d.egress_ns, env_ns, &space.spec.owner, &owner), &ctx).await?;
            let in_env: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), env_ns);
            ensure(&in_env, &k8s::space_ingress(env_ns, &d.egress_ns, &space.spec.owner, &owner), &ctx).await?;
        }
        // Nothing to point at: an egress half granting reach to a namespace that is not there is a
        // grant to whatever gets that name next, so it goes — ONCE, and the memory below is what
        // makes it once. Three states, and only the middle one costs anything:
        //   - we rendered a grant last pass  → this pass is the TRANSITION away from it: delete.
        //   - we have no memory (a restart)  → one GET decides, and settles the memory either way.
        //   - we already rendered nothing    → nothing to do, and NO GET: we know.
        _ => {
            let act = match prev.as_deref() {
                Some(p) if !p.is_empty() => true,
                Some(_) => false,
                None => exists(&in_space, k8s::SPACE_EGRESS_POLICY).await?,
            };
            if act {
                // Return BEFORE the memory write, as the ingress arm below does: settling the
                // memory to empty on a fenced pass would mean the delete is never retried.
                if !may_write(&ctx).await {
                    return Ok(Action::requeue(NOT_LEADER_RETRY));
                }
                drop_object(&in_space, &ctx, &d.egress_ns, k8s::SPACE_EGRESS_POLICY).await?;
            }
        }
    }

    // The one DELETE this reconciler makes from MEMORY rather than from a list: the ingress half in
    // the environment this space named last pass. A pass that changes nothing takes this branch
    // never, which is what keeps a converged cluster free of steady-state deletes.
    if let Some(old) = prev.clone().filter(|p| !p.is_empty() && *p != space.spec.environment) {
        let old_ns = crd::env_namespace(&old);
        let api: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), &old_ns);
        if !may_write(&ctx).await {
            return Ok(Action::requeue(NOT_LEADER_RETRY));
        }
        drop_object(&api, &ctx, &old_ns, &ingress_name).await?;
    }
    // The RENDERED choice, not the wish: an orphaned wish (the environment is gone, or in another
    // region) renders nothing, so it must be remembered as nothing. Remembering the wish instead
    // made every later pass read "transition" and delete the same absent policy once per resync,
    // forever — the wish does not change, so the transition never ends.
    let rendered = if d.env_ns.is_some() { space.spec.environment.clone() } else { String::new() };
    ctx.last_choice.lock().unwrap_or_else(|p| p.into_inner()).insert(name.clone(), (uid, rendered));

    // What was rendered, and from what. The vet's 3am note was that `space.grant.pruned` never
    // said which value it had read.
    tracing::info!(
        space = %name,
        environment = %space.spec.environment,
        env_ns = d.env_ns.as_deref().unwrap_or(""),
        namespaces = ?touched_namespaces(&space, prev.as_deref()),
        "grant.rendered"
    );
    Ok(Action::requeue(RESYNC))
}

/// The restart-safe half: whichever `space-*` halves are in this environment's namespace and should
/// not be. A DERIVED SET against the cluster-wide space cache, not a disagreement between two
/// caches — and with no cache at all it prunes nothing.
pub async fn reconcile_environment(env: Arc<crd::Environment>, ctx: Arc<Ctx>) -> Result<Action, ReconcileErr> {
    let id = env.name_any();
    forget_gone_spaces(&ctx);
    let Some(keep) = kept_here(&id, ctx.spaces().map(|s| s.state()).as_deref()) else {
        tracing::debug!(environment = %id, decision = "unknown", "reconcile.pass");
        return Ok(Action::requeue(Duration::from_secs(5)));
    };
    let ns = crd::env_namespace(&id);
    let policies: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), &ns);
    let list = match policies.list(&kube::api::ListParams::default()).await {
        Ok(l) => l.items,
        // No namespace yet is no grants yet.
        Err(kube::Error::Api(ae)) if ae.code == 404 => return Ok(Action::requeue(RESYNC)),
        Err(e) => return Err(ReconcileErr(e.to_string())),
    };
    for p in list {
        let name = p.name_any();
        // `space-env` is the EGRESS half's name and never lives in an environment namespace, but
        // it shares the prefix: excluded so a namespace that somehow holds one is never read as a
        // space's grant.
        if !name.starts_with("space-") || name == k8s::SPACE_EGRESS_POLICY || keep.contains(&name) || young(&p) {
            continue;
        }
        // Re-read the lease before each delete, same rule as the space side: this sweep has no
        // applies, so every write it makes is one of these.
        if !may_write(&ctx).await {
            return Ok(Action::requeue(NOT_LEADER_RETRY));
        }
        tracing::info!(environment = %id, policy = %name, kept = keep.len(), "grant.pruned");
        drop_object(&policies, &ctx, &ns, &name).await?;
    }
    Ok(Action::requeue(RESYNC))
}

/// Created within `GRANT_MIN_AGE` — or carrying no timestamp at all, which a real API server always
/// sets, so the only object without one is a fixture and keeping it is the safe read either way.
fn young(p: &NetworkPolicy) -> bool {
    let Some(t) = p.meta().creation_timestamp.as_ref() else { return true };
    let age = k8s_openapi::jiff::Timestamp::now().as_millisecond() - t.0.as_millisecond();
    age < GRANT_MIN_AGE.as_millis() as i64
}

fn error_policy<K>(_obj: Arc<K>, err: &ReconcileErr, _ctx: Arc<Ctx>) -> Action {
    tracing::warn!(error = %err, "reconcile.failed");
    Action::requeue(Duration::from_secs(10))
}

fn watch_config() -> watcher::Config {
    watcher::Config::default().timeout(60)
}

/// Two controllers over two shared watches. A `SpaceEnvironment` event reconciles THAT space; an
/// `Environment` event reconciles that environment's own prune AND the spaces naming it — a store
/// read bounded by those spaces, never every space in the cluster.
pub async fn run(ctx: Arc<Ctx>) {
    use futures::StreamExt;
    use kube::runtime::WatchStreamExt;

    let space_writer = ctx.space_writer.lock().unwrap_or_else(|p| p.into_inner()).take();
    let env_writer = ctx.environment_writer.lock().unwrap_or_else(|p| p.into_inner()).take();
    let (Some(space_writer), Some(env_writer)) = (space_writer, env_writer) else {
        // Two `run`s on one Ctx would be two controllers in one process, which is not a thing.
        tracing::error!("the store writers are already taken");
        return;
    };
    let (Some(space_sub), Some(env_self), Some(env_for_spaces)) =
        (space_writer.subscribe(), env_writer.subscribe(), env_writer.subscribe())
    else {
        tracing::error!("the stores are not shared");
        return;
    };

    // `reflect_shared`, not `reflect`: the plain variant fills the store and tells nobody, which is
    // a controller that never reconciles anything.
    let space_watch = watcher(Api::<crd::SpaceEnvironment>::all(ctx.client.clone()), watch_config())
        .default_backoff()
        .reflect_shared(space_writer)
        .touched_objects()
        .for_each(|r| async move {
            if let Err(e) = r {
                tracing::warn!(kind = "SpaceEnvironment", error = %e, "reconcile.queue.failed")
            }
        });
    let env_watch = watcher(Api::<crd::Environment>::all(ctx.client.clone()), watch_config())
        .default_backoff()
        .reflect_shared(env_writer)
        .touched_objects()
        .for_each(|r| async move {
            if let Err(e) = r {
                tracing::warn!(kind = "Environment", error = %e, "reconcile.queue.failed")
            }
        });

    let spaces_for_map = ctx.space_store.clone();
    let space_ctl = Controller::for_shared_stream(space_sub, ctx.space_store.clone())
        .watches_shared_stream(env_for_spaces, move |e: Arc<crd::Environment>| {
            let id = e.name_any();
            spaces_for_map
                .state()
                .into_iter()
                .filter(move |s| s.spec.environment == id)
                .map(|s| ObjectRef::new(&s.name_any()))
                .collect::<Vec<_>>()
        })
        .run(reconcile_space, error_policy, ctx.clone())
        .for_each(|_| std::future::ready(()));
    let env_ctl = Controller::for_shared_stream(env_self, ctx.environment_store.clone())
        .run(reconcile_environment, error_policy, ctx.clone())
        .for_each(|_| std::future::ready(()));

    futures::join!(space_watch, env_watch, space_ctl, env_ctl);
}

#[cfg(test)]
mod tests {
    use super::*;
    use kloudlite_workspaces::kube_test;

    #[test]
    fn not_leader_requeues_within_one_lease_ttl() {
        assert!(NOT_LEADER_RETRY <= lease::TTL);
    }

    fn space(owner: &str, team: &str, env: &str) -> crd::SpaceEnvironment {
        let mut s = crd::space_environment(owner, team, env);
        s.metadata.uid = Some("uid-space".into());
        s
    }

    fn env(name: &str, owner: &str, region: &str) -> crd::Environment {
        let mut e = crd::Environment::new(
            name,
            crd::EnvironmentSpec {
                owner: owner.into(),
                name: name.into(),
                region: region.into(),
                services: vec![],
                storage: None,
                desired_state: crd::DesiredState::Running,
                restore: None,
                intercepts: vec![],
                system: None,
            },
        );
        e.metadata.uid = Some("uid-env".into());
        e
    }

    /// The space's own namespace, as `ws_namespace` folds an (owner, team) pair — a hashed tail,
    /// not `wt-{owner}-{team}`, so every assertion here derives it rather than spelling it.
    fn ns(owner: &str, team: &str) -> String {
        crd::ws_namespace(owner, team)
    }

    fn policy_path(ns: &str, name: &str) -> String {
        format!("/apis/networking.k8s.io/v1/namespaces/{ns}/networkpolicies/{name}")
    }

    /// The whole derived set, from the wish alone: one egress half in the space's namespace and
    /// one ingress half in the environment's, both named the way `policies.rs` names them.
    #[test]
    fn a_choice_renders_exactly_two_halves() {
        let d = desired(&space("alice", "acme", "env-1"), Some(&env("env-1", "acme", "r1")), "r1");
        assert_eq!(d.egress_ns, ns("alice", "acme"));
        assert_eq!(d.env_ns, Some(crd::env_namespace("env-1")));
    }

    /// An environment in ANOTHER region is not this controller's to grant: the pods are in a
    /// different cluster and the namespace here would not exist.
    #[test]
    fn an_environment_in_another_region_grants_nothing() {
        let d = desired(&space("alice", "acme", "env-1"), Some(&env("env-1", "acme", "r2")), "r1");
        assert_eq!(d.env_ns, None);
    }

    /// A gone environment likewise — and the egress namespace is still known, which is what lets
    /// the caller delete the stale egress half rather than leaving the space pointed at nothing.
    #[test]
    fn a_missing_environment_grants_nothing_but_still_names_the_space() {
        let d = desired(&space("alice", "acme", "env-1"), None, "r1");
        assert_eq!(d.egress_ns, ns("alice", "acme"));
        assert_eq!(d.env_ns, None);
    }

    /// The bytes themselves come from the one definition in `policies.rs` and are unchanged by
    /// this move — if they were not, an adopting apply would rewrite every policy in the cluster.
    #[test]
    fn the_rendered_bytes_are_the_shared_definition() {
        let s = space("alice", "acme", "env-1");
        let r = owner_ref(&s).unwrap();
        let space_ns = ns("alice", "acme");
        let eg = k8s::space_egress(&space_ns, "env-1", "alice", &r);
        assert_eq!(eg.metadata.name.as_deref(), Some(k8s::SPACE_EGRESS_POLICY));
        assert_eq!(eg.metadata.namespace.as_deref(), Some(&space_ns[..]));
        let ing = k8s::space_ingress("env-1", &space_ns, "alice", &r);
        assert_eq!(ing.metadata.name.as_deref(), Some(&k8s::space_ingress_name(&space_ns)[..]));
        assert_eq!(ing.metadata.namespace.as_deref(), Some("env-1"));
    }

    /// The env-side derived set: which `space-*` halves belong in THIS namespace, given the cache.
    /// A space pointing elsewhere is collected; a space pointing here is kept; and a cache that
    /// has not listed keeps EVERYTHING — read as empty it would strip every grant in the cluster
    /// on each controller restart.
    #[test]
    fn the_env_side_keeps_only_the_spaces_that_point_here() {
        let here = vec![Arc::new(space("alice", "acme", "env-1")), Arc::new(space("bob", "acme", "env-2"))];
        let keep = kept_here("env-1", Some(&here)).expect("a listed cache decides");
        assert!(keep.contains(&k8s::space_ingress_name(&ns("alice", "acme"))));
        assert!(!keep.contains(&k8s::space_ingress_name(&ns("bob", "acme"))));
        // Unknown: the caller must not prune at all, which is `None`, not an empty set.
        assert!(kept_here("env-1", None).is_none());
    }

    /// The fan-out bound, which is the whole reason this moved: one wish event touches the space's
    /// own namespace and at most two environment namespaces (the new choice and the old one).
    #[test]
    fn one_wish_event_touches_at_most_three_namespaces() {
        let touched = touched_namespaces(&space("alice", "acme", "env-2"), Some("env-1"));
        assert_eq!(touched, vec![ns("alice", "acme"), crd::env_namespace("env-2"), crd::env_namespace("env-1")]);
        // An unchanged choice names no old namespace, and therefore deletes nothing.
        assert_eq!(touched_namespaces(&space("alice", "acme", "env-2"), Some("env-2")).len(), 2);
    }

    /// A Ctx leading under epoch 3, over a lease the fence agrees with.
    fn leading(routes: Vec<kube_test::Route>) -> (Arc<Ctx>, kube_test::Recorder) {
        let mut all = vec![lease_route("ctl-test", 3)];
        all.extend(routes);
        let (client, rec) = kube_test::mock_client(all);
        let ctx = Arc::new(Ctx::for_test_with(client));
        ctx.promote(3);
        (ctx, rec)
    }

    fn lease_route(holder: &str, transitions: i64) -> kube_test::Route {
        kube_test::get(
            "/apis/coordination.k8s.io/v1/namespaces/kube-system/leases/kloudlite-controller",
            serde_json::json!({
                "metadata": {"name": "kloudlite-controller", "namespace": "kube-system"},
                "spec": {"holderIdentity": holder, "leaseTransitions": transitions,
                         "renewTime": k8s_openapi::jiff::Timestamp::now().to_string()}
            }),
        )
    }

    fn delete(path: String) -> kube_test::Route {
        kube_test::Route {
            method: "DELETE",
            path,
            status: 200,
            body: serde_json::json!({"metadata": {"name": "gone"}}),
        }
    }

    /// The happy path, end to end: two applies, in the two namespaces, under the CONTROLLER's own
    /// field manager and FORCED — the apply that adopts whatever an agent wrote.
    #[tokio::test]
    async fn a_pass_applies_both_halves_under_the_controller_manager() {
        let space_ns = ns("alice", "acme");
        let (ctx, rec) = leading(vec![
            kube_test::patch(policy_path(&space_ns, k8s::SPACE_EGRESS_POLICY), serde_json::json!({})),
            kube_test::patch(policy_path("env-1", &k8s::space_ingress_name(&space_ns)), serde_json::json!({})),
        ]);
        ctx.remember_environments(vec![env("env-1", "acme", "test")]);
        reconcile_space(Arc::new(space("alice", "acme", "env-1")), ctx.clone()).await.unwrap();
        let reqs = rec.requests().join("\n");
        assert!(reqs.contains("fieldManager=kloudlite-controller"), "{reqs}");
        assert!(reqs.contains("force=true"), "{reqs}");
        assert!(reqs.contains(&policy_path(&space_ns, k8s::SPACE_EGRESS_POLICY)), "{reqs}");
        assert!(reqs.contains(&policy_path("env-1", &k8s::space_ingress_name(&space_ns))), "{reqs}");
    }

    /// BOTH halves name the `SpaceEnvironment` as their controller. Asserted on what the
    /// RECONCILER sent, not on a hand-rolled render: naming the `Environment` on the ingress half
    /// (the shape this replaced) would add a second `controller: true` reference to the object the
    /// agents wrote, and the API server refuses that — "only one reference can have Controller set
    /// to true" — forever.
    #[tokio::test]
    async fn both_halves_are_owned_by_the_space() {
        let space_ns = ns("alice", "acme");
        let (egress, ingress) =
            (policy_path(&space_ns, k8s::SPACE_EGRESS_POLICY), policy_path("env-1", &k8s::space_ingress_name(&space_ns)));
        let (ctx, rec) = leading(vec![
            kube_test::patch(egress.clone(), serde_json::json!({})),
            kube_test::patch(ingress.clone(), serde_json::json!({})),
        ]);
        ctx.remember_environments(vec![env("env-1", "acme", "test")]);
        reconcile_space(Arc::new(space("alice", "acme", "env-1")), ctx.clone()).await.unwrap();
        for path in [egress, ingress] {
            let sent = rec.sent("PATCH", &path);
            assert_eq!(sent.len(), 1, "{path}: {sent:?}");
            let refs = sent[0]["metadata"]["ownerReferences"].as_array().expect("an owner reference");
            assert_eq!(refs.len(), 1, "{path}: {refs:?}");
            assert_eq!(refs[0]["kind"], "SpaceEnvironment", "{path}");
            assert_eq!(refs[0]["uid"], "uid-space", "{path}");
            assert_eq!(refs[0]["controller"], true, "{path}");
        }
    }

    /// Adoption, on the wire: the apply lands on an object the AGENT already wrote (same policy,
    /// its own `controller: true` reference to the same `SpaceEnvironment` uid) and the body we
    /// send carries exactly one ownerReference — so the associative list is updated in place and
    /// never grows a second controller.
    #[tokio::test]
    async fn the_adopting_apply_sends_exactly_one_owner_reference() {
        let space_ns = ns("alice", "acme");
        let ingress = policy_path("env-1", &k8s::space_ingress_name(&space_ns));
        let agent_written = serde_json::json!({
            "apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy",
            "metadata": {"name": k8s::space_ingress_name(&space_ns), "namespace": "env-1",
                "ownerReferences": [{"apiVersion": "kloudlite.io/v1alpha1", "kind": "SpaceEnvironment",
                    "name": space_ns, "uid": "uid-space", "controller": true, "blockOwnerDeletion": true}],
                "managedFields": [{"manager": "kloudlite-agent", "operation": "Apply"}]},
        });
        let (ctx, rec) = leading(vec![
            kube_test::patch(policy_path(&space_ns, k8s::SPACE_EGRESS_POLICY), serde_json::json!({})),
            kube_test::patch(ingress.clone(), agent_written),
        ]);
        ctx.remember_environments(vec![env("env-1", "acme", "test")]);
        reconcile_space(Arc::new(space("alice", "acme", "env-1")), ctx.clone()).await.unwrap();
        let sent = rec.sent("PATCH", &ingress);
        assert_eq!(sent.len(), 1, "{sent:?}");
        let refs = sent[0]["metadata"]["ownerReferences"].as_array().expect("an owner reference");
        assert_eq!(refs.len(), 1, "{refs:?}");
        assert_eq!(refs[0]["uid"], "uid-space");
        assert_eq!(refs[0]["kind"], "SpaceEnvironment");
    }

    /// An unlisted environment cache renders nothing and deletes nothing — the rule that, read the
    /// other way, strips every grant in the cluster on each restart. No route is canned, so any
    /// call at all would be a 404 and a failed pass.
    #[tokio::test]
    async fn an_unknown_cache_writes_nothing() {
        let (ctx, rec) = leading(vec![]);
        reconcile_space(Arc::new(space("alice", "acme", "env-1")), ctx.clone()).await.unwrap();
        assert!(rec.calls().is_empty(), "{:?}", rec.calls());
    }

    /// Not the leader: the fence is checked before the first write, so a demoted process makes none.
    #[tokio::test]
    async fn a_follower_writes_nothing() {
        let (ctx, rec) = leading(vec![]);
        ctx.demote("test");
        ctx.remember_environments(vec![env("env-1", "acme", "test")]);
        reconcile_space(Arc::new(space("alice", "acme", "env-1")), ctx.clone()).await.unwrap();
        assert!(rec.calls().iter().all(|c| !c.contains("networkpolicies")), "{:?}", rec.calls());
    }

    /// A newer term fences the pass: the lease says epoch 9 and somebody else holds it, we were
    /// elected under 3, so nothing is written and the process demotes rather than finishing.
    #[tokio::test]
    async fn a_stale_term_is_fenced() {
        let (client, rec) = kube_test::mock_client(vec![lease_route("someone-else", 9)]);
        let ctx = Arc::new(Ctx::for_test_with(client));
        ctx.promote(3);
        ctx.remember_environments(vec![env("env-1", "acme", "test")]);
        reconcile_space(Arc::new(space("alice", "acme", "env-1")), ctx.clone()).await.unwrap();
        assert!(rec.calls().iter().all(|c| !c.contains("networkpolicies")), "{:?}", rec.calls());
        assert!(!ctx.leading());
    }

    /// A switch deletes exactly one grant — the ingress half in the environment the space named
    /// last pass — and FORGETS the apply, so the same policy can be created again immediately
    /// rather than being unrecreatable for `APPLY_RESYNC`. That forget is the vet's CRITICAL.
    #[tokio::test]
    async fn a_switch_deletes_the_old_ingress_half_and_forgets_it() {
        let space_ns = ns("alice", "acme");
        let ingress = k8s::space_ingress_name(&space_ns);
        let (ctx, rec) = leading(vec![
            kube_test::patch(policy_path(&space_ns, k8s::SPACE_EGRESS_POLICY), serde_json::json!({})),
            kube_test::patch(policy_path("env-2", &ingress), serde_json::json!({})),
            delete(policy_path("env-1", &ingress)),
        ]);
        ctx.remember_environments(vec![env("env-2", "acme", "test")]);
        ctx.last_choice.lock().unwrap().insert(space_ns.clone(), ("uid-space".into(), "env-1".into()));
        // Seed `ensure`'s memory as an earlier pass in the OLD environment would have left it.
        let key = format!("NetworkPolicy/{}/{ingress}", crd::env_namespace("env-1"));
        ctx.applied.lock().unwrap().insert(key.clone(), (0, std::time::Instant::now()));
        reconcile_space(Arc::new(space("alice", "acme", "env-2")), ctx.clone()).await.unwrap();
        assert!(rec.calls().iter().any(|c| c.starts_with("DELETE") && c.contains("env-1")), "{:?}", rec.calls());
        let applied = ctx.applied.lock().unwrap();
        assert!(!applied.contains_key(&key), "{:?}", applied.keys());
        // The new choice is remembered, so the NEXT pass deletes nothing.
        assert_eq!(ctx.last_choice.lock().unwrap().get(&space_ns).map(|(_, e)| e.as_str()), Some("env-2"));
    }

    /// A converged pass issues no DELETE at all: the old-choice branch is a transition, never a
    /// steady state.
    #[tokio::test]
    async fn a_converged_pass_deletes_nothing() {
        let space_ns = ns("alice", "acme");
        let (ctx, rec) = leading(vec![
            kube_test::patch(policy_path(&space_ns, k8s::SPACE_EGRESS_POLICY), serde_json::json!({})),
            kube_test::patch(policy_path("env-1", &k8s::space_ingress_name(&space_ns)), serde_json::json!({})),
        ]);
        ctx.remember_environments(vec![env("env-1", "acme", "test")]);
        ctx.last_choice.lock().unwrap().insert(space_ns, ("uid-space".into(), "env-1".into()));
        reconcile_space(Arc::new(space("alice", "acme", "env-1")), ctx.clone()).await.unwrap();
        assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE")), "{:?}", rec.calls());
    }

    /// A space whose environment is gone loses its egress half — a grant to a namespace that is
    /// not there is a grant to whatever gets that name next. With no memory of a previous choice
    /// (a just-restarted process) the GET is what licenses the delete.
    #[tokio::test]
    async fn a_gone_environment_drops_the_egress_half() {
        let space_ns = ns("alice", "acme");
        let path = policy_path(&space_ns, k8s::SPACE_EGRESS_POLICY);
        let (ctx, rec) = leading(vec![
            kube_test::get(path.clone(), serde_json::json!({"metadata": {"name": k8s::SPACE_EGRESS_POLICY}})),
            delete(path.clone()),
        ]);
        ctx.remember_environments(vec![]);
        reconcile_space(Arc::new(space("alice", "acme", "env-1")), ctx.clone()).await.unwrap();
        assert!(rec.calls().iter().any(|c| *c == format!("DELETE {path}")), "{:?}", rec.calls());
    }

    /// And it deletes it ONCE. A cleared choice is the steady state of a space with no environment:
    /// the second pass finds the object gone and issues nothing, rather than a DELETE per pass
    /// forever.
    #[tokio::test]
    async fn a_cleared_choice_deletes_the_egress_half_once() {
        let space_ns = ns("alice", "acme");
        let path = policy_path(&space_ns, k8s::SPACE_EGRESS_POLICY);
        let (ctx, rec) = leading(vec![
            kube_test::get(path.clone(), serde_json::json!({"metadata": {"name": k8s::SPACE_EGRESS_POLICY}})),
            // The mock walks a path's routes in order, so the second GET is the world after the
            // delete: gone.
            kube_test::not_found(path.clone()),
            delete(path.clone()),
        ]);
        ctx.remember_environments(vec![]);
        let s = Arc::new(space("alice", "acme", ""));
        reconcile_space(s.clone(), ctx.clone()).await.unwrap();
        reconcile_space(s, ctx.clone()).await.unwrap();
        let deletes: Vec<_> = rec.calls().into_iter().filter(|c| c.starts_with("DELETE")).collect();
        assert_eq!(deletes.len(), 1, "{deletes:?}");
    }

    /// The ORPHAN: the wish still names an environment, but the environment is gone (or in another
    /// region), so nothing is rendered. The memory records what was RENDERED — nothing — so the
    /// second pass is not a transition and issues no second DELETE. Remembering the wish instead
    /// deleted the same absent policy once per resync forever, since the wish never changes.
    #[tokio::test]
    async fn an_orphaned_wish_deletes_the_egress_half_once() {
        let space_ns = ns("alice", "acme");
        let path = policy_path(&space_ns, k8s::SPACE_EGRESS_POLICY);
        let (ctx, rec) = leading(vec![
            kube_test::get(path.clone(), serde_json::json!({"metadata": {"name": k8s::SPACE_EGRESS_POLICY}})),
            delete(path.clone()),
        ]);
        // A ready cache with no environment at all: the wish is orphaned, not unknown.
        ctx.remember_environments(vec![]);
        let s = Arc::new(space("alice", "acme", "env-1"));
        reconcile_space(s.clone(), ctx.clone()).await.unwrap();
        reconcile_space(s, ctx.clone()).await.unwrap();
        let deletes: Vec<_> = rec.calls().into_iter().filter(|c| c.starts_with("DELETE")).collect();
        assert_eq!(deletes.len(), 1, "{deletes:?}");
    }

    /// And a space with nothing to render whose policy is already gone settles after ONE GET: the
    /// confirmed absence is remembered, so a settled space costs no API call at all per pass.
    #[tokio::test]
    async fn a_settled_space_stops_asking() {
        let space_ns = ns("alice", "acme");
        let path = policy_path(&space_ns, k8s::SPACE_EGRESS_POLICY);
        let (ctx, rec) = leading(vec![kube_test::not_found(path.clone())]);
        ctx.remember_environments(vec![]);
        let s = Arc::new(space("alice", "acme", ""));
        reconcile_space(s.clone(), ctx.clone()).await.unwrap();
        reconcile_space(s, ctx.clone()).await.unwrap();
        let gets: Vec<_> = rec.calls().into_iter().filter(|c| c == &format!("GET {path}")).collect();
        assert_eq!(gets.len(), 1, "{gets:?}");
        assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE")), "{:?}", rec.calls());
    }

    /// A transition that finds the fence shut writes nothing AND leaves the memory as it was, so
    /// the delete is retried: settling the memory on a fenced pass would lose it for good. The
    /// second pass leads again and issues the one DELETE.
    #[tokio::test]
    async fn a_fenced_transition_keeps_its_memory_and_retries() {
        let space_ns = ns("alice", "acme");
        let path = policy_path(&space_ns, k8s::SPACE_EGRESS_POLICY);
        let (client, rec) = kube_test::mock_client(vec![
            // Pass 1: the pass-level fence agrees (3), the pre-delete re-read finds a newer term.
            lease_route("ctl-test", 3),
            lease_route("someone-else", 9),
            // Pass 2: ours again, for both reads.
            lease_route("ctl-test", 3),
            delete(path.clone()),
        ]);
        let ctx = Arc::new(Ctx::for_test_with(client));
        ctx.promote(3);
        ctx.remember_environments(vec![]);
        ctx.last_choice.lock().unwrap().insert(space_ns.clone(), ("uid-space".into(), "env-1".into()));
        let s = Arc::new(space("alice", "acme", "env-1"));

        reconcile_space(s.clone(), ctx.clone()).await.unwrap();
        assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE")), "{:?}", rec.calls());
        assert_eq!(
            ctx.last_choice.lock().unwrap().get(&space_ns).map(|(_, e)| e.as_str()),
            Some("env-1"),
            "a fenced pass must not settle the memory"
        );

        ctx.promote(3);
        reconcile_space(s, ctx.clone()).await.unwrap();
        let deletes: Vec<_> = rec.calls().into_iter().filter(|c| c.starts_with("DELETE")).collect();
        assert_eq!(deletes, vec![format!("DELETE {path}")]);
    }

    /// A space deleted and recreated under the same name is a DIFFERENT space: the dead one's
    /// choice must not make the new one's pass a transition, or it would delete the grant this
    /// very pass just applied.
    #[tokio::test]
    async fn a_recreated_space_does_not_inherit_the_dead_one_s_choice() {
        let space_ns = ns("alice", "acme");
        let (ctx, rec) = leading(vec![
            kube_test::patch(policy_path(&space_ns, k8s::SPACE_EGRESS_POLICY), serde_json::json!({})),
            kube_test::patch(policy_path("env-1", &k8s::space_ingress_name(&space_ns)), serde_json::json!({})),
        ]);
        ctx.remember_environments(vec![env("env-1", "acme", "test")]);
        ctx.last_choice.lock().unwrap().insert(space_ns.clone(), ("uid-of-the-dead-one".into(), "env-9".into()));
        reconcile_space(Arc::new(space("alice", "acme", "env-1")), ctx.clone()).await.unwrap();
        assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE")), "{:?}", rec.calls());
        assert_eq!(
            ctx.last_choice.lock().unwrap().get(&space_ns).map(|(u, e)| (u.as_str(), e.as_str())),
            Some(("uid-space", "env-1"))
        );
    }

    /// A space that is GONE from a ready cache is forgotten, so the map is bounded by the live
    /// cluster rather than by uptime.
    #[tokio::test]
    async fn a_gone_space_is_forgotten() {
        let (ctx, _rec) = leading(vec![kube_test::get(
            "/apis/networking.k8s.io/v1/namespaces/env-1/networkpolicies",
            serde_json::json!({"apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicyList", "metadata": {}, "items": []}),
        )]);
        ctx.last_choice.lock().unwrap().insert(ns("gone", "acme"), ("uid-gone".into(), "env-1".into()));
        ctx.remember_spaces(vec![space("alice", "acme", "env-1")]);
        reconcile_environment(Arc::new(env("env-1", "acme", "test")), ctx.clone()).await.unwrap();
        assert!(ctx.last_choice.lock().unwrap().is_empty());
    }

    /// The env-side sweep with an UNLISTED space cache: it does not even list, and deletes nothing.
    #[tokio::test]
    async fn the_env_sweep_prunes_nothing_from_an_unknown_cache() {
        let (ctx, rec) = leading(vec![]);
        reconcile_environment(Arc::new(env("env-1", "acme", "test")), ctx.clone()).await.unwrap();
        assert!(rec.calls().is_empty(), "{:?}", rec.calls());
    }

    /// And with a listed one: an orphaned grant old enough to judge goes; a grant that belongs
    /// here, one created seconds ago, and a policy that is not a space grant all stay.
    #[tokio::test]
    async fn the_env_sweep_drops_an_orphan_and_spares_a_young_grant() {
        let old = "2020-01-01T00:00:00Z";
        let now = k8s_openapi::jiff::Timestamp::now().to_string();
        let (alice, bob, fresh) = (ns("alice", "acme"), ns("bob", "acme"), ns("new", "acme"));
        let orphan = k8s::space_ingress_name(&bob);
        let (ctx, rec) = leading(vec![
            kube_test::get(
                "/apis/networking.k8s.io/v1/namespaces/env-1/networkpolicies",
                serde_json::json!({"apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicyList", "metadata": {},
                    "items": [
                        {"metadata": {"name": orphan, "namespace": "env-1", "creationTimestamp": old}},
                        {"metadata": {"name": k8s::space_ingress_name(&alice), "namespace": "env-1", "creationTimestamp": old}},
                        {"metadata": {"name": k8s::space_ingress_name(&fresh), "namespace": "env-1", "creationTimestamp": now}},
                        {"metadata": {"name": "allow-dns", "namespace": "env-1", "creationTimestamp": old}}
                    ]}),
            ),
            delete(policy_path("env-1", &orphan)),
        ]);
        ctx.remember_spaces(vec![space("alice", "acme", "env-1"), space("bob", "acme", "env-9")]);
        reconcile_environment(Arc::new(env("env-1", "acme", "test")), ctx.clone()).await.unwrap();
        let deletes: Vec<_> = rec.calls().into_iter().filter(|c| c.starts_with("DELETE")).collect();
        assert_eq!(deletes.len(), 1, "{deletes:?}");
        assert!(deletes[0].contains(&orphan), "{deletes:?}");
    }
}
