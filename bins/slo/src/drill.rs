//! The destructive drills' one rule: whatever a drill does to the fleet, the drill undoes.
//!
//! A drill is the only thing in this probe that BREAKS the system on purpose — it taints a node,
//! cordons one, cuts the fleet off from Redis. Every one of those is a fleet left worse than the
//! probe found it if the middle step errors, times out, or simply fails, and "the monthly probe
//! left a node tainted" is an outage nobody would think to look for. So the three mutations are
//! paired here rather than at their call sites, around a body that may do anything.
//!
//! Every mark a drill leaves NAMES THE RUN that left it (2026-09-12): the taint's value, the
//! node label and the NetworkPolicy's name all carry `run-{run_id}`, so a sweep can tell its own
//! litter from a drill another suite is in the middle of. Before that the sweep untainted and
//! uncordoned blind, and a fast run's teardown could undo a weekly drill's cordon while the
//! weekly step was still inside it.
//!
//! They sit behind a trait for one reason: `drills_always_undo` has to watch the pairing hold when
//! the middle errors, and a real API server cannot be asked to fail on demand.

use std::future::Future;
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

/// The drill taint, and the label a cordon or a decommission is written down with. `NoExecute`
/// because the drill is pretending the node died: a `NoSchedule` taint would leave every pod
/// already on it running, which is the one thing a dead node does not do.
///
/// One string for both: a node carries at most one drill's marks, and a reader that finds either
/// wants the same answer — which run put it there.
pub const DRILL_TAINT: &str = "kloudlite.io/slo-drill";

/// What a node is carrying from some drill: the taint's value and the label's, each `Some` only
/// when that mark is actually on the node. Both are a run id (`run-{suite}-{unix}`), which is what
/// lets a sweep recognise its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    pub node: String,
    pub taint: Option<String>,
    pub label: Option<String>,
}

/// The fleet mutations a drill makes, and nothing else. Each takes the run that is making it —
/// `Some(run)` sets, `None` clears — so the undo is the same call with the run dropped and a
/// separate `untaint` method is not a second place to get wrong.
#[async_trait]
pub trait Cluster: Send + Sync {
    async fn taint(&self, node: &str, run: Option<&str>) -> Result<()>;
    async fn cordon(&self, node: &str, on: bool) -> Result<()>;
    /// The product's own drain verb: `kloudlite.io/decommission=true`, the label the agent watches
    /// and `peer::unplaceable` reads. A CORDON is invisible to placement — the first live weekly
    /// run cordoned a node, killed the pod on it and watched nothing reschedule for 148 s — so a
    /// drill that wants a worktree to move must set this, not `spec.unschedulable`.
    async fn decommission(&self, node: &str, on: bool) -> Result<()>;
    /// The `kloudlite.io/slo-drill={run}` LABEL. A cordon and a decommission are both states an
    /// operator reaches by hand, so unlike the taint they cannot be recognised from their own
    /// shape — this label is what says "a drill did this, and here is which run". It replaced an
    /// emptyDir file (2026-09-12): the file lived in the pod whose death is the exact case the
    /// sweep exists for, and it named no run, so any run's teardown undid any run's cordon.
    async fn mark(&self, node: &str, run: Option<&str>) -> Result<()>;
    /// `Some(spec)` creates the NetworkPolicy, `None` deletes it.
    async fn netpol(&self, ns: &str, name: &str, spec: Option<Value>) -> Result<()>;
    async fn netpol_names(&self, ns: &str) -> Result<Vec<String>>;
    /// Every node carrying either drill mark. The sweep's input, and the half that needs no
    /// memory: a drill whose pod was killed between the mark and the undo left nothing behind
    /// except the marks themselves, and the marks name their run.
    async fn drill_marks(&self) -> Result<Vec<Mark>>;
}

/// The minute every `undoing` step's ceiling adds on top of its body cap. `Ctx::step` drops the
/// whole future when ITS timeout fires, undo included, so the body's own ceiling must always be
/// the one that fires first — with this much room left for the undo to run in.
pub const UNDO_SLACK: u64 = 60;

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
        Err(_) => Err(anyhow!("timed out after {} ms", cap.as_millis())),
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
    run: &str,
    cap: Duration,
    body: impl Future<Output = Result<T>>,
) -> Result<T> {
    k.taint(node, Some(run)).await?;
    undoing(cap, body, || k.taint(node, None)).await
}

