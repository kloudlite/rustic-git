//! The 2026-09-06 coverage review's weekly ids: the deploy, the two registry paths real clients
//! use and nothing walked, the sweeps that touch user data, and the gateway's own ceilings.
//!
//! A second file beside `weekly` for the same reason `experience_gaps2` sits beside
//! `experience_gaps`. Every rule of the stage still holds: one sample per id on every path, a
//! missing precondition is a SKIP, and anything that changes the fleet is paired with its undo.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams, Patch, PatchParams};
use serde_json::{json, Value};

use super::{admin, api, get, poll_json, post};
use crate::ctx::Ctx;
use crate::{drill, tools};

/// The namespace and workload names on AKS. Repeated rather than derived: what these steps are
/// about is the objects the deployment actually carries.
const CENTRAL_NS: &str = "kloudlite";
const SRV: &str = "kloudlite-srv";
/// The srv PODS, which do not carry the StatefulSet's name as their `app`: `deploy/kloudlite.yaml`
/// labels them `app: kloudlite, role: server`. Selecting `app=kloudlite-srv` matched nothing, so
/// `srv.drain.handover` found "no srv pod to drain" and `roll.zero.errors` tracked no pod at all
/// and passed with nothing observed.
const SRV_PODS: &str = "app=kloudlite,role=server";
/// The srv container's `http` port (`deploy/kloudlite.yaml`), the one `/healthz` answers on and
/// the one the network policy opens to everybody. This dialled 3000 — the WEB app's port — so
/// `srv.drain.handover` reported the leaving pod as never having answered at all.
const SRV_HTTP_PORT: u16 = 8080;

/// The roll's own budget: a StatefulSet of a handful of pods, each with a 90 s grace period for
/// its handover. The step gets a minute on top, as every step with an undo does.
const ROLL_CAP: Duration = Duration::from_secs(600);
const DRAIN_CAP: Duration = Duration::from_secs(60);
const READ_CEILING: Duration = Duration::from_secs(60);
const SWEEP_CAP: Duration = Duration::from_secs(180);
/// `gw.caps` alone: two 30 s `ssh` runs for the replay half, eleven session mints, ten tunnels
/// spawned a beat apart and a 20 s wait on the eleventh. 300 s is that with room, and the id is
/// availability — a step cut off by its own ceiling would report the probe, not the gateway.
const TUNNEL_CEILING: Duration = Duration::from_secs(300);

fn step_cap(body: Duration) -> Duration {
    body + Duration::from_secs(60)
}

/// `cold` is `ws.cold.profile`'s workspace, the one workspace of this run that is still there by
/// stage 12: stage 7's lifecycle verbs DELETE `State::workspace` and its volume (`wt.delete`,
/// `snap.delete`), so the three ids here that used to read them answered 404 every time.
pub async fn run(c: &mut Ctx, cold: Option<&str>) {
    roll_zero_errors(c).await;
    drain_handover(c).await;
    moved_image(c).await;
    blob_session(c).await;
    gc_packs(c).await;
    limits(c).await;
    workload_roll(c).await;
    spread(c, cold).await;
    retain(c, cold).await;
    janitor(c).await;
    lanes(c).await;
    gw_caps(c, cold).await;
}

