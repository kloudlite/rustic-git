//! Weekly drills on a drain: the handover of a node's repos and a moved image's first request.

use super::*;


/// `srv.drain.handover`: the endpoint's own contract, without a whole roll.
///
/// The drain is triggered the way the deployment triggers it — the pod's preStop hook, fired by
/// deleting one pod — because `POST /peer/v1/drain` is peer-only and the probe holds no peer
/// secret, deliberately (`sec.peer.listener` is the id that says so). What is asserted is what a
/// person would see: the leaving pod reports itself unhealthy as `draining`, so the Service stops
/// sending it traffic, and a repo it may have owned still reads throughout.
pub(crate) async fn drain_handover(c: &mut Ctx) {
    let Some(repo) = c.state.repo.clone() else {
        return c.skip("srv.drain.handover", "no repo to read while a pod leaves");
    };
    let aks = match drill::incluster() {
        Ok(k) => k,
        Err(e) => return c.skip("srv.drain.handover", &format!("no in-cluster client: {e:#}")),
    };
    let probe = c.probe_user.clone();
    c.step("srv.drain.handover", step_cap(DRAIN_CAP), move |c| {
        let refs = api(c, &format!("/api/{probe}/{repo}/refs"));
        let jwt = c.probe_jwt.clone();
        let pods: Api<Pod> = Api::namespaced(aks.clone(), CENTRAL_NS);
        let sts: Api<StatefulSet> = Api::namespaced(aks.clone(), CENTRAL_NS);
        async move {
            let names = pod_names(&pods).await?;
            let victim = names.first().map(|(name, _)| name.clone()).ok_or_else(|| anyhow!("no srv pod to drain"))?;
            let ip = pods
                .get(&victim)
                .await
                .ok()
                .and_then(|p| p.status.and_then(|s| s.pod_ip))
                .ok_or_else(|| anyhow!("{victim} publishes no address"))?;
            pods.delete(&victim, &kube::api::DeleteParams::default())
                .await
                .map_err(|e| anyhow!("could not ask {victim} to leave: {e}"))?;
            let settle = || async { settled(&sts).await.context("the srv tier was left short a pod") };
            let body = async {
                // `draining` on /healthz is what takes it out of the Service. A pod that vanished
                // before the first poll is a grace period shorter than the probe's beat, not a
                // failed handover — so a 404/refused dial ends the wait rather than failing it.
                let saw = draining(c, &ip, DRAIN_CAP / 2).await?;
                if !saw {
                    // Asserted BEFORE the repo read, so the message names what actually failed:
                    // a pod that answered `/healthz` for the whole window without ever saying
                    // `draining` never took itself out of the Service, which is the handover this
                    // id is about (2026-09-12 — the check came after and read as an afterthought).
                    return Err(anyhow!("{victim} answered /healthz for {} s without ever reporting `draining`, so it never left the Service", (DRAIN_CAP / 2).as_secs()));
                }
                // And the repo still reads, which is the half a person would notice.
                let (status, text) = super::super::raw(c, reqwest::Method::GET, &refs, &jwt, None, &[]).await?;
                if !status.is_success() {
                    return Err(anyhow!("a repo stopped reading while a pod drained: {status}: {}", text.chars().take(120).collect::<String>()));
                }
                Ok(())
            };
            drill::undoing(DRAIN_CAP, body, settle).await
        }
        .boxed()
    })
    .await;
}


/// Poll a pod's own `/healthz` for the drain answer. `Ok(false)` means it never said so and was
/// still reachable; a connection that stops answering is the pod having gone, which is a `true`.
pub(crate) async fn draining(c: &Ctx, ip: &str, cap: Duration) -> Result<bool> {
    let start = std::time::Instant::now();
    let mut answered = false;
    loop {
        match c.http.get(format!("http://{ip}:{SRV_HTTP_PORT}/healthz")).timeout(Duration::from_secs(3)).send().await {
            Ok(r) => {
                let body = r.text().await.unwrap_or_default();
                answered = true;
                if body.contains("draining") {
                    return Ok(true);
                }
            }
            // Gone, or refusing connections: it left. Only meaningful once it HAS answered once,
            // so a wrong address cannot pass as a completed drain.
            Err(_) if answered => return Ok(true),
            Err(e) => {
                if start.elapsed() >= cap {
                    return Err(anyhow!("{ip} never answered /healthz at all: {}", e.without_url()));
                }
            }
        }
        if start.elapsed() >= cap {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// ── registry ────────────────────────────────────────────────────────────


/// `reg.moved.image`: the first pull after an image's database changes nodes.
///
/// The known user-visible failure: every store error becomes an `oci_internal`, and the first
/// request to a moved image can 500 once on a fenced handle. It is a KNOWN gap with no id, which
/// makes it a gap that can get worse without anyone noticing. The move is forced the way a deploy
/// forces it — the srv pods restart — and the pull happens immediately afterwards.
pub(crate) async fn moved_image(c: &mut Ctx) {
    let Some(secret) = c.state.token_value.clone() else {
        return c.skip("reg.moved.image", "no personal token");
    };
    let aks = match drill::incluster() {
        Ok(k) => k,
        Err(e) => return c.skip("reg.moved.image", &format!("no in-cluster client: {e:#}")),
    };
    let probe = c.probe_user.clone();
    let name = format!("{}-moved", c.prefix());
    let dir = c.tmp.join("img-moved");
    let host = super::super::registry::host(c);
    c.step("reg.moved.image", step_cap(ROLL_CAP), move |c| {
        let crane = super::super::registry::authed(c);
        let pods: Api<Pod> = Api::namespaced(aks.clone(), CENTRAL_NS);
        let sts: Api<StatefulSet> = Api::namespaced(aks.clone(), CENTRAL_NS);
        let dest = c.tmp.join("pull-moved");
        async move {
            let layer = super::super::registry::random_layer();
            super::super::registry::write_layout(&dir, &layer, &name).context("could not build the image")?;
            crane.login(&host, &probe, &secret).await.context("could not log in")?;
            let reference = format!("{host}/{probe}/{name}:latest");
            crane.push(&dir, &reference).await.context("could not push it")?;
            // Warm: this pull opens the image's database on whichever node owns it now.
            let _ = std::fs::remove_dir_all(&dest);
            crane.pull(&reference, &dest).await.context("the image would not pull before the move")?;
            // The tier ROLLS, the way a deploy rolls it — `roll.zero.errors`' own verb — rather
            // than the probe deleting every pod by name (2026-09-12): the ownership map moves
            // either way, but only the roll respects the StatefulSet's ordering, and only the
            // roll is the event this id claims to measure. Inside `undoing` like every other
            // fleet mutation: a body that times out mid-restart must still leave the tier waited
            // out rather than half rolled.
            let settle = || async { settled(&sts).await.context("the srv tier was left mid-restart") };
            let body = async {
                settled(&sts).await.context("the srv tier was already mid-roll, so this is not our roll")?;
                let before = pod_names(&pods).await?;
                start_roll(&sts).await?;
                wait_rolled(&pods, &sts, &before, ROLL_CAP - Duration::from_secs(120))
                    .await
                    .context("the tier did not come back after the roll")?;
                let _ = std::fs::remove_dir_all(&dest);
                crane
                    .pull(&reference, &dest)
                    .await
                    .context("the first pull after the image's database moved failed")
            };
            drill::undoing(ROLL_CAP, body, settle).await
        }
        .boxed()
    })
    .await;
}