/// Make a node unplaceable the way the platform does, for the length of `body`.
///
/// The label goes on FIRST and comes off LAST, exactly as the cordon's does: a drill killed
/// between the two mutations must leave the marked state, never the unmarked one, or the sweep
/// has nothing to find.
pub async fn with_decommission<T>(
    k: &dyn Cluster,
    node: &str,
    run: &str,
    cap: Duration,
    body: impl Future<Output = Result<T>>,
) -> Result<T> {
    k.mark(node, Some(run)).await?;
    k.decommission(node, true).await?;
    undoing(cap, body, || async {
        k.decommission(node, false).await?;
        k.mark(node, None).await
    })
    .await
}

pub async fn with_cordon<T>(
    k: &dyn Cluster,
    node: &str,
    run: &str,
    cap: Duration,
    body: impl Future<Output = Result<T>>,
) -> Result<T> {
    k.mark(node, Some(run)).await?;
    k.cordon(node, true).await?;
    undoing(cap, body, || async {
        k.cordon(node, false).await?;
        k.mark(node, None).await
    })
    .await
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

/// Undo the node mutations THIS run's marks name, whatever this run did or did not get to.
///
/// Teardown runs this on EVERY run, not only the monthly one: a drill's own undo is the first
/// thing a killed pod loses, and a node left tainted or cordoned is an outage nobody would think
/// to look for. `mine` is what keeps that from becoming the opposite bug — a fast run's teardown
/// lifting the cordon a weekly drill is standing inside — so it answers true only for this run's
/// own id and for one old enough that no run can still be holding it. Best effort and logged
/// throughout: teardown's job is the report, and an error propagated from here would lose it.
pub async fn sweep_nodes(k: &dyn Cluster, mine: &dyn Fn(&str) -> bool) {
    let marks = match k.drill_marks().await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(op = "list", error = %format!("{e:#}"), "slo.drill.sweep.failed");
            return;
        }
    };
    for m in marks {
        if m.taint.as_deref().is_some_and(&mine) {
            match k.taint(&m.node, None).await {
                Ok(()) => tracing::info!(kind = "taint", name = %m.node, "slo.drill.swept"),
                Err(e) => tracing::warn!(kind = "taint", name = %m.node, error = %format!("{e:#}"), "slo.drill.sweep.failed"),
            }
        }
        if !m.label.as_deref().is_some_and(&mine) {
            continue;
        }
        // Both, blind: the one label records either mutation, and a decommission label left on a
        // node is a node placement will never use again.
        for (kind, out) in [
            ("cordon", k.cordon(&m.node, false).await),
            ("decommission", k.decommission(&m.node, false).await),
            ("mark", k.mark(&m.node, None).await),
        ] {
            match out {
                Ok(()) => tracing::info!(kind, name = %m.node, "slo.drill.swept"),
                Err(e) => tracing::warn!(kind, name = %m.node, error = %format!("{e:#}"), "slo.drill.sweep.failed"),
            }
        }
    }
}

/// The same rule for the policies: delete only the ones whose NAME this run owns.
pub async fn sweep_netpols(k: &dyn Cluster, ns: &str, mine: &dyn Fn(&str) -> bool) {
    let names = match k.netpol_names(ns).await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(op = "list", error = %format!("{e:#}"), "slo.drill.sweep.failed");
            return;
        }
    };
    for name in names.into_iter().filter(|n| mine(n)) {
        match k.netpol(ns, &name, None).await {
            Ok(()) => tracing::info!(kind = "netpol", name = %name, "slo.drill.swept"),
            Err(e) => tracing::warn!(kind = "netpol", name = %name, error = %format!("{e:#}"), "slo.drill.sweep.failed"),
        }
    }
}

