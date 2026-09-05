//! The destructive drills' one rule: whatever a drill does to the fleet, the drill undoes.
//!
//! A drill is the only thing in this probe that BREAKS the system on purpose — it taints a node,
//! cordons one, cuts the fleet off from Redis. Every one of those is a fleet left worse than the
//! probe found it if the middle step errors, times out, or simply fails, and "the monthly probe
//! left a node tainted" is an outage nobody would think to look for. So the three mutations are
//! paired here rather than at their call sites, around a body that may do anything.
//!
//! They sit behind a trait for one reason: `drills_always_undo` has to watch the pairing hold when
//! the middle errors, and a real API server cannot be asked to fail on demand.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

/// The drill taint. `NoExecute` because the drill is pretending the node died: a `NoSchedule` taint
/// would leave every pod already on it running, which is the one thing a dead node does not do.
pub const DRILL_TAINT: &str = "kloudlite-git.io/slo-drill";

/// The three fleet mutations a drill makes, and nothing else. Each takes `on`, so the undo is the
/// same call with the flag flipped — a separate `untaint` method is a second place to get wrong.
#[async_trait]
pub trait Cluster: Send + Sync {
    async fn taint(&self, node: &str, on: bool) -> Result<()>;
    async fn cordon(&self, node: &str, on: bool) -> Result<()>;
    /// `Some(spec)` creates the NetworkPolicy, `None` deletes it.
    async fn netpol(&self, ns: &str, name: &str, spec: Option<Value>) -> Result<()>;
    /// Every node still carrying `DRILL_TAINT`. The sweep's half that needs no memory: a drill
    /// whose pod was killed between the taint and the untaint left no file behind, but it did
    /// leave the taint, and the taint names itself.
    async fn tainted_nodes(&self) -> Result<Vec<String>>;
}

/// Run `body` under `cap`, then run `undo` — on EVERY path out of it, the timeout included.
///
/// The `cap` is the whole reason this takes one: `Ctx::step` runs a step inside its own
/// `tokio::time::timeout`, and a timeout there DROPS the future — undo and all — leaving the fleet
/// tainted with nothing to say so. So the body's ceiling lives HERE, inside the cancellable region,
/// and every drill's step ceiling is this plus a minute so the outer one can never fire first.
///
/// A failed undo turns a passing body into a failure: a drill that proved the fleet heals and then
/// left a node tainted has not passed, it has broken something quietly. When both fail the BODY's
/// error is the one reported — that is what the drill was measuring — and the undo's is logged.
pub async fn undoing<T, B, U, UF>(cap: Duration, body: B, undo: U) -> Result<T>
where
    B: Future<Output = Result<T>>,
    U: FnOnce() -> UF,
    UF: Future<Output = Result<()>>,
{
    let out = match tokio::time::timeout(cap, body).await {
        Ok(out) => out,
        Err(_) => Err(anyhow!("the drill timed out after {} ms", cap.as_millis())),
    };
    match (out, undo().await) {
        (out, Ok(())) => out,
        (Ok(_), Err(e)) => Err(e.context("the drill could not undo itself")),
        (Err(body), Err(undo)) => {
            tracing::error!(error = %format!("{undo:#}"), "slo.drill.undo.failed");
            Err(body)
        }
    }
}

pub async fn with_taint<T>(
    k: &dyn Cluster,
    node: &str,
    cap: Duration,
    body: impl Future<Output = Result<T>>,
) -> Result<T> {
    k.taint(node, true).await?;
    undoing(cap, body, || k.taint(node, false)).await
}

/// `tmp` is where the cordon is WRITTEN DOWN before it is made — an `unschedulable` node looks
/// exactly like one an operator cordoned by hand, so unlike the taint it cannot be recognised
/// later. The parent process reads that file in teardown, which is the only thing that can clean
/// up after a child that died mid-drill.
pub async fn with_cordon<T>(
    k: &dyn Cluster,
    tmp: &Path,
    node: &str,
    cap: Duration,
    body: impl Future<Output = Result<T>>,
) -> Result<T> {
    note_cordon(tmp, node);
    k.cordon(node, true).await?;
    undoing(cap, body, || k.cordon(node, false)).await
}