// ── the deploy ──────────────────────────────────────────────────────────

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
async fn roll_zero_errors(c: &mut Ctx) {
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
async fn watch_roll(
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
async fn routed(c: &Ctx, url: &str, jwt: &str) -> std::result::Result<(), String> {
    for attempt in 0..RETRIES {
        match super::raw(c, reqwest::Method::GET, url, jwt, None, &[]).await {
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

/// How many times a 421 is re-issued before it counts against the id. Three tries over a second is
/// more patience than a git client has and far less than the roll takes.
const RETRIES: usize = 3;

/// How often the loop pushes, rather than every beat: a push is a pack negotiation, and one every
/// two seconds would measure the probe's own git rather than the fleet.
const PUSH_EVERY: Duration = Duration::from_secs(10);

/// The push half of the loop: one commit onto its own branch, over HTTP, each time it is asked.
struct Pushing {
    work: std::path::PathBuf,
    url: String,
    branch: String,
    n: std::sync::atomic::AtomicUsize,
}

impl Pushing {
    async fn once(&self, c: &Ctx) -> Result<()> {
        let i = self.n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::fs::write(self.work.join("roll.txt"), format!("{i}\n")).context("could not write")?;
        super::git::git(c, vec!["add".into(), "-A".into()], Some(&self.work)).await?;
        super::git::git(c, vec!["commit".into(), "-q".into(), "-m".into(), format!("roll {i}")], Some(&self.work)).await?;
        let refspec = format!("HEAD:refs/heads/{}", self.branch);
        super::git::git(c, super::git::authed(c, &["push", "-q", &self.url, &refspec]), Some(&self.work)).await.map(|_| ())
    }
}

fn uid_of(p: &Pod) -> String {
    p.metadata.uid.clone().unwrap_or_else(|| kube::ResourceExt::name_any(p))
}

/// The live srv pods as `(name, uid)`. The uid is what identity means across a StatefulSet roll —
/// the name comes back, the uid never does.
async fn pod_names(pods: &Api<Pod>) -> Result<Vec<(String, String)>> {
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
async fn settled(sts: &Api<StatefulSet>) -> Result<()> {
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

/// `srv.drain.handover`: the endpoint's own contract, without a whole roll.
///
/// The drain is triggered the way the deployment triggers it — the pod's preStop hook, fired by
/// deleting one pod — because `POST /peer/v1/drain` is peer-only and the probe holds no peer
/// secret, deliberately (`sec.peer.listener` is the id that says so). What is asserted is what a
/// person would see: the leaving pod reports itself unhealthy as `draining`, so the Service stops
/// sending it traffic, and a repo it may have owned still reads throughout.
async fn drain_handover(c: &mut Ctx) {
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
                // And the repo still reads, which is the half a person would notice.
                let (status, text) = super::raw(c, reqwest::Method::GET, &refs, &jwt, None, &[]).await?;
                if !status.is_success() {
                    return Err(anyhow!("a repo stopped reading while a pod drained: {status}: {}", text.chars().take(120).collect::<String>()));
                }
                if !saw {
                    return Err(anyhow!("{victim} never reported `draining` on /healthz"));
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
async fn draining(c: &Ctx, ip: &str, cap: Duration) -> Result<bool> {
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
async fn moved_image(c: &mut Ctx) {
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
    let host = super::registry::host(c);
    c.step("reg.moved.image", step_cap(ROLL_CAP), move |c| {
        let crane = super::registry::authed(c);
        let pods: Api<Pod> = Api::namespaced(aks.clone(), CENTRAL_NS);
        let sts: Api<StatefulSet> = Api::namespaced(aks.clone(), CENTRAL_NS);
        let dest = c.tmp.join("pull-moved");
        async move {
            let layer = super::registry::random_layer();
            super::registry::write_layout(&dir, &layer, &name).context("could not build the image")?;
            crane.login(&host, &probe, &secret).await.context("could not log in")?;
            let reference = format!("{host}/{probe}/{name}:latest");
            crane.push(&dir, &reference).await.context("could not push it")?;
            // Warm: this pull opens the image's database on whichever node owns it now.
            let _ = std::fs::remove_dir_all(&dest);
            crane.pull(&reference, &dest).await.context("the image would not pull before the move")?;
            // Every pod out and back, one at a time — the ownership map moves with them. Inside
            // `undoing` like every other fleet mutation: a body that times out mid-restart must
            // still leave the tier waited out rather than half rolled.
            let settle = || async { settled(&sts).await.context("the srv tier was left mid-restart") };
            let body = async {
                for (pod, _) in pod_names(&pods).await? {
                    pods.delete(&pod, &kube::api::DeleteParams::default())
                        .await
                        .map_err(|e| anyhow!("could not restart {pod}: {e}"))?;
                    settled(&sts).await.with_context(|| format!("the tier did not come back after {pod}"))?;
                }
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

/// `reg.blob.session`: the three `/v2` verbs real clients use and nothing probed.
///
/// A chunked upload resumed and finished, a session cancelled, a blob deleted, and `referrers`
/// answered — all of them paths where `Digest::parse` is the only thing between a path segment and
/// an object-store key, which is why they are worth a sample of their own rather than being left
/// to `crane`, which uses none of them.
async fn blob_session(c: &mut Ctx) {
    let Some(secret) = c.state.token_value.clone() else {
        return c.skip("reg.blob.session", "no personal token");
    };
    let probe = c.probe_user.clone();
    let name = format!("{}-sess", c.prefix());
    let dir = c.tmp.join("img-sess");
    let host = super::registry::host(c);
    c.step("reg.blob.session", READ_CEILING, move |c| {
        let crane = super::registry::authed(c);
        let base = super::registry::base(c);
        async move {
            // A real image first: `referrers` answers about a manifest that exists, and the whole
            // session dance below runs in a repository the token already has a scope for.
            let layer = super::registry::random_layer();
            let digest = super::registry::write_layout(&dir, &layer, &name).context("could not build the image")?;
            crane.login(&host, &probe, &secret).await.context("could not log in")?;
            crane.push(&dir, &format!("{host}/{probe}/{name}:latest")).await.context("could not push it")?;
            let token = super::registry::bearer(c, Some(&secret), &format!("repository:{probe}/{name}:pull,push"))
                .await
                .context("could not mint a registry token")?;
            let v2 = format!("{base}/v2/{probe}/{name}");

            // 1 · a chunked upload, in two PATCHes with a status read between them.
            let body: Vec<u8> = (0..2048u32).map(|i| (i % 251) as u8).collect();
            let (half, rest) = body.split_at(1024);
            let want = super::registry::sha256(&body);
            let session = start_upload(c, &v2, &token).await?;
            patch_chunk(c, &session, &token, half, 0).await.context("the first chunk")?;
            let (status, _, range) = raw_v2(c, reqwest::Method::GET, &session, &token, None).await?;
            if status.as_u16() != 204 || range.is_empty() {
                return Err(anyhow!("an upload session's status answered {status} with range {range:?}"));
            }
            patch_chunk(c, &session, &token, rest, half.len()).await.context("the second chunk")?;
            let put = format!("{session}{}digest={want}", if session.contains('?') { "&" } else { "?" });
            let (status, text, _) = raw_v2(c, reqwest::Method::PUT, &put, &token, Some(vec![])).await?;
            if !status.is_success() {
                return Err(anyhow!("finishing the chunked upload answered {status}: {}", text.chars().take(160).collect::<String>()));
            }

            // 2 · it is really there, then really gone — the client DELETE, which no other id walks.
            let blob = format!("{v2}/blobs/{want}");
            let (status, _, _) = raw_v2(c, reqwest::Method::HEAD, &blob, &token, None).await?;
            if !status.is_success() {
                return Err(anyhow!("the finished blob answered {status}"));
            }
            let (status, text, _) = raw_v2(c, reqwest::Method::DELETE, &blob, &token, None).await?;
            if !status.is_success() {
                return Err(anyhow!("deleting a blob answered {status}: {}", text.chars().take(160).collect::<String>()));
            }
            let (status, _, _) = raw_v2(c, reqwest::Method::HEAD, &blob, &token, None).await?;
            if status.as_u16() != 404 {
                return Err(anyhow!("a deleted blob still answers {status}"));
            }

            // 3 · a cancelled session leaves nothing behind.
            let doomed = start_upload(c, &v2, &token).await?;
            let (status, _, _) = raw_v2(c, reqwest::Method::DELETE, &doomed, &token, None).await?;
            if !status.is_success() {
                return Err(anyhow!("cancelling an upload session answered {status}"));
            }
            let (status, _, _) = raw_v2(c, reqwest::Method::GET, &doomed, &token, None).await?;
            if status.as_u16() != 404 {
                return Err(anyhow!("a cancelled session still answers {status}"));
            }

            // 4 · referrers, about the manifest the push above left.
            let (status, text, _) =
                raw_v2(c, reqwest::Method::GET, &format!("{v2}/referrers/{digest}"), &token, None).await?;
            if !status.is_success() {
                return Err(anyhow!("referrers answered {status}: {}", text.chars().take(160).collect::<String>()));
            }
            let doc: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            doc.get("manifests")
                .and_then(Value::as_array)
                .map(|_| ())
                .ok_or_else(|| anyhow!("referrers did not answer an index"))
        }
        .boxed()
    })
    .await;
}

/// `POST /v2/{o}/{n}/blobs/uploads/`, answering the session URL the Location header names.
async fn start_upload(c: &Ctx, v2: &str, token: &str) -> Result<String> {
    let r = c
        .http
        .post(format!("{v2}/blobs/uploads/"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-length", "0")
        .send()
        .await
        .map_err(|e| anyhow!("could not start an upload: {}", e.without_url()))?;
    let status = r.status();
    let location = r.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    if status.as_u16() != 202 || location.is_empty() {
        return Err(anyhow!("starting an upload answered {status} with location {location:?}"));
    }
    // The registry may answer an absolute URL or a path; both are legal, and only one is a URL.
    Ok(match location.starts_with("http") {
        true => location,
        false => format!("{}{location}", v2.split("/v2/").next().unwrap_or_default()),
    })
}

async fn patch_chunk(c: &Ctx, session: &str, token: &str, bytes: &[u8], from: usize) -> Result<()> {
    let r = c
        .http
        .patch(session)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/octet-stream")
        .header("content-range", format!("{from}-{}", from + bytes.len() - 1))
        .body(bytes.to_vec())
        .send()
        .await
        .map_err(|e| anyhow!("{}", e.without_url()))?;
    let status = r.status();
    if !status.is_success() {
        return Err(anyhow!("answered {status}"));
    }
    Ok(())
}

/// One `/v2` request, answering `(status, body, range header)`.
async fn raw_v2(
    c: &Ctx,
    method: reqwest::Method,
    url: &str,
    token: &str,
    body: Option<Vec<u8>>,
) -> Result<(reqwest::StatusCode, String, String)> {
    let mut req = c.http.request(method, url).header("authorization", format!("Bearer {token}"));
    if let Some(b) = body {
        req = req.header("content-length", b.len().to_string()).body(b);
    }
    let r = req.send().await.map_err(|e| anyhow!("{}", e.without_url()))?;
    let status = r.status();
    let range = r.headers().get("range").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    Ok((status, r.text().await.unwrap_or_default(), range))
}

/// `git.limits`: the three body ceilings are three knobs, and a 413 comes from the right one.
///
/// Only the manifest limit is testable in band — `max_layer` is 5 GiB and the git `max_body` is
/// 2 GiB, and a probe that sent either would be measuring the CronJob's disk. What the id catches
/// is the failure that has actually confused people: the three limits collapsing into one, which
/// shows immediately as a blob of manifest size being refused, or a manifest of blob size accepted.
async fn limits(c: &mut Ctx) {
    let Some(secret) = c.state.token_value.clone() else {
        return c.skip("git.limits", "no personal token");
    };
    let probe = c.probe_user.clone();
    let name = format!("{}-limits", c.prefix());
    let dir = c.tmp.join("img-limits");
    let host = super::registry::host(c);
    c.step("git.limits", READ_CEILING, move |c| {
        let crane = super::registry::authed(c);
        let base = super::registry::base(c);
        async move {
            let layer = super::registry::random_layer();
            super::registry::write_layout(&dir, &layer, &name).context("could not build the image")?;
            crane.login(&host, &probe, &secret).await.context("could not log in")?;
            crane.push(&dir, &format!("{host}/{probe}/{name}:latest")).await.context("could not push it")?;
            let token = super::registry::bearer(c, Some(&secret), &format!("repository:{probe}/{name}:pull,push"))
                .await
                .context("could not mint a registry token")?;
            let v2 = format!("{base}/v2/{probe}/{name}");
            // A manifest over its own 4 MiB ceiling: refused, and by the MANIFEST limit.
            let big = vec![b'x'; OVER_MANIFEST];
            let r = c
                .http
                .put(format!("{v2}/manifests/too-big"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/vnd.oci.image.manifest.v1+json")
                .body(big.clone())
                .send()
                .await
                .map_err(|e| anyhow!("{}", e.without_url()))?;
            if r.status().as_u16() != 413 {
                return Err(anyhow!("a {OVER_MANIFEST}-byte manifest answered {}, not 413", r.status()));
            }
            // The same number of bytes as a BLOB: accepted, because that limit is a different one.
            let session = start_upload(c, &v2, &token).await?;
            let digest = super::registry::sha256(&big);
            let put = format!("{session}{}digest={digest}", if session.contains('?') { "&" } else { "?" });
            let (status, text, _) = raw_v2(c, reqwest::Method::PUT, &put, &token, Some(big)).await?;
            if !status.is_success() {
                return Err(anyhow!(
                    "a {OVER_MANIFEST}-byte blob was refused {status}: the layer limit has been collapsed into the manifest one: {}",
                    text.chars().take(160).collect::<String>()
                ));
            }
            let _ = raw_v2(c, reqwest::Method::DELETE, &format!("{v2}/blobs/{digest}"), &token, None).await;
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// A megabyte past `manifests::MAX_MANIFEST` (4 MiB). Repeated rather than imported: what is being
/// asserted is the limit the DEPLOYED tier holds, and a constant shared with it would move with it.
const OVER_MANIFEST: usize = 5 * 1024 * 1024;

// ── the sweeps over user data ───────────────────────────────────────────

/// `git.gc.packs`: the repo survives a consolidation pass unchanged.
///
/// Five sweeps run over user data with no coverage at all — the worker's GC lane (merge-cache
/// prune, image sweep, marker reconcile, repo-owner reconcile, stale upload sweep) and the server's
/// pack consolidation. A sweep that took the wrong pack loses a person's history, and the way to
/// see it is the only way that matters: push, wait a pass out, clone, compare.
/// Poll `check` every few seconds until it answers `Ok(None)`, and say how long that took — the
/// number a person would feel. Replaces the fixed `sleep(SWEEP_CAP - 60 s)` the sweep steps used
/// to take: a sleep asserted after a wait it chose, and reported nothing about how long the fleet
/// actually needed. `Some(why)` is what still stands in the way; at `cap` that is the reason.
async fn settle<F, Fut>(cap: Duration, what: &str, mut check: F) -> Result<Duration>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Option<String>>>,
{
    let start = std::time::Instant::now();
    loop {
        match check().await? {
            None => return Ok(start.elapsed()),
            Some(why) if start.elapsed() >= cap => return Err(anyhow!("{what} after {} s: {why}", cap.as_secs())),
            Some(_) => tokio::time::sleep(Duration::from_secs(5)).await,
        }
    }
}

async fn gc_packs(c: &mut Ctx) {
    let (Some(repo), probe) = (c.state.repo.clone(), c.probe_user.clone()) else {
        return c.skip("git.gc.packs", "no repo");
    };
    let work = c.tmp.join("git").join(&repo);
    if !work.is_dir() {
        return c.skip("git.gc.packs", "stage 2 left no working tree");
    }
    c.step("git.gc.packs", step_cap(SWEEP_CAP), move |c| {
        let jwt = c.probe_jwt.clone();
        let listing = api(c, &format!("/v1/repos?owner={probe}"));
        let http = format!("{}/{probe}/{repo}.git", c.cfg.git_url.trim_end_matches('/'));
        let dest = c.tmp.join("gc-clone");
        async move {
            // A handful of small commits, so consolidation has several packs to fold.
            for i in 0..5 {
                std::fs::write(work.join("gc.txt"), format!("{i}")).context("could not write")?;
                let g = |a: Vec<String>| super::git::git(c, a, Some(&work));
                g(vec!["add".into(), "-A".into()]).await?;
                g(vec!["commit".into(), "-q".into(), "-m".into(), format!("gc {i}")]).await?;
            }
            super::git::git(c, super::git::authed(c, &["push", "-q", &http, "HEAD:refs/heads/gc"]), Some(&work))
                .await
                .context("the push failed")?;
            let want = super::git::git(c, vec!["rev-parse".into(), "HEAD^{tree}".into()], Some(&work)).await?;
            // A whole pass: the lane walks every repo in turn, and a check that raced it would say
            // nothing about the sweep at all.
            tokio::time::sleep(SWEEP_CAP - Duration::from_secs(60)).await;
            let _ = std::fs::remove_dir_all(&dest);
            super::git::git(c, super::git::authed(c, &["clone", "-q", "--branch", "gc", &http, &dest.display().to_string()]), None)
                .await
                .context("the repo would not clone after a consolidation pass")?;
            let got = super::git::git(c, vec!["rev-parse".into(), "HEAD^{tree}".into()], Some(&dest)).await?;
            if got.trim() != want.trim() {
                return Err(anyhow!("the tree changed across a consolidation pass: {} became {}", want.trim(), got.trim()));
            }
            // And the index markers still list it: the marker reconcile is one of the five sweeps,
            // and a repo that vanished from the listing is invisible to every page in the app.
            let rows = get(c, &listing, &jwt).await.context("could not list the repos")?;
            let listed = rows
                .get("repos")
                .and_then(Value::as_array)
                .or_else(|| rows.as_array())
                .is_some_and(|rs| rs.iter().any(|r| r.get("name").and_then(Value::as_str) == Some(repo.as_str())));
            if !listed {
                return Err(anyhow!("the repo is no longer listed after a sweep"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `snap.retain`: retention prunes sync points and never a push.
///
/// The rule is one Ready transient per worktree and a push that is never pruned — which is the
/// difference between a replica having something recent to fetch and a person losing the cut they
/// asked for. Both halves are read off the CRDs, because history deliberately does not list sync
/// points at all.
async fn retain(c: &mut Ctx, cold: Option<&str>) {
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
            let pushes = Some(super::experience_env::push_once(c, &ws, "retain").await.context("could not push")?);
            let volume = get(c, &doc, &jwt)
                .await
                .context("could not read the workspace")?
                .get("volume")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("the workspace names no volume after a push"))?
                .to_string();
            let history = api(c, &format!("/v1/volumes/{volume}/history"));
            // Long enough that several sync beats have certainly cut and pruned.
            tokio::time::sleep(SWEEP_CAP - Duration::from_secs(60)).await;
            let api: kube::Api<crd::Snapshot> = kube::Api::all(k.clone());
            let all = api
                .list(&kube::api::ListParams::default())
                .await
                .map_err(|e| anyhow!("could not list the snapshots: {e}"))?;
            let mine: Vec<&crd::Snapshot> =
                all.items.iter().filter(|s| s.spec.volume == volume).collect();
            let mut per_worktree: std::collections::HashMap<String, usize> = Default::default();
            for s in mine.iter().filter(|s| s.spec.transient) {
                let ready = s.status.as_ref().is_some_and(|st| st.phase == crd::Phase::Ready);
                if ready {
                    *per_worktree.entry(s.spec.worktree.clone()).or_default() += 1;
                }
            }
            if let Some((wt, n)) = per_worktree.iter().find(|(_, n)| **n > 1) {
                return Err(anyhow!("{n} Ready sync points remain for worktree {wt:?}: retain has stopped pruning"));
            }
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
async fn janitor(c: &mut Ctx) {
    let Some(k) = c.kube.clone() else { return c.skip("agent.janitor", "no kubeconfig") };
    let prefix = c.prefix();
    c.step("agent.janitor", step_cap(SWEEP_CAP), move |_| {
        async move {
            use kloudlite_workspaces::crd;
            tokio::time::sleep(SWEEP_CAP - Duration::from_secs(60)).await;
            let vols: kube::Api<crd::Volume> = kube::Api::all(k.clone());
            let snaps: kube::Api<crd::Snapshot> = kube::Api::all(k.clone());
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
            if !orphans.is_empty() {
                return Err(anyhow!("a snapshot record outlived its volume: {}", orphans.join(", ")));
            }
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
async fn lanes(c: &mut Ctx) {
    let Some(secret) = c.state.token_value.clone() else {
        return c.skip("srv.lanes", "no personal token");
    };
    let probe = c.probe_user.clone();
    let name = format!("{}-lanes", c.prefix());
    let dir = c.tmp.join("img-lanes");
    let host = super::registry::host(c);
    c.step("srv.lanes", step_cap(SWEEP_CAP), move |c| {
        let crane = super::registry::authed(c);
        let jwt = c.probe_jwt.clone();
        // The TAG rows, not the images listing: `pulls` is a per-tag counter and only
        // `imagetags` carries it — the listing rows are markers (name, manifests, visibility) and
        // polling them for a field they never have is a step that cannot pass.
        let tags = api(c, &format!("/api/{probe}/{name}/imagetags"));
        let dest = c.tmp.join("pull-lanes");
        async move {
            let layer = super::registry::random_layer();
            super::registry::write_layout(&dir, &layer, &name).context("could not build the image")?;
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
async fn spread(c: &mut Ctx, cold: Option<&str>) {
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
            super::experience_env::push_once(c, &ws, "spread").await.context("could not push")?;
            let before = get(c, &doc, &jwt).await.context("could not read the workspace")?;
            let was = before.get("placement").and_then(Value::as_str).unwrap_or_default().to_string();
            let volume = before
                .get("volume")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("the workspace names no volume after a push"))?
                .to_string();
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
/// The node the agent's own spread rule picks for `volume`'s next start: rendezvous over
/// `{owner} ∪ {nodes up to date for the worktree}`, exactly `peer::preferred_node` — the same
/// hash (`replicate::targets`) and the same up-to-date test (`VolumeReplica.status.branches`
/// names the newest Ready transient). Recomputed here rather than read back because the agent
/// records no "preferred" anywhere: the contract is the rule, and this is the rule.
async fn rendezvous_choice(k: &kube::Client, volume: &str, ws: &str, owner: &str) -> Result<(String, Vec<String>)> {
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
async fn placeable_nodes(k: &kube::Client) -> Result<usize> {
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

/// `gw.caps`: the two things between one person and the region's gateway.
///
/// A connect token is spent on use (`Gateway::spend`) and a workspace may hold ten tunnels; both
/// are refusals, and a gateway that stopped enforcing either would look perfectly healthy to every
/// other id. The replay half is first because it costs one request; the cap half opens ten real
/// tunnels and requires the eleventh to be refused, then closes all of them.
async fn gw_caps(c: &mut Ctx, cold: Option<&str>) {
    let Some(ws) = cold.map(str::to_string) else {
        return c.skip("gw.caps", "no cold workspace");
    };
    let key = c.cfg.ssh_key_path.clone();
    c.step("gw.caps", TUNNEL_CEILING, move |c| {
        async move {
            // Started first: stage 7 parks the workspace's pod as soon as its own ids are done
            // (the region's nodes cannot hold four at once), and a tunnel needs something running.
            let doc = api(c, &format!("/v1/workspaces/{ws}"));
            post(c, &api(c, &format!("/v1/workspaces/{ws}/start")), &c.probe_jwt.clone(), Value::Null)
                .await
                .context("could not start the workspace to tunnel into")?;
            poll_json(c, &doc, &c.probe_jwt.clone(), Duration::from_secs(120), |v| {
                v.get("state").and_then(Value::as_str) == Some("ready")
            })
            .await
            .context("the workspace never came back ready for the tunnel")?;
            // One session, spent twice. The second connect must be refused: a CONNECT token is
            // one-shot, and replaying one is either a bug or an attack.
            let session = super::workspace::ssh_session(c, &ws).await?;
            let (ssh, kl) = (c.programs.ssh.clone(), c.programs.kl.clone());
            let args = super::workspace::ssh_args(&kl, &key, &ws);
            let env = super::workspace::session_env(&session);
            tools::run(&ssh, &args, &env, None, Duration::from_secs(30))
                .await
                .context("the first use of a connect token failed")?;
            if tools::run(&ssh, &args, &env, None, Duration::from_secs(30)).await.is_ok() {
                return Err(anyhow!("a connect token was accepted twice"));
            }

            // The per-workspace cap. Each tunnel is its own ssh, held open by a sleep; the
            // eleventh is judged by the GATEWAY's own answer — ssh exits when the tunnel is
            // refused — waited for with a bound rather than sampled a second after spawn, which
            // read a still-handshaking child as "the cap is not enforced".
            let mut open = vec![];
            for i in 0..MAX_PER_WS {
                let session = super::workspace::ssh_session(c, &ws).await?;
                match spawn_tunnel(&ssh, &kl, &key, &ws, &session) {
                    Ok(ch) => open.push(ch),
                    Err(e) => {
                        for mut ch in open {
                            let _ = ch.kill().await;
                        }
                        return Err(anyhow!("tunnel {i} could not be opened at all: {e}"));
                    }
                }
            }
            // A moment for the ten to be counted by the gateway before the eleventh asks.
            tokio::time::sleep(Duration::from_secs(5)).await;
            let session = super::workspace::ssh_session(c, &ws).await?;
            let over = match spawn_tunnel(&ssh, &kl, &key, &ws, &session) {
                Ok(mut ch) => {
                    let out = tokio::time::timeout(OVER_CAP, ch.wait()).await;
                    let _ = ch.kill().await;
                    match out {
                        // Refused: ssh exited on its own, non-zero, inside the window.
                        Ok(Ok(status)) => !status.success(),
                        Ok(Err(_)) => false,
                        // Still pumping after the window: the eleventh tunnel is live.
                        Err(_) => false,
                    }
                }
                Err(_) => true,
            };
            for mut ch in open {
                let _ = ch.kill().await;
            }
            if !over {
                return Err(anyhow!("an eleventh tunnel to one workspace stayed open: the cap of {MAX_PER_WS} is not being enforced"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// One held-open tunnel, as a child process. `kill_on_drop` so a step that times out takes its
/// tunnels with it rather than leaving slots held until the gateway's 30-minute idle close.
fn spawn_tunnel(
    ssh: &str,
    kl: &str,
    key: &str,
    ws: &str,
    session: &str,
) -> std::io::Result<tokio::process::Child> {
    // `ssh_args` ends in the command `true`; pushing after it would run `true sleep 60`, which
    // exits at once — ten tunnels the gateway counted for 300 ms each, and an eleventh that
    // "stayed open" against a count of zero. The held tunnel replaces the command.
    let mut argv = super::workspace::ssh_args(kl, key, ws);
    argv.pop();
    argv.push("sleep 60".into());
    tokio::process::Command::new(ssh)
        .args(&argv)
        .envs(super::workspace::session_env(session))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
}

/// How long the eleventh tunnel gets to be refused. An ssh that is still connected after this has
/// been let through — the gateway refuses at dial, not lazily.
const OVER_CAP: Duration = Duration::from_secs(20);

/// `tunnel::MAX_PER_WS`. Repeated rather than imported — the gateway is a binary crate with no
/// library target, and the number is part of its contract with a person's editor.
const MAX_PER_WS: usize = 10;

// ── the console's own write ─────────────────────────────────────────────

/// `admin.workload.roll`: the button an operator has, doing what it says.
///
/// `settings.roll` proves only the 409 the precheck answers; the WRITE — a merge patch of
/// `kloudlite.io/restarted-at` on a pod template — was never asserted to restart anything. The
/// target is the agent DaemonSet, the same reader `settings.roll` uses, because it is the one
/// workload whose restart costs the fleet nothing.
async fn workload_roll(c: &mut Ctx) {
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
async fn annotation_of(k3s: &kube::Client) -> Result<String> {
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

async fn rows_of(c: &Ctx, url: &str, jwt: &str) -> Result<Vec<Value>> {
    let doc = get(c, url, jwt).await.context("could not read the workloads")?;
    Ok(doc.get("workloads").and_then(Value::as_array).or_else(|| doc.as_array()).cloned().unwrap_or_default())
}

fn agent_row(rows: &[Value]) -> Option<(i64, i64)> {
    let r = rows.iter().find(|r| r.get("name").and_then(Value::as_str) == Some("kloudlite-agent"))?;
    let n = |k: &str| r.get(k).and_then(Value::as_i64).unwrap_or(0);
    Some((n("ready"), n("desired")))
}

async fn poll_rows(
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every id this file owns is produced exactly once with nothing reachable.
    #[tokio::test]
    async fn every_id_is_produced_once_with_nothing_reachable() {
        let mut c = crate::testkit::ctx().await;
        c.kube = None;
        run(&mut c, None).await;
        let ids: Vec<&str> = c.steps.iter().map(|s| s.slo_id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "roll.zero.errors",
                "srv.drain.handover",
                "reg.moved.image",
                "reg.blob.session",
                "git.gc.packs",
                "git.limits",
                "admin.workload.roll",
                "ws.spread",
                "snap.retain",
                "agent.janitor",
                "srv.lanes",
                "gw.caps",
            ]
        );
    }
}
