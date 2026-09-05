//! Stage 5 · Workspace: one dev workspace, exercised the way a person uses one — created, exec'd
//! into, ssh'd into through the gateway, pushed and cloned.
//!
//! Everything here needs the workspace, so a create that never reaches `ready` skips the whole
//! stage with one reason rather than reporting eight failures for one broken thing.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;
use serde_json::Value;

use super::{api, poll_json, post, raw};
use crate::ctx::Ctx;
use crate::tools;

/// Per-step ceilings. Each is at least its catalogue target — a slow answer must be a breach with a
/// number, not a step the probe cut off. This stage sums to 310 s if every single step times out;
/// stages 6 and 7 add 290 s and 335 s, so the three are 935 s of pure worst case against the fast
/// suite's 900 s deadline. That is deliberate: the sum is only reachable by a fleet that is wedged
/// in every dimension at once, where the CronJob deadline is the right backstop and the run is
/// already a failure whatever it reports.
const CREATE_CEILING: Duration = Duration::from_secs(90);
const EXEC_CEILING: Duration = Duration::from_secs(20);
const TUNNEL_CEILING: Duration = Duration::from_secs(20);
const PUSH_CEILING: Duration = Duration::from_secs(60);
const CLONE_CEILING: Duration = Duration::from_secs(60);
/// The body's own cap plus `UNDO_SLACK`: `env.quota.refused` brings the probe's quota DOWN and
/// must put it back, and `Ctx::step`'s timeout drops the whole future, undo included.
const QUOTA_BODY: Duration = Duration::from_secs(15);
const QUOTA_CEILING: Duration = Duration::from_secs(20);
const ENV_QUOTA_CEILING: Duration =
    Duration::from_secs(QUOTA_BODY.as_secs() + crate::drill::UNDO_SLACK);

/// The disk a probe workspace asks for, well inside `Quota/slo-probe`'s `diskGb`.
const QUOTA_GB: u64 = 1;

/// The container in a workspace pod (`k8s::workspace_pod`). Named rather than defaulted: the pod
/// grows a second container the day anything is side-carred, and an exec with no container named
/// starts failing then for a reason that reads like the fleet being down.
pub(crate) const WS_CONTAINER: &str = "workspace";

/// Every id this stage owns after the create, in journey order.
const AFTER_CREATE: [&str; 8] = [
    "ws.exec.ok",
    "homes.rw.p95",
    "gw.tunnel.p95",
    "gw.unregistered.refused",
    "ws.push.p95",
    "ws.clone.p95",
    "quota.refused",
    "env.quota.refused",
];

/// The sentence `quota::refuse` builds, minus the dimension and the numbers: every refusal from
/// the single gate carries it, and a 409 that does not is some other conflict wearing the same
/// status. Repeated rather than imported — `kloudlite-workspaces` is a dependency, but the text is
/// the CONTRACT with a person reading it, and a test that could be satisfied by importing the
/// constant would not notice it being reworded.
const REFUSAL_TAIL: &str = "in use; request more under Quota";

pub async fn run(c: &mut Ctx) {
    let created = create(c).await;
    // The one moment a Workspace of ours certainly exists: the admission-policy probe runs here
    // (under the security stage's id), because the security stage itself comes after teardown.
    if created {
        super::security::agent_spec(c).await;
    }
    if !created {
        for id in AFTER_CREATE {
            c.skip(id, "the workspace never became ready");
        }
        return;
    }
    let Some(id) = c.state.workspace.clone() else {
        for id in AFTER_CREATE {
            c.skip(id, "the create answered no workspace id");
        }
        return;
    };
    exec_ok(c, &id).await;
    home_round_trip(c, &id).await;
    tunnel(c, &id).await;
    unregistered_refused(c, &id).await;
    push(c, &id).await;
    clone(c, &id).await;
    quota_refused(c).await;
    env_quota_refused(c, &id).await;
}

