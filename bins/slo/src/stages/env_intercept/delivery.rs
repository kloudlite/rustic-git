//! The intercept itself, and every vantage point the intercepted service is dialled from: the
//! environment's own sibling, the workspace's port mapping, a SECOND owner's workspace in the same
//! team, the bench, a restart of the intercepting pod, the release and the automatic fallback.
//!
//! Every id in this file ends in a dial whose bytes are checked; the object assertions that sit
//! beside them (the proxy's uid, the policies a release must not leave) are there to catch the
//! regressions a working dial cannot see.

use super::*;

/// `env.intercept`: the environment's traffic to `TARGET:8080` is delivered to the workspace on
/// 3000, dialled by `TARGET`'s own name and port from a sibling pod.
pub(super) async fn intercept(c: &mut Ctx, j: &Journey) -> bool {
    let (e, w) = (j.env.clone(), j.ws.clone());
    c.step("env.intercept", INTERCEPT_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/environments/{e}/intercepts"));
        async move {
            post(c, &url, &jwt, intercept_body(&w, TARGET_PORT, WS_PORT)).await.context("could not intercept the service")?;
            // Polled, never asked once: the proxy pod has to be pulled and Ready and the ClusterIP's
            // endpoints have to name it, and none of that is synchronous with the answer.
            answers(c, &e, &workspace_dial(), MARKER, INTERCEPT_CEILING - SLACK).await
        }
        .boxed()
    })
    .await
}

