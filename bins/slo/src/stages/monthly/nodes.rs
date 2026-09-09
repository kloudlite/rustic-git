//! Monthly drills on a node's life: decommission (drain, then cordon once drained) and the dead-node
//! sweep, on an idle pool node the drill picks itself.

use super::*;


/// `cluster.decommission`: the 409 gate, and the cordon behind it.
///
/// Drain and decommission are two distinct actions and only drain was drilled. The interesting
/// half is the REFUSAL: a decommission is 409 "not drained yet" until the node's own agent has
/// stamped the sticky `drained <RFC 3339>`, and that gate is the only thing between an operator
/// and cordoning a node that is still holding somebody's bytes. Both halves in one step, in that
/// order — a fleet that refused everything would pass the first alone, and one that cordoned
/// anything would pass the second.
///
/// Everything it does is undone: the drain is lifted and the cordon taken off on every path out —
/// including the path where the GATE IS OPEN and the first POST cordons the node, which is why
/// both decommission attempts live inside the `undoing` region rather than in front of it. It
/// never deletes anything — the console stops at the
/// cordon by design, and deleting the VM is a human's separate step.
pub(crate) async fn decommission(c: &mut Ctx) {
    let (Some(k), region) = (c.kube.clone(), c.cfg.region.clone()) else {
        return c.skip("cluster.decommission", "no kubeconfig");
    };
    // The same choice `drill.drain` makes, and it must not be the node that drill just used: two
    // nodes retiring at once on a shared cluster is a fleet with nowhere left to place anything.
    let busy = match probe_workspace(c).await {
        Some(ws) => node_of(c, &ws).await,
        None => None,
    };
    let node = match idle_node(&k, busy.as_deref()).await {
        Ok(n) => n,
        Err(e) => return c.skip("cluster.decommission", &format!("{e:#}")),
    };
    c.step("cluster.decommission", step_cap(DRAIN_CAP), move |c| {
        let jwt = c.admin_jwt.clone();
        let base = admin(c, &format!("/admin/clusters/{region}/nodes/{node}"));
        let reason = json!({ "reason": format!("slo probe decommission drill {}", c.run_id) });
        async move {
            // The undo is established BEFORE the first decommission POST, not after it. The gate
            // being OPEN is the very failure this id exists to catch — and it is also the state a
            // node an earlier run left stamped `drained` is in — so a POST outside this region
            // would cordon the node and return `Err` with nothing to uncordon it. Both mutations
            // go back on every path out, in the order that leaves the node usable.
            let undo = || async {
                use crate::drill::Cluster;
                let uncordon = k.cordon(&node, false).await.context("the node was left CORDONED");
                let undrained = verb(c, &base, "undrain", &jwt, &reason).await.context("the node was left DRAINING");
                uncordon.and(undrained)
            };
            let body = async {
                // Before the drain: nothing has stamped `drained`, so this must be refused.
                refused_until_drained(c, &base, &jwt, &reason).await?;
                verb(c, &base, "drain", &jwt, &reason).await.context("the drain was refused")?;
                stamped(&k, &node, DRAIN_CAP - Duration::from_secs(60))
                    .await
                    .context("the node never finished draining, so the gate could not be tried")?;
                // Now it must be TAKEN, and the node must actually be cordoned afterwards: the
                // console's own contract is that a decommission stops at `spec.unschedulable`.
                verb(c, &base, "decommission", &jwt, &reason)
                    .await
                    .context("the decommission was refused even though the agent had stamped `drained`")?;
                cordoned(&k, &node).await
            };
            drill::undoing(DRAIN_CAP, body, undo).await
        }
        .boxed()
    })
    .await;
}