/// `ws.create.p95`: the create AND the wait for `ready`, because "creating a workspace completes"
/// is the whole of what a person waits through — a 202 on its own measures nothing they can use.
async fn create(c: &mut Ctx) -> bool {
    let name = c.prefix();
    let body = serde_json::json!({
        "name": name,
        "region": c.cfg.region,
        "quota_gb": QUOTA_GB,
        // The warm set: a package list nix has an indexed profile for, so this measures the
        // workspace and not a cold nixpkgs evaluation (`ws.cold.profile` is the weekly one).
        "packages": ["bash"],
    });
    c.step("ws.create.p95", CREATE_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, "/v1/workspaces");
        async move {
            let doc = post(c, &url, &jwt, body).await.context("could not create the workspace")?;
            let id = doc
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("the answer carried no workspace id"))?
                .to_string();
            // Recorded BEFORE the wait: a workspace that never becomes ready still exists, and
            // teardown finds it by name — but the later stages need the id whatever happened here.
            c.state.workspace = Some(id.clone());
            // The one moment a Workspace of ours certainly exists: the admission-policy probe runs here.
            // The Volume a fresh workspace gets is named after the workspace itself
            // (`stop_workspace`'s `volume_ref … unwrap_or_else(|| w.name_any())`), which is what
            // makes the volume routes addressable before the first push has published a pointer.
            c.state.volume = Some(id.clone());
            let ws = api(c, &format!("/v1/workspaces/{id}"));
            poll_json(c, &ws, &jwt, CREATE_CEILING, |v| state_is(v, "ready")).await
        }
        .boxed()
    })
    .await
}

fn state_is(v: &Value, want: &str) -> bool {
    v.get("state").and_then(Value::as_str) == Some(want)
}

/// `ws.exec.ok`: a command inside the running pod, its OUTPUT, and the pod's home.
///
/// A channel that opened proves nothing a person can use, and neither does an echo on its own: a
/// pod started before its node's NFS mount was up hostPaths an empty local directory and silently
/// strands the owner's dotfiles (`apply_workspace` parks a workspace in `HomeNotReady` to stop
/// exactly that, and this is the id that would notice if it ever stopped). So the script prints a
/// word AND the filesystem `/home/kl` is on, and the step judges both.
async fn exec_ok(c: &mut Ctx, id: &str) {
    if c.kube.is_none() {
        return c.skip("ws.exec.ok", "no kubeconfig");
    }
    let id = id.to_string();
    c.step("ws.exec.ok", EXEC_CEILING, move |c| {
        async move {
            let (code, out, err) = ws_exec(c, &id, EXEC_SCRIPT, EXEC_CEILING).await?;
            if code != 0 {
                return Err(anyhow!("exec exited {code}: {}", err.trim()));
            }
            home_is_shared(&out)
        }
        .boxed()
    })
    .await;
}

/// Echo a word, then say what `/home/kl` actually is.
///
/// `stat -f -c %T` names the filesystem type and is in both coreutils and busybox; the
/// `/proc/mounts` line is the fallback and the more precise answer, since a bind of a local
/// directory and the NFS export are different lines there whatever `stat` decides to call them.
const EXEC_SCRIPT: &str = r#"echo slo
stat -f -c %T /home/kl 2>/dev/null || true
awk '$2 == "/home/kl" { print $3 }' /proc/mounts 2>/dev/null || true"#;

/// The exec said `slo`, and its home is the shared export rather than an empty local directory.
///
/// A pure function so the one judgement this id turns on is testable without a cluster. NFS is the
/// only accepted answer: `mount_homes` mounts `{pool}/homes` over NFS 4.1 on every node, so
/// anything else under `/home/kl` — an overlay, a tmpfs, a local btrfs subvolume — is the pod
/// having come up before its node's mount and hostPathing an empty directory.
fn home_is_shared(out: &str) -> Result<()> {
    let mut lines = out.lines().map(str::trim).filter(|l| !l.is_empty());
    if lines.next() != Some("slo") {
        return Err(anyhow!("the exec printed {:?}, not its command's output", out.trim()));
    }
    let rest: Vec<&str> = lines.collect();
    if rest.is_empty() {
        return Err(anyhow!("the pod could not say what /home/kl is"));
    }
    if rest.iter().any(|l| l.contains("nfs")) {
        return Ok(());
    }
    Err(anyhow!("/home/kl is {rest:?}, not the shared NFS export"))
}

