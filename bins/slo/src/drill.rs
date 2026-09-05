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

use anyhow::Result;
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
}

/// Run `body`, then run `undo` — on EVERY path out of it.
///
/// A failed undo turns a passing body into a failure: a drill that proved the fleet heals and then
/// left a node tainted has not passed, it has broken something quietly. When both fail the BODY's
/// error is the one reported — that is what the drill was measuring — and the undo's is logged.
pub async fn undoing<T, B, U, UF>(body: B, undo: U) -> Result<T>
where
    B: Future<Output = Result<T>>,
    U: FnOnce() -> UF,
    UF: Future<Output = Result<()>>,
{
    let out = body.await;
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
    body: impl Future<Output = Result<T>>,
) -> Result<T> {
    k.taint(node, true).await?;
    undoing(body, || k.taint(node, false)).await
}

pub async fn with_cordon<T>(
    k: &dyn Cluster,
    node: &str,
    body: impl Future<Output = Result<T>>,
) -> Result<T> {
    k.cordon(node, true).await?;
    undoing(body, || k.cordon(node, false)).await
}

pub async fn with_netpol<T>(
    k: &dyn Cluster,
    ns: &str,
    name: &str,
    spec: Value,
    body: impl Future<Output = Result<T>>,
) -> Result<T> {
    k.netpol(ns, name, Some(spec)).await?;
    undoing(body, || k.netpol(ns, name, None)).await
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

    async fn netpol(&self, ns: &str, name: &str, spec: Option<Value>) -> Result<()> {
        let api: kube::Api<k8s_openapi::api::networking::v1::NetworkPolicy> =
            kube::Api::namespaced(self.clone(), ns);
        match spec {
            Some(spec) => {
                // Server-side apply, not create: a policy a killed drill left behind must be
                // replaced rather than answered with a 409 that reads like a broken cluster.
                let doc = json!({
                    "apiVersion": "networking.k8s.io/v1",
                    "kind": "NetworkPolicy",
                    "metadata": { "name": name, "namespace": ns },
                    "spec": spec,
                });
                api.patch(
                    name,
                    &kube::api::PatchParams::apply("kloudlite-git-slo").force(),
                    &kube::api::Patch::Apply(&doc),
                )
                .await?;
            }
            None => {
                api.delete(name, &kube::api::DeleteParams::default()).await?;
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
        let boom = || async { Err::<(), _>(anyhow::anyhow!("the drill's middle step failed")) };

        for out in [
            with_taint(&k, "node-a", boom()).await,
            with_cordon(&k, "node-a", boom()).await,
            with_netpol(&k, "kloudlite-git", "slo-drill-redis", json!({}), boom()).await,
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
        }
        let e = with_taint(&Stuck, "node-a", async { Ok(()) }).await.unwrap_err();
        assert!(format!("{e:#}").contains("could not undo itself"), "{e:#}");
    }
}