/// An EXPLICIT in-cluster client. `Ctx::kube` follows `KUBECONFIG` and lands in k3s, where none of
/// the server tier's pods run — every caller here wants the AKS cluster the probe is running in.
/// Whether a NetworkPolicy is enforced here at all. A policy engine is a DaemonSet in kube-system
/// (Azure NPM `azure-npm`, Cilium `cilium`, Calico `calico-node`); without one the API server
/// accepts the object and nothing reads it — `networkProfile.networkPolicy: none` on AKS — so a
/// deny drill cuts nothing and reports what the fleet did with the dependency UP (2026-09-06:
/// `drill.redis.down` saw pull events arrive over the "denied" stream). Skipping is the honest
/// verdict; a pass would be the false green this probe exists to prevent.
pub async fn netpol_enforced(k: &kube::Client) -> Result<bool> {
    use kube::ResourceExt;
    let api: kube::Api<k8s_openapi::api::apps::v1::DaemonSet> = kube::Api::namespaced(k.clone(), "kube-system");
    let names: Vec<String> =
        api.list(&kube::api::ListParams::default()).await.map_err(|e| anyhow!("could not list kube-system DaemonSets: {e}"))?.items.iter().map(|d| d.name_any()).collect();
    Ok(names.iter().any(|n| n == "azure-npm" || n.starts_with("cilium") || n == "calico-node" || n.starts_with("kube-router")))
}

/// The skip reason both deny drills give when `netpol_enforced` says no.
pub const NETPOL_UNENFORCED: &str =
    "NetworkPolicy is not enforced on this cluster (no policy engine in kube-system): the deny would cut nothing";

pub fn incluster() -> Result<kube::Client> {
    let cfg = kube::Config::incluster().map_err(|e| anyhow!("{e}"))?;
    kube::Client::try_from(cfg).map_err(|e| anyhow!("{e}"))
}