pub async fn with_netpol<T>(
    k: &dyn Cluster,
    ns: &str,
    name: &str,
    spec: Value,
    cap: Duration,
    body: impl Future<Output = Result<T>>,
) -> Result<T> {
    k.netpol(ns, name, Some(spec)).await?;
    undoing(cap, body, || k.netpol(ns, name, None)).await
}

/// The nodes this run has cordoned, on disk. Best effort in both directions: a file that cannot be
/// written costs the sweep its second half, and the taint sweep still runs.
const CORDONED: &str = "drill.json";

fn note_cordon(tmp: &Path, node: &str) {
    let path = tmp.join(CORDONED);
    let mut nodes = cordoned(tmp);
    if !nodes.iter().any(|n| n == node) {
        nodes.push(node.to_string());
    }
    if let Err(e) = serde_json::to_vec(&nodes).map_err(|e| e.to_string()).and_then(|b| std::fs::write(&path, b).map_err(|e| e.to_string())) {
        tracing::warn!(op = "write", name = %path.display(), error = %e, "slo.drill.note.failed");
    }
}

fn cordoned(tmp: &Path) -> Vec<String> {
    std::fs::read(tmp.join(CORDONED))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Undo the two node mutations unconditionally, whatever this run did or did not get to.
///
/// Teardown runs this on EVERY run, not only the monthly one: a drill's own undo is the first
/// thing a killed pod loses, and a node left tainted or cordoned is an outage nobody would think
/// to look for. Best effort and logged throughout — teardown's job is the report, and an error
/// propagated from here would lose it.
pub async fn sweep_nodes(k: &dyn Cluster, tmp: &Path) {
    let tainted = match k.tainted_nodes().await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(op = "list", error = %format!("{e:#}"), "slo.drill.sweep.failed");
            vec![]
        }
    };
    for node in tainted {
        match k.taint(&node, false).await {
            Ok(()) => tracing::info!(kind = "taint", name = %node, "slo.drill.swept"),
            Err(e) => tracing::warn!(kind = "taint", name = %node, error = %format!("{e:#}"), "slo.drill.sweep.failed"),
        }
    }
    for node in cordoned(tmp) {
        match k.cordon(&node, false).await {
            Ok(()) => tracing::info!(kind = "cordon", name = %node, "slo.drill.swept"),
            Err(e) => tracing::warn!(kind = "cordon", name = %node, error = %format!("{e:#}"), "slo.drill.sweep.failed"),
        }
    }
}

/// An EXPLICIT in-cluster client. `Ctx::kube` follows `KUBECONFIG` and lands in k3s, where none of
/// the server tier's pods run — every caller here wants the AKS cluster the probe is running in.
pub fn incluster() -> Result<kube::Client> {
    let cfg = kube::Config::incluster().map_err(|e| anyhow!("{e}"))?;
    kube::Client::try_from(cfg).map_err(|e| anyhow!("{e}"))
}

/// The real thing: a `kube::Client` for whichever cluster the drill belongs to — k3s for the node
/// drills, the in-cluster AKS one for the Redis policy.
#[async_trait]
impl Cluster for kube::Client {
    async fn taint(&self, node: &str, on: bool) -> Result<()> {
        // A merge patch of the whole `taints` array, because that is the only shape the field has:
        // there is no per-taint address, so removing one means writing the list without it. The
        // read-then-write is safe here in a way it would not be for a controller — this is a drill
        // node nothing else is editing for the length of the drill.
        let api: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(self.clone());
        let obj = api.get(node).await?;
        let mut taints = obj.spec.and_then(|s| s.taints).unwrap_or_default();
        taints.retain(|t| t.key != DRILL_TAINT);
        if on {
            taints.push(k8s_openapi::api::core::v1::Taint {
                key: DRILL_TAINT.into(),
                value: Some("true".into()),
                effect: "NoExecute".into(),
                ..Default::default()
            });
        }
        api.patch(
            node,
            &kube::api::PatchParams::default(),
            &kube::api::Patch::Merge(&json!({ "spec": { "taints": taints } })),
        )
        .await?;
        Ok(())
    }

