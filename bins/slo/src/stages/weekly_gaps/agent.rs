//! Weekly drills on the node agent: retention, the janitor, the worker lanes, and start spread
//! by rendezvous over the placeable nodes.

use super::*;


/// `snap.retain`: retention prunes sync points and never a push.
///
/// The rule is one Ready transient per worktree and a push that is never pruned — which is the
/// difference between a replica having something recent to fetch and a person losing the cut they
/// asked for. Both halves are read off the CRDs, because history deliberately does not list sync
/// points at all.
pub(crate) async fn retain(c: &mut Ctx, cold: Option<&str>) {
    let (Some(k), Some(ws)) = (c.kube.clone(), cold.map(str::to_string)) else {
        let why = if c.kube.is_none() { "no kubeconfig" } else { "no cold workspace" };
        return c.skip("snap.retain", why);
    };
    c.step("snap.retain", step_cap(SWEEP_CAP), move |c| {
        let jwt = c.probe_jwt.clone();
        let doc = api(c, &format!("/v1/workspaces/{ws}"));
        async move {
            use kloudlite_workspaces::crd;
            // The cold workspace's own volume and a push of this step's own: stage 5's volume and
            // push are gone by now (stage 7 deletes both), and a push is the half of the rule that
            // matters — it is the cut retain must never prune.
            // The push comes first: the doc names its volume only once a snapshot exists.
            let pushes = Some(super::super::experience_env::push_once(c, &ws, "retain").await.context("could not push")?);
            let volume = volume_id(&get(c, &doc, &jwt).await.context("could not read the workspace")?)?;
            let history = api(c, &format!("/v1/volumes/{volume}/history"));
            // The RULE is the signal (2026-09-12): this used to sleep out two minutes and look
            // once, so it asserted after a wait it had chosen and said nothing about how long the
            // fleet actually needed. `settle` polls the rule and answers that number.
            let api: kube::Api<crd::Snapshot> = kube::Api::all(k.clone());
            let took = settle(SWEEP_CAP - Duration::from_secs(60), "retain has stopped pruning", || {
                let (api, volume) = (api.clone(), volume.clone());
                async move {
                    let all = api
                        .list(&kube::api::ListParams::default())
                        .await
                        .map_err(|e| anyhow!("could not list the snapshots: {e}"))?;
                    let mut per_worktree: std::collections::HashMap<String, usize> = Default::default();
                    for s in all.items.iter().filter(|s| s.spec.volume == volume && s.spec.transient) {
                        if s.status.as_ref().is_some_and(|st| st.phase == crd::Phase::Ready) {
                            *per_worktree.entry(s.spec.worktree.clone()).or_default() += 1;
                        }
                    }
                    Ok(per_worktree
                        .iter()
                        .find(|(_, n)| **n > 1)
                        .map(|(wt, n)| format!("{n} Ready sync points remain for worktree {wt:?}")))
                }
            })
            .await?;
            tracing::info!(secs = took.as_secs(), "slo.retain.settled");
            // And the push is still there — the half that loses somebody's cut if it is wrong.
            let Some(push) = pushes else { return Ok(()) };
            let doc = get(c, &history, &jwt).await.context("could not read the history")?;
            let kept = doc
                .get("snapshots")
                .and_then(Value::as_array)
                .or_else(|| doc.as_array())
                .is_some_and(|rs| rs.iter().any(|r| r.get("id").and_then(Value::as_str) == Some(push.as_str())));
            if !kept {
                return Err(anyhow!("the push {push} is gone from history: retain pruned a snapshot"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}


/// `agent.janitor`: nothing is left on disk for an object this run deleted.
///
/// The janitor's three sweeps (attach directories, dangling profile-index entries, orphan snapshot
/// records) all clean up after objects that are already gone, so the honest way to watch them is
/// from the other end: a workspace this run deleted must leave no attach directory, and no
/// `Snapshot` may name a `Volume` that no longer exists.
pub(crate) async fn janitor(c: &mut Ctx) {
    let Some(k) = c.kube.clone() else { return c.skip("agent.janitor", "no kubeconfig") };
    let prefix = c.prefix();
    c.step("agent.janitor", step_cap(SWEEP_CAP), move |_| {
        async move {
            use kloudlite_workspaces::crd;
            let vols: kube::Api<crd::Volume> = kube::Api::all(k.clone());
            let snaps: kube::Api<crd::Snapshot> = kube::Api::all(k.clone());
            // The janitor's own beat is the wait, and the ORPHANS are the signal — the fixed
            // two-minute sleep asserted after a wait it chose and reported nothing about how long
            // the sweep really took (2026-09-12).
            let took = settle(SWEEP_CAP - Duration::from_secs(60), "a snapshot record outlived its volume", || {
                let (vols, snaps, prefix) = (vols.clone(), snaps.clone(), prefix.clone());
                async move {
                    let p = kube::api::ListParams::default();
                    let volumes: Vec<String> = vols
                        .list(&p)
                        .await
                        .map_err(|e| anyhow!("could not list the volumes: {e}"))?
                        .items
                        .iter()
                        .map(kube::ResourceExt::name_any)
                        .collect();
                    let orphans: Vec<String> = snaps
                        .list(&p)
                        .await
                        .map_err(|e| anyhow!("could not list the snapshots: {e}"))?
                        .items
                        .iter()
                        .filter(|s| kube::ResourceExt::name_any(*s).starts_with(&prefix))
                        .filter(|s| !volumes.contains(&s.spec.volume))
                        .map(kube::ResourceExt::name_any)
                        .collect();
                    Ok((!orphans.is_empty()).then(|| orphans.join(", ")))
                }
            })
            .await?;
            tracing::info!(secs = took.as_secs(), "slo.janitor.settled");
            Ok(())
        }
        .boxed()
    })
    .await;
}


/// `srv.lanes`: the server's own beats, seen from the outside.
///
/// The pull-counter flush is the one lane with a user-visible number: a pull increments a counter
/// held in memory and a lane writes it back to the image's row, so a lane that stopped running
/// leaves every image reporting zero pulls forever — which is also the strongest available
/// evidence that the beats are turning at all.
pub(crate) async fn lanes(c: &mut Ctx) {
    let Some(secret) = c.state.token_value.clone() else {
        return c.skip("srv.lanes", "no personal token");
    };
    let probe = c.probe_user.clone();
    let name = format!("{}-lanes", c.prefix());
    let dir = c.tmp.join("img-lanes");
    let host = super::super::registry::host(c);
    c.step("srv.lanes", step_cap(SWEEP_CAP), move |c| {
        let crane = super::super::registry::authed(c);
        let jwt = c.probe_jwt.clone();
        // The TAG rows, not the images listing: `pulls` is a per-tag counter and only
        // `imagetags` carries it — the listing rows are markers (name, manifests, visibility) and
        // polling them for a field they never have is a step that cannot pass.
        let tags = api(c, &format!("/api/{probe}/{name}/imagetags"));
        let dest = c.tmp.join("pull-lanes");
        async move {
            let layer = super::super::registry::random_layer();
            super::super::registry::write_layout(&dir, &layer, &name).context("could not build the image")?;
            crane.login(&host, &probe, &secret).await.context("could not log in")?;
            let reference = format!("{host}/{probe}/{name}:latest");
            crane.push(&dir, &reference).await.context("could not push it")?;
            for _ in 0..3 {
                let _ = std::fs::remove_dir_all(&dest);
                crane.pull(&reference, &dest).await.context("the image would not pull")?;
            }
            // The flush lane's own beat, then the number it is supposed to have written.
            poll_json(c, &tags, &jwt, SWEEP_CAP - Duration::from_secs(60), |v| {
                let rows = v.get("tags").and_then(Value::as_array).or_else(|| v.as_array()).cloned().unwrap_or_default();
                rows.iter().any(|r| {
                    r.get("tag").and_then(Value::as_str) == Some("latest")
                        && r.get("pulls").and_then(Value::as_u64).unwrap_or(0) > 0
                })
            })
            .await
            .context("three pulls never reached the image's pull counter: the flush lane is not running")
        }
        .boxed()
    })
    .await;
}

// ── placement and the gateway ───────────────────────────────────────────


/// `ws.spread`: a movable volume lands on the node placement prefers, and that is a DIFFERENT one.
///
/// `ws.cross.node` forces a move by making the owner unplaceable, which is the FAILURE path. This
/// is the ordinary one: a volume with nothing running on it is movable, and its owner hands it
/// over when rendezvous prefers somebody else — the whole of how a fleet balances. Nothing is
/// broken to make it happen.
///
/// A fleet with one placeable node cannot spread, and saying so is the honest answer: the step
/// SKIPS rather than passing on the owner it started from, which would have been a tautology.
///
// ponytail: on THIS region a second node also has to have room — a pool node is 8 vCPU and a
// workspace requests 2 — so `ws.spread` and the two cross-node ids can fail for capacity on a busy
// hour rather than for placement. The ceiling is the pool: a bigger node, or a second one kept
// free, is what makes them measure only what they name.
pub(crate) async fn spread(c: &mut Ctx, cold: Option<&str>) {
    let (Some(ws), Some(k)) = (cold.map(str::to_string), c.kube.clone()) else {
        let why = if cold.is_none() { "no cold workspace" } else { "no kubeconfig" };
        return c.skip("ws.spread", why);
    };
    match placeable_nodes(&k).await {
        Ok(n) if n >= 2 => {}
        Ok(_) => return c.skip("ws.spread", "one placeable node: this region cannot spread"),
        Err(e) => return c.skip("ws.spread", &format!("{e:#}")),
    }
    c.step("ws.spread", step_cap(SWEEP_CAP), move |c| {
        let jwt = c.probe_jwt.clone();
        let doc = api(c, &format!("/v1/workspaces/{ws}"));
        let (stop, start) = (
            api(c, &format!("/v1/workspaces/{ws}/stop")),
            api(c, &format!("/v1/workspaces/{ws}/start")),
        );
        async move {
            // A push first: the doc names its volume only once a snapshot exists, and the
            // volume id is the rendezvous key.
            super::super::experience_env::push_once(c, &ws, "spread").await.context("could not push")?;
            let before = get(c, &doc, &jwt).await.context("could not read the workspace")?;
            let was = before.get("placement").and_then(Value::as_str).unwrap_or_default().to_string();
            let volume = volume_id(&before)?;
            post(c, &stop, &jwt, Value::Null).await.context("could not stop it")?;
            poll_json(c, &doc, &jwt, Duration::from_secs(60), |v| {
                v.get("state").and_then(Value::as_str) == Some("stopped")
            })
            .await
            .context("it never stopped")?;
            // The candidate set is only settled once the stop cut has landed on a peer; before
            // that the owner is the one node that may start it, and "it stayed" proves nothing.
            poll_json(c, &doc, &jwt, Duration::from_secs(120), |v| {
                v.pointer("/replicated/ready").and_then(Value::as_bool) == Some(true)
            })
            .await
            .context("the stop cut never reached a peer")?;
            let (preferred, candidates) = rendezvous_choice(&k, &volume, &ws, &was).await?;
            post(c, &start, &jwt, Value::Null).await.context("could not start it")?;
            poll_json(c, &doc, &jwt, Duration::from_secs(120), |v| {
                v.get("state").and_then(Value::as_str) == Some("ready")
                    && v.get("placement").and_then(Value::as_str) == Some(preferred.as_str())
            })
            .await
            .with_context(|| {
                format!("it did not come back on {preferred}, the rendezvous choice over {candidates:?} (it was on {was})")
            })
        }
        .boxed()
    })
    .await;
}


/// Nodes placement may choose: Ready, not cordoned, not being decommissioned. Fewer than two and
/// The Volume CR's name from a workspace doc. The doc's `volume` is the registry-style pointer
/// `vol/{owner}/{id}`; the CR, the replica rows, the field selector and `/v1/volumes/{name}` all
/// key on the bare id — the pointer used whole matches nothing and reads as "no replicas".
pub(crate) fn volume_id(doc: &Value) -> Result<String> {
    let pointer = doc
        .get("volume")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("the workspace names no volume after a push"))?;
    Ok(pointer.rsplit('/').next().unwrap_or(pointer).to_string())
}


/// The node the agent's own spread rule picks for `volume`'s next start: rendezvous over
/// `{owner} ∪ {nodes up to date for the worktree}`, exactly `peer::preferred_node` — the same
/// hash (`replicate::targets`) and the same up-to-date test (`VolumeReplica.status.branches`
/// names the newest Ready transient). Recomputed here rather than read back because the agent
/// records no "preferred" anywhere: the contract is the rule, and this is the rule.
pub(crate) async fn rendezvous_choice(k: &kube::Client, volume: &str, ws: &str, owner: &str) -> Result<(String, Vec<String>)> {
    use kloudlite_workspaces::crd;
    let snaps: Api<crd::Snapshot> = Api::all(k.clone());
    let snaps = snaps
        .list(&ListParams::default().fields(&format!("spec.volume={volume}")))
        .await
        .context("could not list the snapshots")?;
    let newest = crd::newest_transient_of(&snaps.items, ws);
    let rows: Api<crd::VolumeReplica> = Api::all(k.clone());
    let rows = rows.list(&ListParams::default()).await.context("could not list the volume replicas")?;
    let mut candidates: Vec<String> = rows
        .items
        .iter()
        .filter(|r| r.spec.volume == volume)
        .filter(|r| {
            r.status.as_ref().is_some_and(|st| match newest.as_deref() {
                None => st.phase == "Synced",
                Some(want) => st.branches.get(ws).is_some_and(|held| held == want),
            })
        })
        .map(|r| r.spec.node.clone())
        .collect();
    candidates.push(owner.to_string());
    candidates.sort();
    candidates.dedup();
    let preferred = kloudlite_workspaces::replicate::targets(volume, "", &candidates, 2)
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no candidate node at all"))?;
    Ok((preferred, candidates))
}


/// there is nothing to spread across.
pub(crate) async fn placeable_nodes(k: &kube::Client) -> Result<usize> {
    use k8s_openapi::api::core::v1::Node;
    let api: Api<Node> = Api::all(k.clone());
    let list = api.list(&ListParams::default()).await.map_err(|e| anyhow!("could not list the nodes: {e}"))?;
    Ok(list
        .items
        .iter()
        .filter(|n| {
            let ready = n
                .status
                .as_ref()
                .and_then(|s| s.conditions.as_ref())
                .is_some_and(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"));
            let cordoned = n.spec.as_ref().and_then(|s| s.unschedulable).unwrap_or(false);
            let leaving = n
                .metadata
                .labels
                .as_ref()
                .is_some_and(|l| l.contains_key(kloudlite_workspaces::crd::DECOMMISSION_LABEL));
            ready && !cordoned && !leaving
        })
        .count())
}