/// The gate: a decommission before the stamp answers 409, and only 409.
///
/// A 5xx is not a refusal — the tier that fell over refused nothing, it could not answer — and a
/// 2xx is the gate being open, which is the whole failure this id exists for.
pub(crate) async fn refused_until_drained(c: &Ctx, base: &str, jwt: &str, reason: &Value) -> Result<()> {
    let (status, body) = super::super::raw(
        c,
        reqwest::Method::POST,
        &format!("{base}/decommission"),
        jwt,
        Some(reason.clone()),
        &[],
    )
    .await?;
    match status.as_u16() {
        409 => Ok(()),
        200..=299 => Err(anyhow!("a node that has not drained was ALLOWED to be decommissioned")),
        other => Err(anyhow!(
            "the decommission answered {other}, which is not the gate refusing: {}",
            body.chars().take(200).collect::<String>()
        )),
    }
}


/// The node is unschedulable — where a decommission stops, and no further.
pub(crate) async fn cordoned(k: &kube::Client, node: &str) -> Result<()> {
    let api: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(k.clone());
    let obj = api.get(node).await.map_err(|e| anyhow!("could not read {node}: {e}"))?;
    match obj.spec.and_then(|s| s.unschedulable) {
        Some(true) => Ok(()),
        _ => Err(anyhow!("the decommission was taken but {node} is not cordoned")),
    }
}

// ── backups ─────────────────────────────────────────────────────────────


/// `drill.dead.node`: NOT AUTOMATED, and a skip that says why.
///
/// The first live monthly run failed it by construction. The drill deleted the node's agent pod
/// and tainted the node, but the product's definition of dead is the NODE's own Ready condition
/// being non-True for `WS_NODE_DEAD_SECS` (`peer::unplaceable`) — the DaemonSet puts the agent back
/// in seconds and the node never leaves Ready, so nothing is ever un-placed and the drill was
/// measuring a state the fleet was never in.
///
/// A real node death cannot be produced from inside the cluster, and the probe must not be given
/// node-level access to produce one: a synthetic user with a way to stop kubelets is a bigger
/// hole than this id is worth. So the id stays in the catalogue and files a SKIP naming the
/// operator's own recipe — the console shows "not automated" rather than nothing at all, which is
/// the honest state. `drill.drain`, which uses the decommission label the product itself uses, is
/// the automated monthly path.
pub(crate) async fn dead_node(c: &mut Ctx) {
    c.skip("drill.dead.node", NODE_LEVEL_DRILL);
}


/// `drill.drain`: a drain does NOT interrupt what is running on the node.
///
/// The SLI says "without interrupting a running worktree" and the drill used to pick an IDLE node
/// on purpose, so that clause was vacuously true on every run: the documented guarantee — a
/// decommissioning node keeps running whatever it holds while releasing the rest — was the one
/// thing untested. So the node drained is the one holding this run's own RUNNING workspace, and
/// what is asserted is that the workspace is still running afterwards with the same pod, and that
/// the node's own beat stamped `draining` counting it.
///
/// It never waits for `drained`: a node with a running worktree on it must NOT reach that stamp,
/// and `cluster.decommission` is the id that walks the stamp on a node that can. The undrain is in
/// the undo path — a node left labelled is a node placement will not use again.
pub(crate) async fn drain(c: &mut Ctx) {
    let (Some(k), region) = (c.kube.clone(), c.cfg.region.clone()) else {
        return c.skip("drill.drain", "no kubeconfig");
    };
    let Some(ws) = probe_workspace(c).await else {
        return c.skip("drill.drain", "no probe workspace to keep running through a drain");
    };
    let Some(node) = node_of(c, &ws).await else {
        return c.skip("drill.drain", "the workspace names no node");
    };
    let before = pod_uid(&k, c, &ws).await;
    c.step("drill.drain", step_cap(DRAIN_CAP), move |c| {
        let jwt = c.admin_jwt.clone();
        let probe_jwt = c.probe_jwt.clone();
        let base = admin(c, &format!("/admin/clusters/{region}/nodes/{node}"));
        let doc = api(c, &format!("/v1/workspaces/{ws}"));
        let reason = json!({ "reason": format!("slo probe drill {}", c.run_id) });
        async move {
            verb(c, &base, "drain", &jwt, &reason).await.context("the drain was refused")?;
            let body = async {
                // The agent's own beat is `WS_DECOMMISSION_SECS` (30); two of them, so the stamp
                // below is a decision it made rather than one it has not reached yet.
                draining_stamp(&k, &node, DRAIN_CAP / 2).await?;
                let now = get(c, &doc, &probe_jwt).await.context("could not read the workspace")?;
                let state = now.get("state").and_then(Value::as_str).unwrap_or_default();
                if !matches!(state, "ready" | "running") {
                    return Err(anyhow!("a running workspace on a draining node went to `{state}`"));
                }
                // The pod itself, not only the phase: a controller that deleted and recreated it
                // has interrupted the person at the keyboard whatever the status says afterwards.
                let after = pod_uid(&k, c, &ws).await;
                if before.is_some() && after != before {
                    return Err(anyhow!("the workspace's pod was replaced while its node drained"));
                }
                Ok(())
            };
            drill::undoing(DRAIN_CAP, body, || verb(c, &base, "undrain", &jwt, &reason)).await
        }
        .boxed()
    })
    .await;
}