/// `env.intercept.proxy.up`: the environment's own document says the proxy is `ready`, AND the
/// service answers with the workspace's marker.
///
/// The dial is half the id on purpose: `proxy: "ready"` is a state, and this whole journey exists
/// because a perfect set of objects delivered no packets. A state-only version of this step would
/// have passed on the mechanism the proxy replaced.
pub(super) async fn proxy_up(c: &mut Ctx, j: &Journey, held: bool) {
    if !held {
        return c.skip("env.intercept.proxy.up", NOT_HELD);
    }
    let e = j.env.clone();
    c.step("env.intercept.proxy.up", PROXY_UP_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let read = api(c, &format!("/v1/environments/{e}"));
        async move {
            // Short of the step's own ceiling, so a proxy that never reports ready says so rather
            // than being cut off by the timeout with nothing recorded.
            poll_json(c, &read, &jwt, PROXY_UP_CEILING - SLACK, |v| proxy_state(v, TARGET).as_deref() == Some("ready"))
                .await
                .context("the proxy never reported ready")?;
            let out = dial(c, &e, &workspace_dial()).await?;
            if !out.contains(MARKER) {
                return Err(anyhow!("the proxy reports ready and {TARGET}:{TARGET_PORT} answered {:?}", super::super::clip(out.trim())));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `env.intercept.delivered`: the plain question, asked from inside the environment.
pub(super) async fn delivered(c: &mut Ctx, j: &Journey, held: bool) {
    if !held {
        return c.skip("env.intercept.delivered", NOT_HELD);
    }
    let e = j.env.clone();
    c.step("env.intercept.delivered", INTERCEPT_CEILING, move |c| {
        async move { answers(c, &e, &workspace_dial(), MARKER, INTERCEPT_CEILING - SLACK).await }.boxed()
    })
    .await;
}

/// `env.intercept.remap`: the workspace listens on 3000, the environment dials 8080, and 3000 is
/// NOT what the environment reaches — the remap is real, not two ports that happen to agree.
pub(super) async fn remap(c: &mut Ctx, j: &Journey, held: bool) {
    if !held {
        return c.skip("env.intercept.remap", NOT_HELD);
    }
    let (e, w, ns) = (j.env.clone(), j.ws.clone(), j.ws_ns(&c.probe_user));
    c.step("env.intercept.remap", REMAP_CEILING, move |c| {
        async move {
            // The listener is where the mapping says it is, judged from inside the workspace.
            let (_, out, err) = ws_exec(c, &ns, &w, &http_get("127.0.0.1", WS_PORT), EXEC_CEILING).await?;
            if !out.contains(MARKER) {
                return Err(anyhow!("the workspace does not answer on {WS_PORT}: {:?} {}", super::super::clip(out.trim()), err.trim()));
            }
            // The workspace's OWN port, dialled at the service's name, reaches nothing: only the
            // ports the intercept maps are carried.
            let leaked = dial(c, &e, &http_get(TARGET, WS_PORT)).await.unwrap_or_default();
            if leaked.contains(MARKER) {
                return Err(anyhow!("{TARGET}:{WS_PORT} answered the workspace's marker, so the ports were never remapped"));
            }
            // And the address a caller actually dials still answers. The id ends in the dial.
            answers(c, &e, &workspace_dial(), MARKER, REMAP_CEILING - SLACK).await
        }
        .boxed()
    })
    .await;
}

/// `env.intercept.peer` — **the bug, as a probe.**
///
/// This would have failed every hour since spaces shipped: the follower's packets were DNAT'd to a
/// pod in another owner's namespace, whose ingress policy admitted only the environment's.
///
/// The second owner is a member of this journey's team and their space already follows its
/// environment (`stand_up`); what is left is a workspace of theirs and one dial. Answers the
/// workspace's id so teardown can take it before the team.
pub(super) async fn peer(c: &mut Ctx, j: &Journey, held: bool) -> Option<String> {
    if !held {
        c.skip("env.intercept.peer", NOT_HELD);
        return None;
    }
    // Outside the step: standing the follower up is not what the 120 s is the ceiling for.
    let name = format!("{}-peerws", c.prefix());
    let peer_ws = match create_as(c, &c.other_jwt.clone(), &name, &j.team).await {
        Ok(id) => id,
        Err(e) => {
            c.skip("env.intercept.peer", &format!("the follower's workspace never came up: {e:#}"));
            return None;
        }
    };
    // Recorded before the dial: the workspace exists whatever the answer is, and the prefix sweep
    // has to find it even if this run dies here.
    c.state.extra_workspaces.push(peer_ws.clone());
    let (id, ns, env_ns) = (peer_ws.clone(), j.ws_ns(&c.other_user), env_namespace(&j.env));
    c.step("env.intercept.peer", INTERCEPT_CEILING, move |c| {
        async move {
            // Fully qualified: the follower is being asked about REACHABILITY, not about whose
            // resolv.conf carries which search domain.
            let script = http_get(&format!("{TARGET}.{env_ns}.svc.cluster.local"), TARGET_PORT);
            let start = std::time::Instant::now();
            loop {
                let out = ws_exec(c, &ns, &id, &script, EXEC_CEILING).await.map(|(_, o, _)| o).unwrap_or_default();
                if out.contains(MARKER) {
                    return Ok(());
                }
                if start.elapsed() + SLACK >= INTERCEPT_CEILING {
                    return Err(anyhow!("the follower never reached the intercepted service: {:?}", super::super::clip(out.trim())));
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
        .boxed()
    })
    .await;
    Some(peer_ws)
}

/// `env.intercept.bench`: the probe owner's bench in this team — a pod of the same space, one layer
/// further from a workspace — reaches the intercepted service, through the bench's own exec path.
///
/// `node`, because the bench image is the HARNESS's, not the workspace's: it ships no curl, wget
/// or nc (the three this used to try, all absent on the fleet 2026-09-15). The answer is judged on
/// the marker, so a bench that loses node too fails loudly rather than passing on an exit code.
pub(super) async fn bench_reaches(c: &mut Ctx, j: &Journey, held: bool) {
    if !held {
        return c.skip("env.intercept.bench", NOT_HELD);
    }
    if let Some(why) = &j.bench {
        return c.skip("env.intercept.bench", &why.clone());
    }
    let host = format!("{TARGET}.{}.svc.cluster.local", env_namespace(&j.env));
    let ns = j.ws_ns(&c.probe_user);
    c.step("env.intercept.bench", BENCH_DIAL_CEILING, move |c| {
        async move {
            let script = super::node_get(&format!("http://{host}:{TARGET_PORT}/"));
            let start = std::time::Instant::now();
            loop {
                let out = bench_exec(c, &ns, &["node", "-e", &script]).await.unwrap_or_default();
                if out.contains(MARKER) {
                    return Ok(());
                }
                if start.elapsed() + SLACK >= BENCH_DIAL_CEILING {
                    return Err(anyhow!("the bench never reached the intercepted service: {:?}", super::super::clip(out.trim())));
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
        .boxed()
    })
    .await;
}

/// `env.intercept.proxy.restart`: the INTERCEPTING WORKSPACE's pod is killed, and delivery comes
/// back without the proxy being recreated.
///
/// The uid assertion is the whole claim of a proxy over baked-in addressing: the workspace's pod
/// returns on a different IP, and what absorbs it is the target Service the proxy dials by name.
/// A regression that re-rendered the proxy around the new address would still deliver — and would
/// be caught here alone.
pub(super) async fn proxy_restart(c: &mut Ctx, j: &Journey, held: bool) {
    if !held {
        return c.skip("env.intercept.proxy.restart", NOT_HELD);
    }
    let before = match proxy_uid(c, &j.env).await {
        Ok(uid) => uid,
        Err(e) => return c.skip("env.intercept.proxy.restart", &format!("the proxy pod could not be read: {e:#}")),
    };
    let (e, w, ns) = (j.env.clone(), j.ws.clone(), j.ws_ns(&c.probe_user));
    c.step("env.intercept.proxy.restart", RESTART_CEILING, move |c| {
        async move {
            let k = c.kube.clone().ok_or_else(|| anyhow!("no kubeconfig"))?;
            let pods: kube::Api<Pod> = kube::Api::namespaced(k, &ns);
            pods.delete(&w, &Default::default()).await.context("could not delete the intercepting workspace's pod")?;
            // The pod comes back empty: the listener is a process, not a file.
            listen(c, &ns, &w).await.context("the listener never came back in the restarted pod")?;
            answers(c, &e, &workspace_dial(), MARKER, RESTART_CEILING - SLACK).await?;
            let after = proxy_uid(c, &e).await.context("the proxy pod is gone after the restart")?;
            if after != before {
                return Err(anyhow!("the proxy pod was recreated ({before} -> {after}), so something addressed the workspace pod directly"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `env.intercept.release`: the wish is deleted, and all four things a release owes are true —
/// the real service answers, its StatefulSet is back, the proxy is gone, and no grant is left.
///
/// The policies are listed BY LABEL, never by guessing a name: a grant left behind under a name
/// this probe does not know is exactly the leak worth catching.
pub(super) async fn release(c: &mut Ctx, j: &Journey, held: bool) {
    if !held {
        return c.skip("env.intercept.release", NOT_HELD);
    }
    let (e, w, ws_ns) = (j.env.clone(), j.ws.clone(), j.ws_ns(&c.probe_user));
    c.step("env.intercept.release", RELEASE_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/environments/{e}/intercepts/{TARGET}"));
        async move {
            call(c, reqwest::Method::DELETE, &url, &jwt, None).await.context("could not release the intercept")?;
            // Each half stops short of the step's ceiling, so a breach names the half that missed.
            answers(c, &e, &service_dial(), "PONG", RELEASE_CEILING - SLACK).await?;
            sts_ready(c, &e, TARGET, RELEASE_CEILING - SLACK)
                .await
                .context("the real service never came back to a ready replica")?;
            let env_ns = env_namespace(&e);
            match intercept_pods(c, &env_ns).await? {
                Some(left) if !left.is_empty() => return Err(anyhow!("the release left the proxy pod {left:?} in {env_ns}")),
                _ => {}
            }
            for ns in [env_ns, ws_ns] {
                let grants = intercept_policies(c, &ns).await?;
                if !grants.is_empty() {
                    return Err(anyhow!("the release left the grant {grants:?} in {ns}"));
                }
            }
            // The wish is gone too — this is the one verb that removes it.
            let doc = get(c, &api(c, &format!("/v1/environments/{e}")), &jwt).await?;
            if wished(&doc, TARGET) {
                return Err(anyhow!("the intercept of {TARGET} by {w} is still wished after the release"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `env.intercept.fallback`: the workspace is STOPPED through `/v1` — never released by hand,
/// which would not exercise this path at all — and the real service comes back on its own.
///
/// Both halves are asserted, because they are one rule: what is IN FORCE goes away, and the WISH
/// stays. A fallback that also cleared `spec.intercepts` would silently throw away what the
/// person asked for, and the traffic assertion alone passes straight through that.
///
/// The intercept `release` removed is put back FIRST, outside the step: re-converging is not what
/// this ceiling is for.
pub(super) async fn fallback(c: &mut Ctx, j: &Journey, held: bool) {
    if !held {
        return c.skip("env.intercept.fallback", NOT_HELD);
    }
    let url = api(c, &format!("/v1/environments/{}/intercepts", j.env));
    let again = async {
        post(c, &url, &c.probe_jwt.clone(), intercept_body(&j.ws, TARGET_PORT, WS_PORT)).await?;
        answers(c, &j.env, &workspace_dial(), MARKER, INTERCEPT_CEILING).await
    }
    .await;
    if let Err(e) = again {
        return c.skip("env.intercept.fallback", &format!("the intercept could not be put back after the release: {e:#}"));
    }
    let (e, w) = (j.env.clone(), j.ws.clone());
    c.step("env.intercept.fallback", FALLBACK_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let stop = api(c, &format!("/v1/workspaces/{w}/stop"));
        let read = api(c, &format!("/v1/environments/{e}"));
        async move {
            post(c, &stop, &jwt, Value::Null).await.context("could not stop the intercepting workspace")?;
            answers(c, &e, &service_dial(), "PONG", FALLBACK_CEILING - SLACK).await?;
            let doc = get(c, &read, &jwt).await.context("could not read the environment back")?;
            if let Some(by) = intercepted_by(&doc, TARGET) {
                return Err(anyhow!("{TARGET} answers for itself again and still reports intercepted by {by}"));
            }
            if !wished(&doc, TARGET) {
                return Err(anyhow!("stopping the workspace dropped the intercept from the environment's spec"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}
