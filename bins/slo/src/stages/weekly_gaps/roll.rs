//! Weekly drills that roll a tier and watch for errors: the server StatefulSet, a workload
//! rolled through the settings API, and the rows that prove ownership moved cleanly.

use super::*;


/// `roll.zero.errors`: a rolling restart of the srv tier, measured from outside.
///
/// This is the event the owner named and the one thing nothing measured — worse than uncovered,
/// because the fast suite is designed to YIELD while a rollout is in flight, so the design's own
/// acceptance ("zero failed fast-probe samples across three consecutive rolls") was satisfied by
/// the yield rather than by the deploy. So the roll happens HERE, on the drill's own schedule,
/// with a read loop running through it.
///
/// It restores nothing, and needs no undo: the roll is a restart onto the SAME image — its own
/// undo — through the very annotation the settings machinery patches (`kloudlite.io/restarted-at`).
/// What it does wait for is the fleet settling, on every path out, because a weekly drill that
/// left the tier mid-roll would fail the next fast run for the drill's reason.
///
/// The `ownership.drained` evidence comes from the KUBERNETES log API on each pod while it is
/// terminating: neither `/admin/history/*` nor `/admin/slo` exposes pod logs, and the line exists
/// nowhere else. A pod already gone before the loop saw it is not counted against the id — what
/// fails it is a pod that was watched all the way out without ever logging the handover.
pub(crate) async fn roll_zero_errors(c: &mut Ctx) {
    let Some(repo) = c.state.repo.clone() else {
        return c.skip("roll.zero.errors", "no repo to read through the roll");
    };
    let work = c.tmp.join("git").join(&repo);
    if !work.is_dir() {
        return c.skip("roll.zero.errors", "stage 2 left no working tree to push from");
    }
    let aks = match drill::incluster() {
        Ok(k) => k,
        Err(e) => return c.skip("roll.zero.errors", &format!("no in-cluster client: {e:#}")),
    };
    let probe = c.probe_user.clone();
    c.step("roll.zero.errors", step_cap(ROLL_CAP), move |c| {
        let refs = api(c, &format!("/api/{probe}/{repo}/refs"));
        let jwt = c.probe_jwt.clone();
        let sts: Api<StatefulSet> = Api::namespaced(aks.clone(), CENTRAL_NS);
        let pods: Api<Pod> = Api::namespaced(aks.clone(), CENTRAL_NS);
        let push = Pushing {
            work,
            url: format!("{}/{probe}/{repo}.git", c.cfg.git_url.trim_end_matches('/')),
            branch: format!("roll-{}", c.run_id),
            n: Default::default(),
        };
        async move {
            settled(&sts).await.context("the srv tier was already mid-roll, so this is not our roll")?;
            let before = pod_names(&pods).await?;
            let stamp = chrono::Utc::now().to_rfc3339();
            sts.patch(
                SRV,
                &PatchParams::default(),
                &Patch::Merge(&json!({
                    "spec": { "template": { "metadata": { "annotations": {
                        "kloudlite.io/restarted-at": stamp,
                    }}}}
                })),
            )
            .await
            .map_err(|e| anyhow!("the roll could not be started: {e}"))?;
            let settle = || async {
                settled(&sts).await.context("the srv tier was left mid-roll")
            };
            let watch = async { watch_roll(c, &pods, &refs, &jwt, &before, &push).await };
            drill::undoing(ROLL_CAP, watch, settle).await
        }
        .boxed()
    })
    .await;
}


