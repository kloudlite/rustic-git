//! A person's SPACE follows one environment: every pod the platform runs for that person in that
//! team resolves the environment's services by bare name. This is the one place any pod kind asks
//! "which environment", and the one converge every pod kind runs for it.
//!
//! The choice is a `SpaceEnvironment` named by the space's namespace, written only by `/v1` and
//! read here from the cluster-wide cache (`Ctx::spaces`). An UNLISTED cache is `Unknown`, never
//! "no environment": read as empty it would strip DNS from every pod in the region on every agent
//! restart, so an unknown pass rewrites nothing and deletes nothing.
//!
//! Migration window (one release): an object still carrying the retired
//! `spec.attachedEnvironment` resolves through it only while the cache is KNOWN and holds no choice
//! for the space, so an agent rolled before the api keeps a live attach working. The legacy
//! per-pod `attach-{id}` policies are collected here once, on the first pass that converges a pod
//! in the new shape (its `Attached` reason is `Space`). See
//! `docs/superpowers/specs/2026-09-14-person-environment-design.md`.

use super::{delete_ignoring_404, ensure, owner_ref_of_kind, Ctx, ReconcileErr};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::{Api, ResourceExt};
use kloudlite_workspaces::crd;
use kloudlite_workspaces::k8s;
use std::sync::Arc;

/// The reason an `Attached=True` written by this module carries. Anything else is a condition an
/// older build wrote, which is what marks a pod whose legacy grant has not been collected yet.
pub(crate) const SPACE_REASON: &str = "Space";