/// `homes.rw.p95`: write, `sync`, read back on the shared NFS home, timed INSIDE the pod.
///
/// The ms the pod prints is the sample, not the step's own elapsed time: the step's clock includes
/// the exec handshake with the API server, which is tens of milliseconds against a 200 ms target —
/// it would be most of the number.
async fn home_round_trip(c: &mut Ctx, id: &str) {
    if c.kube.is_none() {
        return c.skip("homes.rw.p95", "no kubeconfig");
    }
    let sample = Arc::new(AtomicU32::new(0));
    let (id, seen, want) = (id.to_string(), sample.clone(), c.run_id.clone());
    c.step("homes.rw.p95", EXEC_CEILING, move |c| {
        async move {
            let (code, out, err) = ws_exec(c, &id, &home_script(&want), EXEC_CEILING).await?;
            if code != 0 {
                return Err(anyhow!("the home round trip exited {code}: {}", err.trim()));
            }
            let ms: u32 = out
                .trim()
                .lines()
                .next_back()
                .and_then(|l| l.trim().parse().ok())
                .ok_or_else(|| anyhow!("the pod printed no duration"))?;
            seen.store(ms, Ordering::SeqCst);
            Ok(())
        }
        .boxed()
    })
    .await;
    // Only on success: a failed step's `ms` is the time it took to fail, which is the honest
    // number for a sample that is not counted as good anyway.
    if let Some(s) = c.steps.last_mut() {
        if s.ok && s.slo_id == "homes.rw.p95" {
            s.ms = sample.load(Ordering::SeqCst);
        }
    }
}

/// Write, `sync`, read back, and time it INSIDE the pod.
///
/// `set -e` and the final `[ … ]` are both load-bearing: an NFS export that answered a write and
/// then handed back somebody else's bytes would exit 0 with a duration to report, and this SLO
/// would stay green through the one failure that loses a person's work. So the read-back is
/// compared to what was written, and the comparison decides the exit code.
///
/// `/proc/uptime` rather than `date +%s%N`: BusyBox `date` has no `%N`, and the workspace image is
/// not guaranteed to be the one with GNU coreutils. Its second field is centiseconds, so the
/// resolution is 10 ms against a 200 ms target — coarse, and the honest ceiling of what every
/// image can measure.
fn home_script(want: &str) -> String {
    format!(
        r#"set -e
want={want}
up() {{ read -r a _ < /proc/uptime; echo "$(( 10#${{a%.*}}${{a#*.}} ))"; }}
s=$(up)
echo "$want" > /home/kl/.slo
sync /home/kl/.slo
got=$(cat /home/kl/.slo)
e=$(up)
[ "$got" = "$want" ]
echo $(( (e - s) * 10 ))"#
    )
}

pub(crate) async fn ws_exec(c: &Ctx, id: &str, script: &str, cap: Duration) -> Result<(i32, String, String)> {
    let probe = c.probe_user.clone();
    let k = c.kube.as_ref().ok_or_else(|| anyhow!("no kubeconfig"))?;
    // The probe's workspaces are personal, never a team's, so the namespace is `ws-slo-probe`.
    let ns = kloudlite_workspaces::crd::ws_namespace(&probe, "");
    // As `kl`, the user sshd hands a person: the container's own user is root, and root's git
    // refuses a repository `kl` owns ("dubious ownership"), which is exactly the vantage point a
    // user never has. What the probe measures is what the person sees.
    //
    // `su … -c`, NOT a login shell: the environment the script reads is the one the container spec
    // sets (`k8s::login_env`), not a profile's. That is what `cache_is_local` is reading back, and
    // calling it "the login shell" was wrong.
    let user = kloudlite_workspaces::k8s::SSH_USER;
    crate::kube::exec(k, &ns, id, Some(WS_CONTAINER), &["su", user, "-s", "/bin/sh", "-c", script], cap).await
}

/// `gw.tunnel.p95`: the whole `kl ssh` path — mint a session, then let `ssh` reach the pod through
/// `kl ws proxy`, which is the websocket tunnel to the region's gateway.
async fn tunnel(c: &mut Ctx, id: &str) {
    let key = c.cfg.ssh_key_path.clone();
    let id = id.to_string();
    c.step("gw.tunnel.p95", TUNNEL_CEILING, move |c| {
        async move {
            let session = ssh_session(c, &id).await?;
            let (ssh, kl) = (c.programs.ssh.clone(), c.programs.kl.clone());
            tools::run(&ssh, &ssh_args(&kl, &key, &id), &session_env(&session), None, TUNNEL_CEILING)
                .await
                .map(|_| ())
        }
        .boxed()
    })
    .await;
}