/// The real thing: a `kube::Client` for whichever cluster the drill belongs to — k3s for the node
/// drills, the in-cluster AKS one for the Redis policy.
#[async_trait]
impl Cluster for kube::Client {
    async fn taint(&self, node: &str, run: Option<&str>) -> Result<()> {
        // A JSON patch of the ONE taint, not a merge patch of the whole array (2026-09-12): the
        // array has no per-taint address, so the old read-then-write rewrote every taint on the
        // node from a list it had read a moment earlier — a `NoSchedule` somebody else added in
        // between was silently dropped. The `test` makes the removal fail rather than take the
        // wrong element when the list has moved under us.
        let api: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(self.clone());
        let obj = api.get(node).await?;
        let taints = obj.spec.and_then(|s| s.taints).unwrap_or_default();
        let at = taints.iter().position(|t| t.key == DRILL_TAINT);
        let ops = match (run, at) {
            (Some(run), _) => {
                let one = json!({ "key": DRILL_TAINT, "value": run, "effect": "NoExecute" });
                match at {
                    // Replacing in place keeps the index stable for a concurrent reader.
                    Some(i) => json!([{ "op": "replace", "path": format!("/spec/taints/{i}"), "value": one }]),
                    None if taints.is_empty() => json!([{ "op": "add", "path": "/spec/taints", "value": [one] }]),
                    None => json!([{ "op": "add", "path": "/spec/taints/-", "value": one }]),
                }
            }
            // Nothing to remove is the state the undo wanted.
            (None, None) => return Ok(()),
            (None, Some(i)) => json!([
                { "op": "test", "path": format!("/spec/taints/{i}/key"), "value": DRILL_TAINT },
                { "op": "remove", "path": format!("/spec/taints/{i}") },
            ]),
        };
        api.patch(node, &kube::api::PatchParams::default(), &kube::api::Patch::Json::<()>(
            serde_json::from_value(ops)?,
        ))
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

    async fn decommission(&self, node: &str, on: bool) -> Result<()> {
        use kloudlite_workspaces::crd::{DECOMMISSION_LABEL, DECOMMISSION_STATUS};
        let api: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(self.clone());
        // `null` REMOVES a label in a merge patch, which is what the undo needs — an empty string
        // would leave the key there, and `unplaceable` reads the key.
        let value = if on { serde_json::json!("true") } else { Value::Null };
        // The undo takes the `draining …` stamp with the label, exactly as the admin's `undrain`
        // does: the agent writes that annotation only while labelled and clears nothing on its
        // own, so a drill that removed the label alone left "draining running=2 …" on a node that
        // was not draining — the same stale stamp the admin route exists to prevent.
        let patch = match on {
            true => json!({ "metadata": { "labels": { DECOMMISSION_LABEL: value } } }),
            false => json!({ "metadata": { "labels": { DECOMMISSION_LABEL: value },
                                            "annotations": { DECOMMISSION_STATUS: Value::Null } } }),
        };
        api.patch(node, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(&patch)).await?;
        Ok(())
    }

    async fn mark(&self, node: &str, run: Option<&str>) -> Result<()> {
        let api: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(self.clone());
        let value = run.map(Value::from).unwrap_or(Value::Null);
        api.patch(
            node,
            &kube::api::PatchParams::default(),
            &kube::api::Patch::Merge(&json!({ "metadata": { "labels": { DRILL_TAINT: value } } })),
        )
        .await?;
        Ok(())
    }

    async fn drill_marks(&self) -> Result<Vec<Mark>> {
        let api: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(self.clone());
        let list = api.list(&kube::api::ListParams::default()).await?;
        Ok(list
            .items
            .iter()
            .filter_map(|n| {
                let taint = n
                    .spec
                    .as_ref()
                    .and_then(|s| s.taints.as_ref())
                    .and_then(|t| t.iter().find(|t| t.key == DRILL_TAINT))
                    .map(|t| t.value.clone().unwrap_or_default());
                let label = n.metadata.labels.as_ref().and_then(|l| l.get(DRILL_TAINT)).cloned();
                (taint.is_some() || label.is_some())
                    .then(|| Mark { node: kube::ResourceExt::name_any(n), taint, label })
            })
            .collect())
    }

    async fn netpol_names(&self, ns: &str) -> Result<Vec<String>> {
        let api: kube::Api<k8s_openapi::api::networking::v1::NetworkPolicy> =
            kube::Api::namespaced(self.clone(), ns);
        Ok(api
            .list(&kube::api::ListParams::default())
            .await?
            .items
            .iter()
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
        /// What `drill_marks` answers — the sweep's input, set by a test rather than by a drill.
        pub marks: Mutex<Vec<Mark>>,
        pub policies: Mutex<Vec<String>>,
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
        async fn taint(&self, node: &str, run: Option<&str>) -> Result<()> {
            self.record(format!("taint {node} {}", run.unwrap_or("-")));
            Ok(())
        }
        async fn drill_marks(&self) -> Result<Vec<Mark>> {
            Ok(self.marks.lock().expect("lock").clone())
        }
        async fn cordon(&self, node: &str, on: bool) -> Result<()> {
            self.record(format!("cordon {node} {on}"));
            Ok(())
        }
        async fn decommission(&self, node: &str, on: bool) -> Result<()> {
            self.record(format!("decommission {node} {on}"));
            Ok(())
        }
        async fn mark(&self, node: &str, run: Option<&str>) -> Result<()> {
            self.record(format!("mark {node} {}", run.unwrap_or("-")));
            Ok(())
        }
        async fn netpol(&self, ns: &str, name: &str, spec: Option<Value>) -> Result<()> {
            self.record(format!("netpol {ns}/{name} {}", spec.is_some()));
            Ok(())
        }
        async fn netpol_names(&self, _ns: &str) -> Result<Vec<String>> {
            Ok(self.policies.lock().expect("lock").clone())
        }
    }

    const RUN: &str = "run-monthly-1000";

    /// The whole contract of this module, and the reason it exists as one: a drill whose middle
    /// step FAILS still leaves the fleet as it found it. Written against a failing body on purpose
    /// — the happy path is the one that would pass however the undo were wired.
    #[tokio::test]
    async fn drills_always_undo() {
        let k = FakeKube::default();
        let cap = Duration::from_secs(30);
        let boom = || async { Err::<(), _>(anyhow::anyhow!("the drill's middle step failed")) };

        for out in [
            with_taint(&k, "node-a", RUN, cap, boom()).await,
            with_cordon(&k, "node-a", RUN, cap, boom()).await,
            with_decommission(&k, "node-a", RUN, cap, boom()).await,
            with_netpol(&k, "kloudlite", "run-monthly-1000-redis", json!({}), cap, boom()).await,
        ] {
            // The BODY's failure is what comes back — the drill measured something and it failed.
            assert!(out.unwrap_err().to_string().contains("middle step"));
        }
        assert_eq!(
            k.calls(),
            [
                "taint node-a run-monthly-1000",
                "taint node-a -",
                "mark node-a run-monthly-1000",
                "cordon node-a true",
                "cordon node-a false",
                "mark node-a -",
                "mark node-a run-monthly-1000",
                "decommission node-a true",
                "decommission node-a false",
                "mark node-a -",
                "netpol kloudlite/run-monthly-1000-redis true",
                "netpol kloudlite/run-monthly-1000-redis false",
            ]
        );
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
                    with_taint(k.as_ref(), "node-a", RUN, Duration::from_secs(30), async {
                        tokio::time::sleep(Duration::from_secs(600)).await;
                        Ok(())
                    })
                    .await
                })
            })
            .await;
        assert!(!ok, "an overrunning drill is a failed sample");
        assert!(c.steps[0].detail.contains("timed out"), "{}", c.steps[0].detail);
        assert_eq!(k.calls(), ["taint node-a run-monthly-1000", "taint node-a -"]);
    }

    /// The other half of H2: a run that died mid-drill left no undo behind, so teardown does it —
    /// from the marks themselves, which is all a dead pod leaves.
    #[tokio::test]
    async fn teardown_sweeps_a_taint_and_a_cordon_a_dead_run_left() {
        let k = FakeKube::default();
        *k.marks.lock().expect("lock") = vec![
            Mark { node: "node-a".into(), taint: Some(RUN.into()), label: None },
            Mark { node: "node-b".into(), taint: None, label: Some(RUN.into()) },
        ];
        sweep_nodes(&k, &|v| v == RUN).await;
        assert_eq!(
            k.calls(),
            ["taint node-a -", "cordon node-b false", "decommission node-b false", "mark node-b -"]
        );
    }

    /// The bug the run id in every mark exists for: a fast run's teardown must NOT lift the cordon
    /// a weekly drill is standing inside, and must not untaint a node another run tainted a second
    /// ago. `mine` is the whole of that judgement, so it is asserted here on a foreign mark.
    #[tokio::test]
    async fn a_sweep_leaves_another_runs_marks_alone() {
        let k = FakeKube::default();
        *k.marks.lock().expect("lock") = vec![Mark {
            node: "node-a".into(),
            taint: Some("run-weekly-9999".into()),
            label: Some("run-weekly-9999".into()),
        }];
        *k.policies.lock().expect("lock") = vec!["run-weekly-9999-redis".into(), "run-fast-1-redis".into()];
        let mine = |v: &str| v.starts_with("run-fast-1");
        sweep_nodes(&k, &mine).await;
        sweep_netpols(&k, "kloudlite", &mine).await;
        assert_eq!(k.calls(), ["netpol kloudlite/run-fast-1-redis false"], "someone else's drill was undone");
    }

    /// A drill that worked and could not clean up after itself is NOT a pass: the fleet is left
    /// tainted, and reporting green would hide it until somebody wondered why a node was empty.
    #[tokio::test]
    async fn an_undo_that_fails_fails_the_drill() {
        struct Stuck;
        #[async_trait]
        impl Cluster for Stuck {
            async fn taint(&self, _: &str, run: Option<&str>) -> Result<()> {
                match run {
                    Some(_) => Ok(()),
                    None => Err(anyhow::anyhow!("the API server refused the untaint")),
                }
            }
            async fn cordon(&self, _: &str, _: bool) -> Result<()> {
                Ok(())
            }
            async fn decommission(&self, _: &str, _: bool) -> Result<()> {
                Ok(())
            }
            async fn mark(&self, _: &str, _: Option<&str>) -> Result<()> {
                Ok(())
            }
            async fn netpol(&self, _: &str, _: &str, _: Option<Value>) -> Result<()> {
                Ok(())
            }
            async fn netpol_names(&self, _: &str) -> Result<Vec<String>> {
                Ok(vec![])
            }
            async fn drill_marks(&self) -> Result<Vec<Mark>> {
                Ok(vec![])
            }
        }
        let e = with_taint(&Stuck, "node-a", RUN, Duration::from_secs(30), async { Ok(()) }).await.unwrap_err();
        assert!(format!("{e:#}").contains("could not undo itself"), "{e:#}");
    }
}