/// Which environment a space follows, as far as this node knows.
#[derive(Debug, Clone, PartialEq)]
pub enum SpaceEnv {
    /// The cache has not finished its first list.
    Unknown,
    /// `None` is "no environment". `space` is the object behind a choice, absent only for the
    /// field fallback.
    Known(Option<Chosen>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chosen {
    pub environment: String,
    pub space: Option<Arc<crd::SpaceEnvironment>>,
}

/// THE resolver: the workspace reconciler, the bench reconciler and the intercept decision all ask
/// here, and so will any pod kind added later. `field` is the object's retired
/// `attachedEnvironment`, consulted only when the cache is known and has no choice.
pub fn space_environment(ctx: &Ctx, owner: &str, team: &str, field: Option<&str>) -> SpaceEnv {
    let Some(store) = ctx.spaces() else { return SpaceEnv::Unknown };
    let name = crd::space_name(owner, team);
    match store.get(&kube::runtime::reflector::ObjectRef::new(&name)) {
        Some(s) => SpaceEnv::Known(Some(Chosen { environment: s.spec.environment.clone(), space: Some(s) })),
        None => SpaceEnv::Known(field.filter(|f| !f.is_empty()).map(|f| Chosen { environment: f.to_string(), space: None })),
    }
}

/// What `converge_space` decided about the pod's `Attached` condition.
pub(crate) enum Attached {
    /// Cache unknown: keep whatever the condition says, touch nothing.
    Keep,
    /// Replace the condition with this one, or remove it (`None`).
    Set(Option<Condition>),
}

impl Attached {
    /// The environment namespace a converged pass pointed the pod at, if any.
    pub(crate) fn env_ns(&self) -> Option<String> {
        match self {
            Attached::Set(Some(c)) if c.status == "True" => Some(crd::env_namespace(&c.message)),
            _ => None,
        }
    }
}

pub(crate) struct Pod<'a> {
    /// The pod's own id — the resolv.conf path and the legacy `attach-{id}` name.
    pub id: &'a str,
    pub owner: &'a str,
    pub team: &'a str,
    /// The region the pod runs in; an environment elsewhere is another cluster.
    pub region: &'a str,
    pub field: Option<&'a str>,
    pub prev: &'a [Condition],
    pub gen: i64,
}

/// One pass for one pod of a space, in the spec's order: resolve, resolv.conf in place, the two
/// namespace-level policies, the legacy per-pod grant, and the condition to write. Idempotent SSA
/// throughout, so two nodes hosting pods of one space converge on identical objects.
pub(crate) async fn converge_space(ctx: &Arc<Ctx>, p: Pod<'_>) -> Result<Attached, ReconcileErr> {
    let ns = crd::ws_namespace(p.owner, p.team);
    let chosen = match space_environment(ctx, p.owner, p.team, p.field) {
        SpaceEnv::Unknown => {
            // A pod about to be created still needs its `type: File` mount target; a missing file
            // cannot be a running pod's DNS, so writing one without an environment strips nothing.
            if !std::path::Path::new(&k8s::attach_file(&ctx.pool, p.id)).exists() {
                write_resolv(ctx, p.id, &ns, None).await?;
            }
            tracing::debug!(pod = %p.id, "store.not_ready");
            return Ok(Attached::Keep);
        }
        SpaceEnv::Known(c) => c,
    };
    let (env, refusal) = match &chosen {
        None => (None, None),
        Some(c) => match Api::<crd::Environment>::all(ctx.client.clone()).get_opt(&c.environment).await? {
            None => (None, Some(("EnvironmentNotFound", format!("environment {} is gone", c.environment)))),
            Some(e) if e.spec.region != p.region => {
                (None, Some(("RegionMismatch", format!("environment {} is in {}", c.environment, e.spec.region))))
            }
            Some(e) => (Some(e), None),
        },
    };
    let env_ns = env.as_ref().map(|e| crd::env_namespace(&e.name_any()));
    write_resolv(ctx, p.id, &ns, env_ns.as_deref()).await?;

    let policies: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), &ns);
    match (&env, &env_ns) {
        (Some(e), Some(env_ns)) => {
            // Owned by the choice, so deleting it collects both halves; the field fallback has no
            // object, so the environment owns them until the migration writes one.
            let owner_ref = match chosen.as_ref().and_then(|c| c.space.as_deref()) {
                Some(s) => owner_ref_of_kind(s)?,
                None => owner_ref_of_kind(e)?,
            };
            ensure(&policies, &k8s::space_egress(&ns, env_ns, p.owner, &owner_ref), ctx).await?;
            let in_env: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), env_ns);
            ensure(&in_env, &k8s::space_ingress(env_ns, &ns, p.owner, &owner_ref), ctx).await?;
        }
        // Only when this pod was ever attached: a DELETE on every pass of every never-attached pod
        // is the 2026-09-12 lesson. The ingress half goes with its owner or the env's prune.
        _ if p.prev.iter().any(|c| c.type_ == crd::ATTACHED) => delete_ignoring_404(&policies, k8s::SPACE_EGRESS_POLICY).await?,
        _ => {}
    }

    // The legacy per-pod pair, collected once: a condition an older build wrote names the env
    // namespace its ingress half sits in.
    // ponytail: removed next release with the field fallback.
    if let Some(old) = p.prev.iter().find(|c| c.type_ == crd::ATTACHED && c.reason != SPACE_REASON) {
        delete_ignoring_404(&policies, &k8s::attach_policy_name(p.id)).await?;
        if old.status == "True" && !old.message.is_empty() {
            let in_old: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), &crd::env_namespace(&old.message));
            delete_ignoring_404(&in_old, &k8s::attach_policy_name(p.id)).await?;
        }
    }

    Ok(Attached::Set(match (&env, refusal) {
        // The BARE environment id: the web and `/v1` read it back as the space's environment.
        (Some(e), _) => Some(crd::condition(crd::ATTACHED, true, SPACE_REASON, &e.name_any(), p.gen)),
        (None, Some((reason, msg))) => Some(crd::condition(crd::ATTACHED, false, reason, &msg, p.gen)),
        (None, None) => None,
    }))
}

async fn write_resolv(ctx: &Arc<Ctx>, id: &str, ns: &str, env_ns: Option<&str>) -> Result<(), ReconcileErr> {
    let (pool, id, ns, env_ns) = (ctx.pool.clone(), id.to_string(), ns.to_string(), env_ns.map(str::to_string));
    tokio::task::spawn_blocking(move || super::write_resolv_conf(&pool, &id, &ns, env_ns.as_deref()))
        .await
        .map_err(|e| ReconcileErr(e.to_string()))?
}