/// `gw.unregistered.refused`: the same tunnel with a key the fleet has never seen. Only a REFUSAL
/// passes — a tunnel that failed to open at all is the outage this exists to catch, not a pass.
async fn unregistered_refused(c: &mut Ctx, id: &str) {
    let junk = c.tmp.join("gw-unregistered");
    let _ = std::fs::remove_file(&junk);
    let _ = std::fs::remove_file(junk.with_extension("pub"));
    if let Err(e) = tools::plain(
        &c.programs.ssh_keygen,
        &["-q", "-t", "ed25519", "-N", "", "-C", "unregistered", "-f", &junk.display().to_string()],
        Duration::from_secs(20),
    )
    .await
    {
        return c.skip("gw.unregistered.refused", &format!("no throwaway key: {e:#}"));
    }
    let id = id.to_string();
    c.step("gw.unregistered.refused", TUNNEL_CEILING, move |c| {
        async move {
            let session = ssh_session(c, &id).await?;
            let (ssh, kl) = (c.programs.ssh.clone(), c.programs.kl.clone());
            let args = ssh_args(&kl, &junk.display().to_string(), &id);
            match tools::run(&ssh, &args, &session_env(&session), None, TUNNEL_CEILING).await {
                Ok(_) => Err(anyhow!("an unregistered key was let into the workspace")),
                Err(e) => {
                    let detail = format!("{e:#}");
                    if detail.contains("Permission denied") {
                        Ok(())
                    } else {
                        Err(anyhow!("ssh failed for some other reason than a refusal: {detail}"))
                    }
                }
            }
        }
        .boxed()
    })
    .await;
}

/// The connect ticket `kl ws ssh` mints, as the JSON `kl ws proxy` reads from its environment.
async fn ssh_session(c: &Ctx, id: &str) -> Result<String> {
    let url = api(c, &format!("/v1/workspaces/{id}/ssh-session"));
    let doc = post(c, &url, &c.probe_jwt.clone(), Value::Null)
        .await
        .context("could not mint an ssh session")?;
    Ok(doc.to_string())
}

/// What `kl ws proxy` expects to be handed: the whole `Session` document, so the child makes no api
/// call and needs no `kl login` state in the pod.
fn session_env(session: &str) -> std::collections::HashMap<String, String> {
    std::collections::HashMap::from([(SESSION_ENV.to_string(), session.to_string())])
}

/// `kl`'s own `proxy::SESSION_ENV` (`bins/kl/src/proxy.rs`). Repeated rather than imported: `kl` is
/// a binary crate with no library target, so there is nothing to depend on — and the name is part
/// of the CLI's contract with ssh, which is exactly the kind of thing this probe exists to catch.
const SESSION_ENV: &str = "KL_SSH_SESSION";

/// ssh's argv for a workspace, through `kl ws proxy`.
///
/// `StrictHostKeyChecking=no` is CORRECT here, unlike everywhere else in this probe: a workspace
/// pod's host key is generated per workspace when the pod is created, so there is nothing to pin —
/// the ssh-session answer is its only source, and pinning what the platform just told us would
/// check nothing. Host identity for the git listener, which does have a stable key, is
/// `ssh.hostkey`'s job and is pinned there.
pub(crate) fn ssh_args(kl: &str, key: &str, id: &str) -> Vec<String> {
    [
        "-i",
        key,
        "-o",
        "IdentitiesOnly=yes",
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
        "-o",
        "BatchMode=yes",
        "-o",
        &format!("ProxyCommand={kl} ws proxy {id}"),
        &format!("kl@{id}"),
        "true",
    ]
    .iter()
    .map(|a| a.to_string())
    .collect()
}