    async fn cordon(&self, node: &str, on: bool) -> Result<()> {
        let api: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(self.clone());
        api.patch(
            node,
            &kube::api::PatchParams::default(),
            &kube::api::Patch::Merge(&json!({ "spec": { "unschedulable": on } })),
        )
        .await?;
        Ok(())
    }

    async fn tainted_nodes(&self) -> Result<Vec<String>> {
        let api: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(self.clone());
        let list = api.list(&kube::api::ListParams::default()).await?;
        Ok(list
            .items
            .iter()
            .filter(|n| {
                n.spec
                    .as_ref()
                    .and_then(|s| s.taints.as_ref())
                    .is_some_and(|t| t.iter().any(|t| t.key == DRILL_TAINT))
            })
            .map(kube::ResourceExt::name_any)
            .collect())
    }

    async fn netpol(&self, ns: &str, name: &str, spec: Option<Value>) -> Result<()> {
        let api: kube::Api<k8s_openapi::api::networking::v1::NetworkPolicy> =
            kube::Api::namespaced(self.clone(), ns);
        match spec {
            Some(spec) => {
                // A plain create, not an apply: apply needs `patch` on every NetworkPolicy in the
                // namespace, and the `attach-{ws}` policies that carry a workspace's own traffic
                // live there. `create` unbounded plus `delete` on this ONE name is the narrowest
                // grant that does the job — the cost is that a policy a killed drill left behind
                // answers 409, which teardown's own sweep is what actually clears.
                let doc: k8s_openapi::api::networking::v1::NetworkPolicy =
                    serde_json::from_value(json!({
                        "apiVersion": "networking.k8s.io/v1",
                        "kind": "NetworkPolicy",
                        "metadata": { "name": name, "namespace": ns },
                        "spec": spec,
                    }))?;
                api.create(&kube::api::PostParams::default(), &doc).await?;
            }
            None => {
                // A policy that is not there is the state we wanted; teardown calls this blind.
                match api.delete(name, &kube::api::DeleteParams::default()).await {
                    Ok(_) => {}
                    Err(kube::Error::Api(e)) if e.code == 404 => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Every call, in order. A pair is balanced when the `on` and the `off` are both there.
    #[derive(Default)]
    pub struct FakeKube {
        pub calls: Mutex<Vec<String>>,
        /// What `tainted_nodes` answers — the sweep's input, set by a test rather than by a taint.
        pub tainted: Mutex<Vec<String>>,
    }

    impl FakeKube {
        fn record(&self, what: String) {
            self.calls.lock().expect("lock").push(what);
        }
        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("lock").clone()
        }
    }

    #[async_trait]
    impl Cluster for FakeKube {
        async fn taint(&self, node: &str, on: bool) -> Result<()> {
            self.record(format!("taint {node} {on}"));
            Ok(())
        }
        async fn tainted_nodes(&self) -> Result<Vec<String>> {
            Ok(self.tainted.lock().expect("lock").clone())
        }
        async fn cordon(&self, node: &str, on: bool) -> Result<()> {
            self.record(format!("cordon {node} {on}"));
            Ok(())
        }
        async fn netpol(&self, ns: &str, name: &str, spec: Option<Value>) -> Result<()> {
            self.record(format!("netpol {ns}/{name} {}", spec.is_some()));
            Ok(())
        }
    }

    /// The whole contract of this module, and the reason it exists as one: a drill whose middle
    /// step FAILS still leaves the fleet as it found it. Written against a failing body on purpose
    /// — the happy path is the one that would pass however the undo were wired.
    #[tokio::test]
    async fn drills_always_undo() {
        let k = FakeKube::default();
        let tmp = tmpdir("undo");
        let cap = Duration::from_secs(30);
        let boom = || async { Err::<(), _>(anyhow::anyhow!("the drill's middle step failed")) };

        for out in [
            with_taint(&k, "node-a", cap, boom()).await,
            with_cordon(&k, &tmp, "node-a", cap, boom()).await,
            with_netpol(&k, "kloudlite-git", "slo-drill-redis", json!({}), cap, boom()).await,
        ] {
            // The BODY's failure is what comes back — the drill measured something and it failed.
            assert!(out.unwrap_err().to_string().contains("middle step"));
        }
        assert_eq!(
            k.calls(),
            [
                "taint node-a true",
                "taint node-a false",
                "cordon node-a true",
                "cordon node-a false",
                "netpol kloudlite-git/slo-drill-redis true",
                "netpol kloudlite-git/slo-drill-redis false",
            ]
        );
    }

    fn tmpdir(what: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("slo-drill-{what}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("tmp");
        d
    }

    /// THE failure mode this module's `cap` exists for. `Ctx::step` runs a step inside its own
    /// timeout, and a timeout there DROPS the future — so a drill whose body overran used to lose
    /// its undo entirely and leave the node tainted with nothing recording it. The body here sleeps
    /// past every ceiling in sight; the untaint must still be on the fake when the step comes back.
    #[tokio::test(start_paused = true)]
    async fn a_body_that_outlives_its_ceiling_still_undoes() {
        let k = std::sync::Arc::new(FakeKube::default());
        let mut c = crate::testkit::ctx().await;
        let seen = k.clone();
        let ok = c
            .step("drill.dead.node", Duration::from_secs(120), move |_| {
                let k = seen.clone();
                Box::pin(async move {
                    // The drill's own ceiling is well inside the step's, which is the rule every
                    // caller follows: body cap + 60 s.
                    with_taint(k.as_ref(), "node-a", Duration::from_secs(30), async {
                        tokio::time::sleep(Duration::from_secs(600)).await;
                        Ok(())
                    })
                    .await
                })
            })
            .await;
        assert!(!ok, "an overrunning drill is a failed sample");
        assert!(c.steps[0].detail.contains("the drill timed out"), "{}", c.steps[0].detail);
        assert_eq!(k.calls(), ["taint node-a true", "taint node-a false"]);
    }

    /// The other half of H2: a run that died mid-drill left no undo behind, so teardown does it —
    /// the taint by its own key, the cordon from the file the drill wrote before it made one.
    #[tokio::test]
    async fn teardown_sweeps_a_taint_and_a_cordon_a_dead_run_left() {
        let k = FakeKube::default();
        *k.tainted.lock().expect("lock") = vec!["node-a".into()];
        let tmp = tmpdir("sweep");
        note_cordon(&tmp, "node-b");
        sweep_nodes(&k, &tmp).await;
        assert_eq!(k.calls(), ["taint node-a false", "cordon node-b false"]);
    }

    /// A drill that worked and could not clean up after itself is NOT a pass: the fleet is left
    /// tainted, and reporting green would hide it until somebody wondered why a node was empty.
    #[tokio::test]
    async fn an_undo_that_fails_fails_the_drill() {
        struct Stuck;
        #[async_trait]
        impl Cluster for Stuck {
            async fn taint(&self, _: &str, on: bool) -> Result<()> {
                if on {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("the API server refused the untaint"))
                }
            }
            async fn cordon(&self, _: &str, _: bool) -> Result<()> {
                Ok(())
            }
            async fn netpol(&self, _: &str, _: &str, _: Option<Value>) -> Result<()> {
                Ok(())
            }
            async fn tainted_nodes(&self) -> Result<Vec<String>> {
                Ok(vec![])
            }
        }
        let e = with_taint(&Stuck, "node-a", Duration::from_secs(30), async { Ok(()) }).await.unwrap_err();
        assert!(format!("{e:#}").contains("could not undo itself"), "{e:#}");
    }
}
