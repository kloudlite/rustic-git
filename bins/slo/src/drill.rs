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
const DRILL_ORIGINALS: &str = "kloudlite.io/slo-drill-originals";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrillAction {
    Cordon,
    Decommission,
    Both,
}

impl DrillAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cordon => "cordon",
            Self::Decommission => "decommission",
            Self::Both => "both",
        }
    }

    fn from_str(value: Option<&str>) -> Option<Self> {
        match value {
            Some("cordon") => Some(Self::Cordon),
            Some("decommission") => Some(Self::Decommission),
            Some("both") => Some(Self::Both),
            _ => None,
        }
    }
}

/// What a node is carrying from some drill: the taint's value and the label's, each `Some` only
/// when that mark is actually on the node. Both are a run id (`run-{suite}-{unix}`), which is what
/// lets a sweep recognise its own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mark {
    pub node: String,
    pub taint: Option<String>,
    pub label: Option<String>,
    pub action: Option<DrillAction>,
    pub original_taint: Option<String>,
    pub original_cordon: Option<bool>,
    pub original_decommission: Option<String>,
    pub original_decommission_status: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct RestorePlan {
    cordon: Option<bool>,
    decommission: Option<Option<String>>,
}

fn restore_plan_for(action: DrillAction, current_cordon: bool, current_decommission: Option<&str>, original_cordon: bool, original_decommission: Option<&str>) -> RestorePlan {
    RestorePlan {
        cordon: matches!(action, DrillAction::Cordon | DrillAction::Both).then_some(current_cordon.then_some(original_cordon)).flatten(),
        decommission: if matches!(action, DrillAction::Decommission | DrillAction::Both) && current_decommission == Some("true") {
            Some(original_decommission.map(str::to_owned))
        } else {
            None
        },
    }
}

#[cfg(test)]
fn restore_plan(current_cordon: bool, current_decommission: Option<&str>, original_cordon: bool, original_decommission: Option<&str>) -> RestorePlan {
    restore_plan_for(DrillAction::Both, current_cordon, current_decommission, original_cordon, original_decommission)
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
    async fn mark_for(&self, node: &str, run: Option<&str>, _action: DrillAction) -> Result<()> {
        self.mark(node, run).await
    }
    async fn restore_taint(&self, node: &str, _run: &str) -> Result<()> {
        self.taint(node, None).await
    }
    async fn restore_marked(&self, node: &str, _run: &str, action: DrillAction) -> Result<()> {
        if matches!(action, DrillAction::Cordon | DrillAction::Both) {
            self.cordon(node, false).await?;
        }
        if matches!(action, DrillAction::Decommission | DrillAction::Both) {
            self.decommission(node, false).await?;
        }
        self.mark(node, None).await
    }
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
    undoing(cap, body, || k.restore_taint(node, run)).await
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
    k.mark_for(node, Some(run), DrillAction::Decommission).await?;
    k.decommission(node, true).await?;
    undoing(cap, body, || k.restore_marked(node, run, DrillAction::Decommission)).await
}