/// `ws.push.p95`: the push, and the wait for the snapshot to turn `ready` — a `Working` cut is not
/// a push somebody can restore from.
async fn push(c: &mut Ctx, id: &str) {
    let Some(volume) = c.state.volume.clone() else {
        return c.skip("ws.push.p95", "no volume");
    };
    let id = id.to_string();
    c.step("ws.push.p95", PUSH_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/workspaces/{id}/push"));
        let history = api(c, &format!("/v1/volumes/{volume}/history"));
        async move {
            let doc = post(c, &url, &jwt, serde_json::json!({})).await.context("could not push")?;
            let snap = doc
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("the push answered no snapshot id"))?
                .to_string();
            c.state.snapshot = Some(snap.clone());
            poll_json(c, &history, &jwt, PUSH_CEILING, |v| row_ready(v, &snap))
                .await
                .context("the snapshot never turned ready")
        }
        .boxed()
    })
    .await;
}

/// Whether `/v1/volumes/{name}/history` carries `snap` as a `ready` row.
pub(crate) fn row_ready(v: &Value, snap: &str) -> bool {
    v.as_array().is_some_and(|rows| {
        rows.iter().any(|r| {
            r.get("id").and_then(Value::as_str) == Some(snap)
                && r.get("phase").and_then(Value::as_str) == Some("ready")
        })
    })
}

/// `ws.clone.p95`: the local-copy verb, named under this run's prefix so teardown's sweep finds it
/// — the API never names a clone itself, the caller does.
async fn clone(c: &mut Ctx, id: &str) {
    let name = format!("{}-clone", c.prefix());
    let id = id.to_string();
    c.step("ws.clone.p95", CLONE_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/workspaces/{id}/clone"));
        let body = serde_json::json!({ "name": name });
        async move {
            let doc = post(c, &url, &jwt, body).await.context("could not clone")?;
            let new = doc
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("the clone answered no workspace id"))?
                .to_string();
            c.state.clone = Some(new.clone());
            let ws = api(c, &format!("/v1/workspaces/{new}"));
            poll_json(c, &ws, &jwt, CLONE_CEILING, |v| state_is(v, "ready")).await
        }
        .boxed()
    })
    .await;
}

/// `quota.refused`: an over-quota create answers 409 and nothing is allocated.
///
/// Over the DISK dimension rather than the workspace count, deliberately: the journey itself needs
/// several workspaces (the clone, then two restores in stage 7), so a probe that leaned on the
/// count would either exhaust the quota its own later steps need or stop refusing the day the
/// deployment raised it. Asking for more disk than `Quota/slo-probe` allows is refused whatever
/// else the run holds, and `guard_alloc` is the same single gate either way.
async fn quota_refused(c: &mut Ctx) {
    let name = format!("{}-overquota", c.prefix());
    c.step("quota.refused", QUOTA_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, "/v1/workspaces");
        let body = serde_json::json!({
            "name": name,
            "region": c.cfg.region,
            "quota_gb": u32::MAX,
            "packages": [],
        });
        async move { refused_over(c, reqwest::Method::POST, &url, &jwt, Some(body), "diskGb", "a create").await }
            .boxed()
    })
    .await;
}

/// One request that must be refused by `guard_alloc`, with the SENTENCE checked.
///
/// The status alone is not the SLI: `quota::refuse` answers `"{dimension}: {used} of {limit} in
/// use; request more under Quota"`, and a 409 naming the wrong dimension — or a 409 from some
/// other conflict entirely, which is what every other refusal on these routes is — would pass a
/// check that only read the number. `dim` is the dimension the caller deliberately overshot.
async fn refused_over(
    c: &Ctx,
    method: reqwest::Method,
    url: &str,
    jwt: &str,
    body: Option<Value>,
    dim: &str,
    what: &str,
) -> Result<()> {
    let (status, text) = raw(c, method, url, jwt, body, &[]).await?;
    let clipped: String = text.chars().take(200).collect();
    if status != reqwest::StatusCode::CONFLICT {
        return Err(anyhow!("an over-quota {what} answered {status}: {clipped}"));
    }
    if !text.starts_with(&format!("{dim}: ")) || !text.contains(REFUSAL_TAIL) {
        return Err(anyhow!("the refusal of {what} does not name {dim} and its usage: {clipped}"));
    }
    Ok(())
}