/// The workspace pod's uid, or `None` when it cannot be read — in which case the comparison above
/// is skipped rather than guessed at.
pub(crate) async fn pod_uid(k: &kube::Client, c: &Ctx, ws: &str) -> Option<String> {
    let ns = kloudlite_workspaces::crd::ws_namespace(&c.probe_user, "");
    let pods: kube::Api<k8s_openapi::api::core::v1::Pod> = kube::Api::namespaced(k.clone(), &ns);
    pods.get_opt(ws).await.ok()??.metadata.uid
}


/// Wait for the agent's `draining running=N …` stamp — the beat's own record that it is retiring
/// the node WITHOUT stopping what runs there. `drained` is a different stamp and a different id.
pub(crate) async fn draining_stamp(k: &kube::Client, node: &str, cap: Duration) -> Result<()> {
    use kloudlite_workspaces::crd;
    let api: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(k.clone());
    let at = std::time::Instant::now();
    loop {
        let obj = api.get(node).await.map_err(|e| anyhow!("could not read {node}: {e}"))?;
        let stamp = obj
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(crd::DECOMMISSION_STATUS))
            .cloned()
            .unwrap_or_default();
        if stamp.starts_with("draining") || stamp.starts_with(crd::DRAINED_PREFIX) {
            return Ok(());
        }
        if at.elapsed() >= cap {
            return Err(anyhow!("{node}'s agent never stamped its drain: it reports {stamp:?}"));
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}


/// One node verb on the admin API. Both halves take the same reason, which is what the audit row
/// carries — a drain nobody can explain is worse in the log than no drain at all.
pub(crate) async fn verb(c: &Ctx, base: &str, v: &str, jwt: &str, reason: &Value) -> Result<()> {
    post(c, &format!("{base}/{v}"), jwt, reason.clone()).await.map(|_| ())
}


/// A node holding nothing that is running, not already being retired, and not the one the taint
/// drill just used.
///
/// "Nothing running" is read off the WORKTREES, not the node: a drain only sets a label, and the
/// agent's beat releases volumes as they become releasable — but whatever is RUNNING there keeps
/// running, so a node with a live worktree on it would never stamp `drained` inside the drill's ten
/// minutes and the id would fail for the fleet behaving exactly as designed. Anyone's worktree
/// counts, not only the probe's: this drill touches a shared cluster.
/// A POOL node only: the drain and the `drained …` stamp are the node's own agent's work, and
/// the agent runs only where `kloudlite.io/pool` is set. The control plane carries no pool label,
/// has nothing to drain, and stamps nothing — picking it labelled k3s-cp waited the whole cap for
/// a stamp that could never come.
pub(crate) fn is_pool_node(n: &k8s_openapi::api::core::v1::Node) -> bool {
    n.metadata.labels.as_ref().and_then(|l| l.get("kloudlite.io/pool")).map(String::as_str) == Some("true")
}


pub(crate) async fn idle_node(k: &kube::Client, avoid: Option<&str>) -> Result<String> {
    use kloudlite_workspaces::crd;
    let busy = running_nodes(k).await?;
    let api: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(k.clone());
    let list = api.list(&kube::api::ListParams::default()).await.map_err(|e| anyhow!("could not list the nodes: {e}"))?;
    list.items
        .iter()
        .find(|n| {
            let name = kube::ResourceExt::name_any(*n);
            is_pool_node(n)
                && Some(name.as_str()) != avoid
                && !busy.contains(&name)
                && !n.metadata.labels.as_ref().is_some_and(|l| l.contains_key(crd::DECOMMISSION_LABEL))
                // A node already cordoned by a person is one somebody is retiring by hand.
                && !n.spec.as_ref().and_then(|s| s.unschedulable).unwrap_or(false)
        })
        .map(kube::ResourceExt::name_any)
        .ok_or_else(|| anyhow!("every node holds a running worktree, or is already draining"))
}


/// Every node with a Running workspace or environment placed on it.
pub(crate) async fn running_nodes(k: &kube::Client) -> Result<Vec<String>> {
    use kloudlite_workspaces::crd;
    let mut out = vec![];
    let ws: kube::Api<crd::Workspace> = kube::Api::all(k.clone());
    let env: kube::Api<crd::Environment> = kube::Api::all(k.clone());
    let p = kube::api::ListParams::default();
    for (node, running) in ws
        .list(&p)
        .await
        .map_err(|e| anyhow!("could not list the workspaces: {e}"))?
        .items
        .iter()
        .map(|w| (w.status.as_ref().map(|s| s.node_name.clone()), is_running(w.status.as_ref().map(|s| s.phase.as_str()))))
        .chain(
            env.list(&p)
                .await
                .map_err(|e| anyhow!("could not list the environments: {e}"))?
                .items
                .iter()
                .map(|e| (e.status.as_ref().map(|s| s.node_name.clone()), is_running(e.status.as_ref().map(|s| s.phase.as_str())))),
        )
    {
        if let (Some(node), true) = (node.filter(|n| !n.is_empty()), running) {
            out.push(node);
        }
    }
    Ok(out)
}


/// Anything but a stopped or failed phase is something a person could be typing into.
pub(crate) fn is_running(phase: Option<&str>) -> bool {
    !matches!(phase.unwrap_or_default(), "" | "Stopped" | "stopped" | "Failed" | "failed")
}


/// Wait for the agent's sticky `drained <RFC 3339>` stamp.
pub(crate) async fn stamped(k: &kube::Client, node: &str, cap: Duration) -> Result<()> {
    use kloudlite_workspaces::crd;
    let api: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(k.clone());
    let at = std::time::Instant::now();
    loop {
        let obj = api.get(node).await.map_err(|e| anyhow!("could not read {node}: {e}"))?;
        // Annotation first, label second: the agent stamps an annotation and `undrain` clears one,
        // but a value that long is not a legal label, so reading only labels would wait forever.
        let stamp = obj
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(crd::DECOMMISSION_STATUS))
            .or_else(|| obj.metadata.labels.as_ref().and_then(|l| l.get(crd::DECOMMISSION_STATUS)))
            .cloned()
            .unwrap_or_default();
        if stamp.starts_with(crd::DRAINED_PREFIX) {
            return Ok(());
        }
        if at.elapsed() >= cap {
            // The COUNTS, verbatim: the first live monthly run sat at `draining running=0 owned=0
            // copies=1 thin=0` for ten minutes — a lone replica copy that never healed or retired,
            // which is a product stall and reads as one only if the stamp is in the detail.
            return Err(anyhow!("after {} ms {node} still reports {stamp:?}", cap.as_millis()));
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