pub async fn with_cordon<T>(
    k: &dyn Cluster,
    node: &str,
    run: &str,
    cap: Duration,
    body: impl Future<Output = Result<T>>,
) -> Result<T> {
    k.mark_for(node, Some(run), DrillAction::Cordon).await?;
    k.cordon(node, true).await?;
    undoing(cap, body, || k.restore_marked(node, run, DrillAction::Cordon)).await
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
            match k.restore_taint(&m.node, m.taint.as_deref().unwrap_or_default()).await {
                Ok(()) => tracing::info!(kind = "taint", name = %m.node, "slo.drill.swept"),
                Err(e) => tracing::warn!(kind = "taint", name = %m.node, error = %format!("{e:#}"), "slo.drill.sweep.failed"),
            }
        }
        if !m.label.as_deref().is_some_and(&mine) {
            continue;
        }
        match k.restore_marked(&m.node, m.label.as_deref().unwrap_or_default(), m.action.unwrap_or(DrillAction::Both)).await {
            Ok(()) => tracing::info!(kind = "marked-node", name = %m.node, "slo.drill.swept"),
            Err(e) => tracing::warn!(kind = "marked-node", name = %m.node, error = %format!("{e:#}"), "slo.drill.sweep.failed"),
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
        let resource_version = obj.metadata.resource_version.clone();
        let taints = obj.spec.and_then(|s| s.taints).unwrap_or_default();
        let at = taints.iter().position(|t| t.key == DRILL_TAINT);
        if let (Some(run), Some(index)) = (run, at) {
            if taints[index].value.as_deref() != Some(run) {
                return Err(anyhow!("node {node} is marked by another drill"));
            }
        }
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
        let ops = if run.is_some() {
            let resource_version = resource_version.ok_or_else(|| anyhow!("node {node} has no resourceVersion"))?;
            let mut operations = ops.as_array().cloned().ok_or_else(|| anyhow!("invalid node taint patch"))?;
            operations.insert(0, json!({ "op": "test", "path": "/metadata/resourceVersion", "value": resource_version }));
            serde_json::Value::Array(operations)
        } else {
            ops
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
        self.mark_for(node, run, DrillAction::Both).await
    }

    async fn mark_for(&self, node: &str, run: Option<&str>, action: DrillAction) -> Result<()> {
        use kloudlite_workspaces::crd::{DECOMMISSION_LABEL, DECOMMISSION_STATUS};
        let api: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(self.clone());
        let current = api.get(node).await?;
        if let Some(run) = run {
            if current.metadata.labels.as_ref().and_then(|labels| labels.get(DRILL_TAINT)).is_some_and(|value| value != run) {
                return Err(anyhow!("node {node} is marked by another drill"));
            }
        }
        let value = run.map(Value::from).unwrap_or(Value::Null);
        let annotations = match run {
            Some(run) => {
                let existing = current
                    .metadata
                    .annotations
                    .as_ref()
                    .and_then(|values| values.get(DRILL_ORIGINALS));
                if let Some(existing) = existing {
                    if let Ok(originals) = serde_json::from_str::<Value>(existing) {
                        if originals.get("run").and_then(Value::as_str) == Some(run) {
                            if DrillAction::from_str(originals.get("action").and_then(Value::as_str)) != Some(action) {
                                return Err(anyhow!("node {node} is already marked for another drill action"));
                            }
                            json!({ DRILL_ORIGINALS: existing })
                        } else {
                            return Err(anyhow!("node {node} drill originals belong to another run"));
                        }
                    } else {
                        return Err(anyhow!("node {node} has invalid drill originals"));
                    }
                } else {
                let original_taint = current
                    .spec
                    .as_ref()
                    .and_then(|spec| spec.taints.as_ref())
                    .and_then(|taints| taints.iter().find(|taint| taint.key == DRILL_TAINT))
                    .and_then(|taint| taint.value.clone());
                json!({
                    DRILL_ORIGINALS: json!({
                        "run": run,
                        "action": action.as_str(),
                        "taint": original_taint,
                        "cordon": current.spec.as_ref().and_then(|spec| spec.unschedulable).unwrap_or(false),
                        "decommission": current.metadata.labels.as_ref().and_then(|labels| labels.get(DECOMMISSION_LABEL)),
                        "decommission_status": current.metadata.annotations.as_ref().and_then(|annotations| annotations.get(DECOMMISSION_STATUS)),
                    }).to_string(),
                })
                }
            }
            None => json!({ DRILL_ORIGINALS: Value::Null }),
        };
        match run {
            Some(_) => {
                let original = annotations.get(DRILL_ORIGINALS).cloned().ok_or_else(|| anyhow!("node {node} has no drill originals"))?;
                let resource_version = current.metadata.resource_version.as_deref().ok_or_else(|| anyhow!("node {node} has no resourceVersion"))?;
                let mut operations = vec![json!({ "op": "test", "path": "/metadata/resourceVersion", "value": resource_version })];
                if current.metadata.labels.is_none() {
                    operations.push(json!({ "op": "add", "path": "/metadata/labels", "value": {} }));
                }
                operations.push(json!({ "op": "add", "path": "/metadata/labels/kloudlite.io~1slo-drill", "value": value }));
                if current.metadata.annotations.is_none() {
                    operations.push(json!({ "op": "add", "path": "/metadata/annotations", "value": {} }));
                }
                operations.push(json!({ "op": "add", "path": "/metadata/annotations/kloudlite.io~1slo-drill-originals", "value": original }));
                api.patch(node, &kube::api::PatchParams::default(), &kube::api::Patch::Json::<()>(serde_json::from_value(serde_json::Value::Array(operations))?)).await?;
            }
            None => {
                api.patch(
                    node,
                    &kube::api::PatchParams::default(),
                    &kube::api::Patch::Merge(&json!({ "metadata": { "labels": { DRILL_TAINT: value }, "annotations": annotations } })),
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn restore_taint(&self, node: &str, run: &str) -> Result<()> {
        let api: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(self.clone());
        let current = api.get(node).await?;
        let owned = current
            .spec
            .as_ref()
            .and_then(|spec| spec.taints.as_ref())
            .and_then(|taints| taints.iter().find(|taint| taint.key == DRILL_TAINT))
            .and_then(|taint| taint.value.as_deref())
            .is_some_and(|value| value == run);
        if !owned {
            Ok(())
        } else {
            let index = current
                .spec
                .as_ref()
                .and_then(|spec| spec.taints.as_ref())
                .and_then(|taints| taints.iter().position(|taint| taint.key == DRILL_TAINT && taint.value.as_deref() == Some(run)))
                .ok_or_else(|| anyhow!("node {node} drill taint changed during cleanup"))?;
            let operations = json!([
                { "op": "test", "path": "/metadata/resourceVersion", "value": current.metadata.resource_version.as_deref().unwrap_or_default() },
                { "op": "test", "path": format!("/spec/taints/{index}/key"), "value": DRILL_TAINT },
                { "op": "test", "path": format!("/spec/taints/{index}/value"), "value": run },
                { "op": "remove", "path": format!("/spec/taints/{index}") },
            ]);
            api.patch(node, &kube::api::PatchParams::default(), &kube::api::Patch::Json::<()>(serde_json::from_value(operations)?)).await?;
            Ok(())
        }
    }

    async fn restore_marked(&self, node: &str, run: &str, _action: DrillAction) -> Result<()> {
        use kloudlite_workspaces::crd::{DECOMMISSION_LABEL, DECOMMISSION_STATUS, DRAINED_PREFIX};
        let api: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(self.clone());
        let current = api.get(node).await?;
        let labels = current.metadata.labels.as_ref();
        if !labels.and_then(|values| values.get(DRILL_TAINT)).is_some_and(|value| value == run) {
            return Ok(());
        }
        let originals = current
            .metadata
            .annotations
            .as_ref()
            .and_then(|values| values.get(DRILL_ORIGINALS))
            .and_then(|value| serde_json::from_str::<Value>(value).ok())
            .filter(|value| value.get("run").and_then(Value::as_str) == Some(run))
            .ok_or_else(|| anyhow!("node {node} has no original drill state for {run}"))?;
        let action = DrillAction::from_str(originals.get("action").and_then(Value::as_str))
            .ok_or_else(|| anyhow!("node {node} has no valid drill action for {run}"))?;
        let cordon_was = originals.get("cordon").and_then(Value::as_bool).unwrap_or(false);
        let decommission_was = originals.get("decommission").and_then(Value::as_str).map(str::to_owned);
        let decommission_status_was = originals.get("decommission_status").and_then(Value::as_str).map(str::to_owned);
        let current_cordon = current.spec.as_ref().and_then(|spec| spec.unschedulable).unwrap_or(false);
        let current_decommission = labels.and_then(|values| values.get(DECOMMISSION_LABEL)).map(String::as_str);
        let current_decommission_status = current
            .metadata
            .annotations
            .as_ref()
            .and_then(|values| values.get(DECOMMISSION_STATUS))
            .map(String::as_str);
        let plan = restore_plan_for(action, current_cordon, current_decommission, cordon_was, decommission_was.as_deref());
        let marker_path = "/metadata/labels/kloudlite.io~1slo-drill";
        let originals_path = "/metadata/annotations/kloudlite.io~1slo-drill-originals";
        let mut operations = vec![
            json!({ "op": "test", "path": marker_path, "value": run }),
        ];
        if let Some(resource_version) = current.metadata.resource_version.as_deref() {
            operations.push(json!({ "op": "test", "path": "/metadata/resourceVersion", "value": resource_version }));
        }
        if let Some(cordon) = plan.cordon {
            operations.extend([
                json!({ "op": "test", "path": "/spec/unschedulable", "value": true }),
                json!({ "op": "replace", "path": "/spec/unschedulable", "value": cordon }),
            ]);
        }
        if let Some(decommission) = plan.decommission {
            operations.push(json!({ "op": "test", "path": "/metadata/labels/kloudlite.io~1decommission", "value": "true" }));
            match decommission {
                Some(value) => operations.push(json!({ "op": "replace", "path": "/metadata/labels/kloudlite.io~1decommission", "value": value })),
                None => operations.push(json!({ "op": "remove", "path": "/metadata/labels/kloudlite.io~1decommission" })),
            }
            if current_decommission_status.is_some_and(|status| status.starts_with("draining ") || status.starts_with(DRAINED_PREFIX)) {
                operations.push(json!({ "op": "test", "path": "/metadata/annotations/kloudlite.io~1decommission-status", "value": current_decommission_status }));
                match decommission_status_was {
                    Some(value) => operations.push(json!({ "op": "replace", "path": "/metadata/annotations/kloudlite.io~1decommission-status", "value": value })),
                    None => operations.push(json!({ "op": "remove", "path": "/metadata/annotations/kloudlite.io~1decommission-status" })),
                }
            }
        }
        operations.extend([
            json!({ "op": "remove", "path": marker_path }),
            json!({ "op": "remove", "path": originals_path }),
        ]);
        api.patch(node, &kube::api::PatchParams::default(), &kube::api::Patch::Json::<()>(serde_json::from_value(serde_json::Value::Array(operations))?)).await?;
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
                let originals = n
                    .metadata
                    .annotations
                    .as_ref()
                    .and_then(|values| values.get(DRILL_ORIGINALS))
                    .and_then(|value| serde_json::from_str::<Value>(value).ok());
                (taint.is_some() || label.is_some()).then(|| Mark {
                    node: kube::ResourceExt::name_any(n),
                    taint,
                    label,
                    action: originals.as_ref().and_then(|value| DrillAction::from_str(value.get("action").and_then(Value::as_str))),
                    original_taint: originals.as_ref().and_then(|value| value.get("taint")).and_then(Value::as_str).map(str::to_owned),
                    original_cordon: originals.as_ref().and_then(|value| value.get("cordon")).and_then(Value::as_bool),
                    original_decommission: originals.as_ref().and_then(|value| value.get("decommission")).and_then(Value::as_str).map(str::to_owned),
                    original_decommission_status: originals.as_ref().and_then(|value| value.get("decommission_status")).and_then(Value::as_str).map(str::to_owned),
                })
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

    #[test]
    fn restore_plan_preserves_operator_changes_per_field() {
        assert_eq!(
            restore_plan(false, Some("operator"), false, Some("true")),
            RestorePlan { cordon: None, decommission: None },
        );
        assert_eq!(
            restore_plan(true, Some("true"), false, None),
            RestorePlan { cordon: Some(false), decommission: Some(None) },
        );
    }

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
            Mark { node: "node-a".into(), taint: Some(RUN.into()), label: None, ..Default::default() },
            Mark { node: "node-b".into(), taint: None, label: Some(RUN.into()), action: Some(DrillAction::Cordon), ..Default::default() },
        ];
        sweep_nodes(&k, &|v| v == RUN).await;
        assert_eq!(
            k.calls(),
            ["taint node-a -", "cordon node-b false", "mark node-b -"]
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
            ..Default::default()
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