/// `env.quota.refused`: the OTHER three verbs behind the one gate.
///
/// `quota.refused` above probes a create; restore, clone and push all route through the same
/// `guard_alloc`, and a gate wired into one of four call sites is a gate that hands out
/// allocation nobody decided on through the other three. All three in one step, first failure
/// wins — each alone says nothing about the others.
///
/// The dimensions differ on purpose and are what each verb actually overshoots: a restore names
/// its own disk, while a clone and a push cannot ask for a size at all, so they are refused on the
/// count dimension the run is already at. The clone and the push are aimed at THIS run's own
/// workspace, so nothing new is allocated even on the path where the gate fails open.
async fn env_quota_refused(c: &mut Ctx, ws: &str) {
    let ws = ws.to_string();
    c.step("env.quota.refused", ENV_QUOTA_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let restore = api(c, "/v1/workspaces/restore");
        let clone = api(c, &format!("/v1/workspaces/{ws}/clone"));
        let push = api(c, &format!("/v1/workspaces/{ws}/push"));
        let snapshot = c.state.snapshot.clone();
        let name = format!("{}-overq", c.prefix());
        async move {
            let Some(snapshot) = snapshot else {
                return Err(anyhow!("the workspace was never pushed, so there is nothing to restore"));
            };
            // A restore carries a size, so it is refused the same way a create is.
            let body = serde_json::json!({ "name": name, "snapshot_id": snapshot, "quota_gb": u32::MAX });
            refused_over(c, reqwest::Method::POST, &restore, &jwt, Some(body), "diskGb", "a restore").await?;
            // A clone and a push carry no size at all, so neither can be made to overshoot by
            // asking for more — the only way to stand them against the gate is to bring the LIMIT
            // down to what the run already holds. Not the second tenant's zero quota: `may_act_on`
            // refuses another owner's workspace long before `guard_alloc` is reached, so that
            // would measure ownership and call it quota.
            let admin_jwt = c.admin_jwt.clone();
            let write = super::admin(c, &format!("/admin/quota/{}", c.probe_user));
            let body = serde_json::json!({ "spec": pinched(c, &jwt).await?, "note": "slo probe quota.refused" });
            super::call(c, reqwest::Method::PUT, &write, &admin_jwt, Some(body))
                .await
                .context("could not bring the quota down to what the run holds")?;
            // Outside the cancellable region, exactly as `request.approve`'s raise is: a probe
            // that left the quota PINCHED would refuse its own next run's every create.
            let restore_quota = || async {
                let back = serde_json::json!({
                    "spec": super::experience_admin::probe_quota(),
                    "note": "slo probe quota restore",
                });
                super::call(c, reqwest::Method::PUT, &write, &admin_jwt, Some(back))
                    .await
                    .map(|_| ())
                    .context("the probe's quota was left PINCHED")
            };
            let both = async {
                refused_over(c, reqwest::Method::POST, &clone, &jwt, Some(serde_json::json!({ "name": name })), "workspaces", "a clone").await?;
                refused_over(c, reqwest::Method::POST, &push, &jwt, Some(serde_json::json!({})), "snapshots", "a push").await
            };
            crate::drill::undoing(QUOTA_BODY, both, restore_quota).await
        }
        .boxed()
    })
    .await;
}

