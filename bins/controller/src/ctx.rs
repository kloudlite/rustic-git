//! Everything one pass of this process needs, and the term it is allowed to write under.

use kloudlite_core::settings::LiveSettings;
use kloudlite_workspaces::crd;
use kloudlite_workspaces::settings::AgentSettings;
use kube::api::{Patch, PatchParams};
use kube::runtime::reflector::{store::Writer, store_shared, Store};
use kube::{Api, Resource};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Env-derived config. No secret, by design: this process holds none.
pub struct Config {
    /// `WS_REGION` — logged and stamped, never used to filter: one controller per cluster means
    /// every object it can see is its own.
    pub region: String,
    /// `POD_NAME`, from the downward API. The lease's `holderIdentity`.
    pub holder: String,
}

impl Config {
    pub fn from_env() -> Result<Config, String> {
        let region = std::env::var("WS_REGION").unwrap_or_default();
        let holder = std::env::var("POD_NAME").unwrap_or_default();
        Config::check(&region, &holder)?;
        Ok(Config { region, holder })
    }

    pub fn check(region: &str, holder: &str) -> Result<(), String> {
        if region.is_empty() {
            return Err("WS_REGION is required".into());
        }
        if holder.is_empty() {
            return Err("POD_NAME is required: it is the lease holder identity".into());
        }
        Ok(())
    }
}

pub struct Ctx {
    pub client: kube::Client,
    pub holder: String,
    pub region: String,
    /// The term this process was elected under, 0 when it is not the leader. Read immediately
    /// before every write (`lease::may_write`); a stale term demotes instead of finishing.
    pub(crate) epoch: AtomicU32,
    /// `ensure`'s memory, exactly as the agent's (`bins/agent/src/controller/status.rs`), except
    /// that with ONE writer per cluster it is now the truth for the whole cluster rather than one
    /// of N per-process guesses.
    pub applied: Mutex<HashMap<String, (u64, Instant)>>,
    pub settings: LiveSettings<AgentSettings>,
    /// EVERY `SpaceEnvironment` and `Environment` in the cluster. Unfiltered: one process decides
    /// for the whole cluster, which is the point.
    ///
    /// Read through `spaces()`/`environments()` and NEVER without `store_ready`. An unlisted cache
    /// is UNKNOWN, never empty — read as empty, the env-side prune would delete every grant in the
    /// cluster on each controller restart.
    pub space_store: Store<crd::SpaceEnvironment>,
    /// The writing half, until `space::run` takes it and drives the watch into it (tests feed it
    /// directly through `remember_spaces`).
    pub space_writer: Mutex<Option<Writer<crd::SpaceEnvironment>>>,
    pub environment_store: Store<crd::Environment>,
    pub environment_writer: Mutex<Option<Writer<crd::Environment>>>,
    /// Which environment each space named on its last rendered pass (space name → env id). The
    /// ONLY thing that turns "the wish changed" into a bounded delete: without it a switch would
    /// have to look for the old grant everywhere. A restarted process has no memory here, which is
    /// what `reconcile_environment`'s prune is for.
    pub last_choice: Mutex<HashMap<String, String>>,
}

/// How long `ensure` trusts its last apply — `bins/agent/src/controller/status.rs`'s rule,
/// unchanged. Bounds the one thing the skip costs: a policy deleted by hand, or by a path that
/// forgot to `forget_applied`, is re-applied within this.
const APPLY_RESYNC: Duration = Duration::from_secs(600);

impl Ctx {
    pub fn new(client: kube::Client, holder: String, region: String, settings: LiveSettings<AgentSettings>) -> Ctx {
        // Shared stores: the two reconcilers read them AND drive off them, so one watch per kind
        // feeds both (`reflect_shared` + `for_shared_stream`), not one watch per reader.
        let (space_store, space_writer) = store_shared(256);
        let (environment_store, environment_writer) = store_shared(256);
        Ctx {
            client,
            holder,
            region,
            epoch: AtomicU32::new(0),
            applied: Default::default(),
            settings,
            space_store,
            space_writer: Mutex::new(Some(space_writer)),
            environment_store,
            environment_writer: Mutex::new(Some(environment_writer)),
            last_choice: Default::default(),
        }
    }

    /// The cluster-wide space cache. `None` until its first list — unknown, never "no choice".
    pub fn spaces(&self) -> Option<&Store<crd::SpaceEnvironment>> {
        store_ready(&self.space_store).then_some(&self.space_store)
    }

    pub fn environments(&self) -> Option<&Store<crd::Environment>> {
        store_ready(&self.environment_store).then_some(&self.environment_store)
    }

    pub fn epoch(&self) -> u32 {
        self.epoch.load(Ordering::SeqCst)
    }
    pub fn leading(&self) -> bool {
        self.epoch() != 0
    }
    pub fn promote(&self, epoch: u32) {
        if self.epoch.swap(epoch, Ordering::SeqCst) != epoch {
            tracing::info!(holder = %self.holder, epoch, "leader.acquired");
        }
    }
    pub fn demote(&self, reason: &str) {
        let was = self.epoch.swap(0, Ordering::SeqCst);
        if was != 0 {
            tracing::info!(holder = %self.holder, epoch = was, reason, "leader.lost");
        }
    }
}

/// Has this reflector finished its first list? `wait_until_ready` is a one-shot latch, so polling
/// it once is the synchronous read of it — and a reconciler cannot await readiness anyway: the
/// answer it needs is "do I know yet", not "tell me when".
pub fn store_ready<K>(s: &Store<K>) -> bool
where
    K: Resource<DynamicType = ()> + Clone + 'static,
{
    use futures::FutureExt;
    s.wait_until_ready().now_or_never().is_some()
}