/// The loop: read and push through the roll, and collect the handover evidence on the way out.
///
/// Pods are tracked by UID, never by name: `kloudlite-srv` is a StatefulSet, so `kloudlite-srv-0`
/// is deleted and recreated under the same name — a name-based "every old pod is gone" test can
/// never become true, and the id would breach on every run for the roll working perfectly.
///
/// A **421** is not a failure. It is what the routing middleware answers while ownership moves
/// between pods, which is the event being measured; the client's own recovery is to ask again, so
/// the step asks again (bounded) and counts only a 421 that will not resolve. A 502/503, a timeout
/// or a dropped connection IS a failure — and is counted, never propagated: aborting on the first
/// dropped keep-alive would report a transport blip instead of the roll.
pub(crate) async fn watch_roll(
    c: &Ctx,
    pods: &Api<Pod>,
    refs: &str,
    jwt: &str,
    before: &[(String, String)],
    push: &Pushing,
) -> Result<()> {
    let started = std::time::Instant::now();
    let mut bad = vec![];
    let mut drained: Vec<String> = vec![];
    let mut leaving: Vec<String> = vec![];
    let mut last_push = std::time::Instant::now() - PUSH_EVERY;
    loop {
        // The read a person's clone makes, through the public listener and the routing middleware.
        if let Err(why) = routed(c, refs, jwt).await {
            bad.push(why);
        }
        // And the WRITE, which is the half a roll is most likely to break: the database has to be
        // open on whichever node now owns it.
        if last_push.elapsed() >= PUSH_EVERY {
            last_push = std::time::Instant::now();
            if let Err(e) = push.once(c).await {
                bad.push(format!("push: {e:#}"));
            }
        }
        for pod in pods.list(&ListParams::default().labels(SRV_PODS)).await.map_err(|e| anyhow!("{e}"))?.items {
            let (name, uid) = (kube::ResourceExt::name_any(&pod), uid_of(&pod));
            if pod.metadata.deletion_timestamp.is_none() || drained.contains(&uid) {
                continue;
            }
            if !leaving.contains(&uid) {
                leaving.push(uid.clone());
            }
            // Logs while it is still there: once the pod is gone so is its log, which is why this
            // is read on the beat rather than after the roll.
            let log = pods
                .logs(&name, &kube::api::LogParams { tail_lines: Some(400), ..Default::default() })
                .await
                .unwrap_or_default();
            if log.contains("ownership.drained") {
                drained.push(uid);
            }
        }
        // Done when no pod carrying an ORIGINAL uid is left, and the tier is back to full count.
        let now = pod_names(pods).await?;
        let olds: Vec<&String> = before.iter().map(|(_, uid)| uid).collect();
        if now.iter().all(|(_, uid)| !olds.contains(&uid)) && now.len() >= before.len() {
            break;
        }
        if started.elapsed() >= ROLL_CAP - Duration::from_secs(60) {
            return Err(anyhow!("the roll did not finish in {} s", ROLL_CAP.as_secs()));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    if !bad.is_empty() {
        return Err(anyhow!("{} bad answers through the roll: {}", bad.len(), bad.join(" · ")));
    }
    let silent: Vec<&String> = leaving.iter().filter(|p| !drained.contains(p)).collect();
    if !silent.is_empty() {
        return Err(anyhow!(
            "{} pod(s) left without logging `ownership.drained`",
            silent.len()
        ));
    }
    Ok(())
}


/// One routed read, with the 421 recovery a real client performs.
///
/// `Ok(())` for a 2xx, including one reached only after a 421 — the middleware handing a request
/// to the node that now owns the database is the roll working. Everything else comes back as the
/// text the step will report.
pub(crate) async fn routed(c: &Ctx, url: &str, jwt: &str) -> std::result::Result<(), String> {
    for attempt in 0..RETRIES {
        match super::super::raw(c, reqwest::Method::GET, url, jwt, None, &[]).await {
            Ok((status, _)) if status.is_success() => return Ok(()),
            // 421 Misdirected Request: ask again, which is what the client does.
            Ok((status, body)) if status.as_u16() == 421 => {
                if attempt + 1 == RETRIES {
                    return Err(format!("421 did not resolve after {RETRIES} tries: {}", body.chars().take(120).collect::<String>()));
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Ok((status, body)) => return Err(format!("{status}: {}", body.chars().take(120).collect::<String>())),
            // A dropped connection is a failed answer, counted — never propagated, or one blip
            // would end the step before it had measured the roll.
            Err(e) => return Err(format!("transport: {e:#}")),
        }
    }
    Ok(())
}


pub(crate) fn uid_of(p: &Pod) -> String {
    p.metadata.uid.clone().unwrap_or_else(|| kube::ResourceExt::name_any(p))
}


/// The live srv pods as `(name, uid)`. The uid is what identity means across a StatefulSet roll —
/// the name comes back, the uid never does.
pub(crate) async fn pod_names(pods: &Api<Pod>) -> Result<Vec<(String, String)>> {
    Ok(pods
        .list(&ListParams::default().labels(SRV_PODS))
        .await
        .map_err(|e| anyhow!("could not list the srv pods: {e}"))?
        .items
        .iter()
        .filter(|p| p.metadata.deletion_timestamp.is_none())
        .map(|p| (kube::ResourceExt::name_any(p), uid_of(p)))
        .collect())
}


/// Every replica on the current template and ready.
pub(crate) async fn settled(sts: &Api<StatefulSet>) -> Result<()> {
    let start = std::time::Instant::now();
    loop {
        let o = sts.get(SRV).await.map_err(|e| anyhow!("could not read {SRV}: {e}"))?;
        let want = o.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
        let st = o.status.unwrap_or_default();
        let (updated, ready) = (st.updated_replicas.unwrap_or(0), st.ready_replicas.unwrap_or(0));
        if updated >= want && ready >= want {
            return Ok(());
        }
        if start.elapsed() >= ROLL_CAP / 2 {
            return Err(anyhow!("{SRV} is {ready}/{want} ready, {updated} updated"));
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}


/// `admin.workload.roll`: the button an operator has, doing what it says.
///
/// `settings.roll` proves only the 409 the precheck answers; the WRITE — a merge patch of
/// `kloudlite.io/restarted-at` on a pod template — was never asserted to restart anything. The
/// target is the agent DaemonSet, the same reader `settings.roll` uses, because it is the one
/// workload whose restart costs the fleet nothing.
pub(crate) async fn workload_roll(c: &mut Ctx) {
    let (region, Some(k3s)) = (c.cfg.region.clone(), c.kube.clone()) else {
        return c.skip("admin.workload.roll", "no kubeconfig to read the restart annotation from");
    };
    c.step("admin.workload.roll", step_cap(Duration::from_secs(240)), move |c| {
        let jwt = c.admin_jwt.clone();
        let workloads = admin(c, "/admin/workloads");
        let roll = admin(c, &format!("/admin/workloads/{region}/kloudlite-agent/roll"));
        let k3s = k3s.clone();
        async move {
            let before = annotation_of(&k3s).await.context("could not read the agent DaemonSet")?;
            post(c, &roll, &jwt, json!({ "reason": "slo probe workload roll" }))
                .await
                .context("the roll was refused")?;
            // The WRITE itself: `kloudlite.io/restarted-at` on the pod template is what a roll IS,
            // and it is a fact rather than a race — the old check sampled `/admin/workloads` for a
            // dip below desired, which a single-node DaemonSet need never show.
            let after = annotation_of(&k3s).await.context("could not re-read the agent DaemonSet")?;
            if after == before {
                return Err(anyhow!("the roll answered 2xx and the restart annotation did not move"));
            }
            // And every reader came back: a roll that restarts a workload into CrashLoop is a roll
            // nobody wanted.
            // The roll is not over when the pods are ready: they were ready BEFORE it started, and
            // reading that answered "back" in 178 ms while every agent was still about to restart —
            // so `ws.spread`, next in line, ran across three agent restarts and lost its handover.
            // Over means every pod is on the new template and ready, which the DaemonSet's own
            // status says; how long that takes is the number a person waits on a settings save.
            let took = settle(Duration::from_secs(180), "the rolled DaemonSet never settled", || async {
                use k8s_openapi::api::apps::v1::DaemonSet;
                let api: Api<DaemonSet> = Api::namespaced(k3s.clone(), "kube-system");
                let ds = api.get("kloudlite-agent").await.map_err(|e| anyhow!("{e}"))?;
                let st = ds.status.unwrap_or_default();
                let (desired, updated, ready) = (st.desired_number_scheduled, st.updated_number_scheduled.unwrap_or(0), st.number_ready);
                let settled = desired > 0 && updated >= desired && ready >= desired
                    && ds.metadata.generation.is_some_and(|g| st.observed_generation.unwrap_or(0) >= g);
                Ok((!settled).then(|| format!("{updated}/{desired} on the new template, {ready} ready")))
            })
            .await?;
            tracing::info!(ms = took.as_millis() as u64, "slo.workload.roll.settled");
            // And the admin's own view agrees, which is what the console shows a person.
            poll_rows(c, &workloads, &jwt, Duration::from_secs(60), |r| {
                agent_row(r).is_some_and(|(ready, desired)| ready >= desired && desired > 0)
            })
            .await
            .context("the console never showed the rolled workload ready")
        }
        .boxed()
    })
    .await;
}


/// The roll annotation on the agent DaemonSet's pod template, or the empty string when it carries
/// none yet — which is the ordinary state before the first roll, and still a value that must move.
pub(crate) async fn annotation_of(k3s: &kube::Client) -> Result<String> {
    use k8s_openapi::api::apps::v1::DaemonSet;
    let api: Api<DaemonSet> = Api::namespaced(k3s.clone(), "kube-system");
    let ds = api.get("kloudlite-agent").await.map_err(|e| anyhow!("{e}"))?;
    Ok(ds
        .spec
        .and_then(|s| s.template.metadata)
        .and_then(|m| m.annotations)
        .and_then(|a| a.get("kloudlite.io/restarted-at").cloned())
        .unwrap_or_default())
}


pub(crate) async fn rows_of(c: &Ctx, url: &str, jwt: &str) -> Result<Vec<Value>> {
    let doc = get(c, url, jwt).await.context("could not read the workloads")?;
    Ok(doc.get("workloads").and_then(Value::as_array).or_else(|| doc.as_array()).cloned().unwrap_or_default())
}


pub(crate) fn agent_row(rows: &[Value]) -> Option<(i64, i64)> {
    let r = rows.iter().find(|r| r.get("name").and_then(Value::as_str) == Some("kloudlite-agent"))?;
    let n = |k: &str| r.get(k).and_then(Value::as_i64).unwrap_or(0);
    Some((n("ready"), n("desired")))
}


pub(crate) async fn poll_rows(
    c: &Ctx,
    url: &str,
    jwt: &str,
    cap: Duration,
    want: impl Fn(&[Value]) -> bool,
) -> Result<()> {
    let start = std::time::Instant::now();
    loop {
        let rows = rows_of(c, url, jwt).await?;
        if want(&rows) {
            return Ok(());
        }
        if start.elapsed() >= cap {
            return Err(anyhow!("not there after {} ms", cap.as_millis()));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