/// The yaml's quota with the two COUNT dimensions brought down to what this run already holds, so
/// the next clone and the next push are each one over. Everything else is left as the yaml has it
/// — a probe that pinched disk as well would refuse things the rest of the stage still needs.
async fn pinched(c: &Ctx, jwt: &str) -> Result<Value> {
    let seen = super::get(c, &api(c, "/v1/quota"), jwt).await.context("could not read the quota")?;
    let used = |dim: &str| seen.pointer(&format!("/used/{dim}")).and_then(Value::as_u64);
    let (ws, snaps) = (used("workspaces"), used("snapshots"));
    let (Some(ws), Some(snaps)) = (ws, snaps) else {
        return Err(anyhow!("the quota answer carries no live counts to pinch against"));
    };
    let mut spec = super::experience_admin::probe_quota();
    let o = spec.as_object_mut().ok_or_else(|| anyhow!("the quota spec is not an object"))?;
    o.insert("workspaces".into(), ws.into());
    o.insert("snapshots".into(), snaps.into());
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;

    /// The ProxyCommand IS the gateway path: without it ssh would dial the workspace directly,
    /// which nothing routes, and the step would measure a DNS failure. The session goes down to the
    /// proxy child through the environment, exactly as `kl ws ssh` hands it over.
    #[test]
    fn gateway_step_uses_kl_proxy() {
        let args = ssh_args("kl", "/etc/slo-ssh/id_ed25519", "ws-abc");
        assert!(
            args.iter().any(|a| a == "ProxyCommand=kl ws proxy ws-abc"),
            "{args:?}"
        );
        assert!(args.iter().any(|a| a == "kl@ws-abc"), "{args:?}");
        // Nothing to pin: the pod's host key is minted with the pod.
        assert!(args.iter().any(|a| a == "StrictHostKeyChecking=no"), "{args:?}");
        let env = session_env(r#"{"id":"ws-abc"}"#);
        assert_eq!(env.get("KL_SSH_SESSION").map(String::as_str), Some(r#"{"id":"ws-abc"}"#));
    }

    /// The one judgement `ws.exec.ok` turns on. A pod that came up before its node's NFS mount
    /// hostPaths an empty local directory and loses the owner's dotfiles silently — an id that
    /// only checked the echo would pass straight through it.
    #[test]
    fn the_exec_check_wants_the_output_and_a_shared_home() {
        assert!(home_is_shared("slo\nnfs\nnfs4\n").is_ok());
        assert!(home_is_shared("slo\nnfs4\n").is_ok());
        // The failure this exists for: a local filesystem under /home/kl.
        assert!(home_is_shared("slo\nbtrfs\n").is_err());
        assert!(home_is_shared("slo\noverlay\n").is_err());
        // And the ordinary weak check: an exec that connected and said nothing useful.
        assert!(home_is_shared("").is_err());
        assert!(home_is_shared("slo\n").is_err());
        assert!(home_is_shared("nfs\n").is_err());
        // The script asks the two questions in that order.
        assert!(EXEC_SCRIPT.starts_with("echo slo"), "{EXEC_SCRIPT}");
        assert!(EXEC_SCRIPT.contains("/proc/mounts"), "{EXEC_SCRIPT}");
    }

    /// The read-back comparison is the whole point: an export that took the write and handed back
    /// somebody else's bytes must fail the step, not report a fast round trip.
    #[test]
    fn the_home_script_fails_on_a_bad_read_back_and_times_with_proc_uptime() {
        let script = home_script("fast-42");
        assert!(script.starts_with("set -e"), "{script}");
        assert!(script.contains(r#"[ "$got" = "$want" ]"#), "{script}");
        assert!(script.contains("/proc/uptime"), "{script}");
        // BusyBox `date` has no `%N`, so a nanosecond clock would fail on some images.
        assert!(!script.contains("%N"), "{script}");
    }

    /// No kubeconfig is a deployment gap, not an SLO breach: the two ids that need one skip with a
    /// reason, and every other id in the stage is still produced.
    #[tokio::test]
    async fn the_exec_ids_skip_without_a_kubeconfig() {
        let app = axum::Router::new().route(
            "/v1/workspaces",
            axum::routing::post(|| async {
                (axum::http::StatusCode::ACCEPTED, axum::Json(serde_json::json!({"id": "ws-1", "state": "ready"})))
            }),
        )
        .route("/v1/workspaces/{id}", axum::routing::get(|| async {
            axum::Json(serde_json::json!({"id": "ws-1", "state": "ready"}))
        }));
        let mut c = testkit::ctx_against(app).await;
        c.kube = None;
        run(&mut c).await;
        for id in ["ws.exec.ok", "homes.rw.p95"] {
            let s = c.steps.iter().find(|s| s.slo_id == id).unwrap_or_else(|| panic!("{id}"));
            assert!(s.skipped && s.detail == "no kubeconfig", "{s:?}");
        }
        // Every id, exactly once, whatever path the stage took.
        for id in AFTER_CREATE.iter().chain(["ws.create.p95"].iter()) {
            assert_eq!(c.steps.iter().filter(|s| s.slo_id == *id).count(), 1, "{id}");
        }
    }
}