/// What went wrong in one pass. A string because every caller does the same thing with it: log it
/// and requeue.
#[derive(Debug)]
pub struct ReconcileErr(pub String);

impl std::fmt::Display for ReconcileErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ReconcileErr {}
impl From<kube::Error> for ReconcileErr {
    fn from(e: kube::Error) -> Self {
        ReconcileErr(e.to_string())
    }
}

/// Server-side apply of a whole child object, `bins/agent/src/controller/status.rs`'s `ensure`
/// with the controller's own field manager. `.force()` is load-bearing TWICE: it is ordinary
/// convergence, and it is what ADOPTS the policies the agents wrote — one apply, identical bytes,
/// no observable change to the object beyond `managedFields`.
///
/// Skipped when the body hashes to what this process last applied under this name, less than
/// `APPLY_RESYNC` ago.
/// ponytail: the memory is per-process and time-bounded, not watch-driven — any path that changes
/// a policy OUTSIDE `ensure` (every delete below) must `forget_applied` it.
pub(crate) async fn ensure<K>(api: &Api<K>, obj: &K, ctx: &Ctx) -> Result<(), ReconcileErr>
where
    K: Resource + Clone + serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
    K::DynamicType: Default,
{
    let name = obj.meta().name.clone().ok_or_else(|| ReconcileErr("child object has no name".into()))?;
    let key = applied_key(&K::kind(&Default::default()), obj.meta().namespace.as_deref(), &name);
    let hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::hash::DefaultHasher::new();
        serde_json::to_vec(obj).map_err(|e| ReconcileErr(e.to_string()))?.hash(&mut h);
        h.finish()
    };
    let fresh = ctx
        .applied
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&key)
        .is_some_and(|(h, at)| *h == hash && at.elapsed() < APPLY_RESYNC);
    if fresh {
        return Ok(());
    }
    api.patch(&name, &PatchParams::apply(crd::CONTROLLER_FIELD_MANAGER).force(), &Patch::Apply(obj)).await?;
    ctx.applied.lock().unwrap_or_else(|p| p.into_inner()).insert(key, (hash, Instant::now()));
    Ok(())
}

fn applied_key(kind: &str, ns: Option<&str>, name: &str) -> String {
    format!("{kind}/{}/{name}", ns.unwrap_or_default())
}

/// Drop `ensure`'s memory of one child, so the next pass applies it again whatever the hash says.
/// Its absence after a DELETE is F1 exactly: the policy was unrecreatable for `APPLY_RESYNC`.
pub(crate) fn forget_applied(ctx: &Ctx, kind: &str, ns: &str, name: &str) {
    ctx.applied.lock().unwrap_or_else(|p| p.into_inner()).remove(&applied_key(kind, Some(ns), name));
}

/// Delete, treating "already gone" as done — and forget the apply in the same call, so no caller
/// can do one without the other.
pub(crate) async fn drop_object<K>(api: &Api<K>, ctx: &Ctx, ns: &str, name: &str) -> Result<(), ReconcileErr>
where
    K: Resource<DynamicType = ()> + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    forget_applied(ctx, &K::kind(&()), ns, name);
    match api.delete(name, &Default::default()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(s)) if s.code == 404 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
use kube::runtime::watcher;

#[cfg(test)]
impl Ctx {
    /// A Ctx over whatever client the test supplies — a canned one for the election beat, an
    /// unrouted one for the epoch guard, which touches no API server.
    pub(crate) fn for_test_with(client: kube::Client) -> Ctx {
        Ctx::new(client, "ctl-test".into(), "test".into(), LiveSettings::new(AgentSettings::from_env()))
    }

    /// Seed a store as a finished initial list would. Applying `InitDone` is what makes the store
    /// READY, so a test that wants the unknown path simply does not call this.
    pub(crate) fn remember<K>(w: &Mutex<Option<Writer<K>>>, items: Vec<K>)
    where
        K: Resource<DynamicType = ()> + Clone + 'static,
    {
        if let Some(w) = w.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
            w.apply_watcher_event(&watcher::Event::Init);
            for i in items {
                w.apply_watcher_event(&watcher::Event::InitApply(i));
            }
            w.apply_watcher_event(&watcher::Event::InitDone);
        }
    }

    pub(crate) fn remember_spaces(&self, spaces: Vec<crd::SpaceEnvironment>) {
        Ctx::remember(&self.space_writer, spaces);
    }

    pub(crate) fn remember_environments(&self, envs: Vec<crd::Environment>) {
        Ctx::remember(&self.environment_writer, envs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The holder identity is the POD NAME, which is what makes "who holds the lease" answerable
    /// from `kubectl get lease` alone. An empty `POD_NAME` is a boot failure, not a blank holder:
    /// two pods with the same (empty) identity would each read the other's lease as their own and
    /// both write.
    #[test]
    fn a_blank_holder_is_refused() {
        assert!(Config::check("centralindia-k3s", "kloudlite-controller-abc").is_ok());
        assert!(Config::check("centralindia-k3s", "").is_err());
        assert!(Config::check("", "kloudlite-controller-abc").is_err());
    }

    /// The epoch guard is a process-wide fact, not a parameter threaded through every call site:
    /// a write path asks the Ctx.
    ///
    /// `tokio::test` only because building a `kube::Client` — even the canned one — needs a
    /// reactor; nothing under test here touches it.
    #[tokio::test]
    async fn the_ctx_remembers_the_term_it_was_elected_under() {
        let ctx = Ctx::for_test_with(kloudlite_workspaces::kube_test::mock_client(vec![]).0);
        assert_eq!(ctx.epoch(), 0);
        assert!(!ctx.leading());
        ctx.promote(4);
        assert_eq!(ctx.epoch(), 4);
        assert!(ctx.leading());
        ctx.demote("fenced");
        assert!(!ctx.leading());
    }
}
