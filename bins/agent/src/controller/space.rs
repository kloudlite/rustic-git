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
//! for the space and the migration has not settled the object (`crd::retired_attach`), so an agent
//! rolled before the api keeps a live attach working. The fallback manages only that pod's legacy
//! per-pod `attach-{id}` pair; the pair is collected once when a choice takes over. See
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
/// The reason for a grant made through the retired field: the per-pod pair, as older builds wrote.
pub(crate) const LEGACY_REASON: &str = "Converged";

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
    /// The pod's parent (Workspace or Bench): owner of the legacy per-pod egress half.
    pub owner_ref: k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference,
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

    // Every DELETE below is a TRANSITION away from a grant the previous pass recorded as True, so a
    // pod that settles (converged, refused, never attached) issues none on later passes — the
    // 2026-09-12 lesson. A condition that is True with a reason other than `Space` is the per-pod
    // legacy pair: written by an older build, or by the field fallback below.
    let prev = p.prev.iter().find(|c| c.type_ == crd::ATTACHED && c.status == "True");
    let legacy_prev = prev.filter(|c| c.reason != SPACE_REASON);
    let space_prev = prev.filter(|c| c.reason == SPACE_REASON);
    let from_space = chosen.as_ref().and_then(|c| c.space.clone());
    let policies: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), &ns);
    let legacy_name = k8s::attach_policy_name(p.id);
    let drop_legacy_ingress = |env_id: String| {
        let api: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), &crd::env_namespace(&env_id));
        let name = legacy_name.clone();
        async move { delete_ignoring_404(&api, &name).await }
    };
    let reason = match (&env, &env_ns, &from_space) {
        // A choice exists: the namespace pair, owned by it, and the legacy pair goes once.
        (Some(_), Some(env_ns), Some(space)) => {
            let owner_ref = owner_ref_of_kind(space.as_ref())?;
            ensure(&policies, &k8s::space_egress(&ns, env_ns, p.owner, &owner_ref), ctx).await?;
            let in_env: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), env_ns);
            ensure(&in_env, &k8s::space_ingress(env_ns, &ns, p.owner, &owner_ref), ctx).await?;
            if let Some(old) = legacy_prev {
                delete_ignoring_404(&policies, &legacy_name).await?;
                drop_legacy_ingress(old.message.clone()).await?;
            }
            SPACE_REASON
        }
        // The field fallback manages ONLY this pod's own legacy pair. `space-env` is one per
        // namespace, and siblings still on their own fields would otherwise fight over it.
        // ponytail: removed with the fallback next release.
        (Some(e), Some(env_ns), None) => {
            ensure(&policies, &k8s::attach_egress(&ns, p.id, env_ns, p.owner, &p.owner_ref), ctx).await?;
            let in_env: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), env_ns);
            ensure(&in_env, &k8s::attach_ingress(env_ns, &ns, p.id, p.owner, &owner_ref_of_kind(e)?), ctx).await?;
            if let Some(old) = legacy_prev.filter(|c| c.message != e.name_any()) {
                drop_legacy_ingress(old.message.clone()).await?;
            }
            if space_prev.is_some() {
                delete_ignoring_404(&policies, k8s::SPACE_EGRESS_POLICY).await?;
            }
            LEGACY_REASON
        }
        _ => {
            if let Some(old) = legacy_prev {
                delete_ignoring_404(&policies, &legacy_name).await?;
                drop_legacy_ingress(old.message.clone()).await?;
            }
            if space_prev.is_some() {
                delete_ignoring_404(&policies, k8s::SPACE_EGRESS_POLICY).await?;
            }
            ""
        }
    };

    Ok(Attached::Set(match (&env, refusal) {
        // The BARE environment id: the web and `/v1` read it back as the space's environment.
        (Some(e), _) => Some(crd::condition(crd::ATTACHED, true, reason, &e.name_any(), p.gen)),
        (None, Some((why, msg))) => Some(crd::condition(crd::ATTACHED, false, why, &msg, p.gen)),
        (None, None) => None,
    }))
}

async fn write_resolv(ctx: &Arc<Ctx>, id: &str, ns: &str, env_ns: Option<&str>) -> Result<(), ReconcileErr> {
    let (pool, id, ns, env_ns) = (ctx.pool.clone(), id.to_string(), ns.to_string(), env_ns.map(str::to_string));
    tokio::task::spawn_blocking(move || super::write_resolv_conf(&pool, &id, &ns, env_ns.as_deref()))
        .await
        .map_err(|e| ReconcileErr(e.to_string()))?
}
