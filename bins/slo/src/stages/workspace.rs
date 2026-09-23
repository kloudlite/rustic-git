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

use kloudlite_workspaces::slo::catalogue::Suite;

use super::{api, get, poll_json, post, raw};
use crate::ctx::Ctx;
use crate::tools;
use super::{state_is};

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
/// The body's own cap plus `UNDO_SLACK`: both quota ids bring the probe's quota DOWN and must put
/// it back, and `Ctx::step`'s timeout drops the whole future, undo included.
const QUOTA_BODY: Duration = Duration::from_secs(15);
const QUOTA_CEILING: Duration = Duration::from_secs(QUOTA_BODY.as_secs() + crate::drill::UNDO_SLACK);
/// `env.quota.refused` pinches TWICE — disk for the restore, then the two counts for the clone and
/// the push — because `guard_alloc` checks the disk limit BEFORE any count dimension, so one pinch
/// carrying both would refuse the clone on `diskGb` and prove nothing about the counts.
const ENV_QUOTA_CEILING: Duration = Duration::from_secs(2 * QUOTA_CEILING.as_secs());
/// The catalogue's own 180 s for `ws.build.p95`: a real `docker buildx build --push`
/// dispatched through the gate to a builder that has to start cold.
const BUILD_CEILING: Duration = Duration::from_secs(180);
/// The catalogue's 30 s for `ws.build.promote`: a registry-side copy, no build.
const PROMOTE_CEILING: Duration = Duration::from_secs(30);
/// The catalogue's own targets for the two `kl` ids. An add is a PATCH the api validates and locks
/// (a version lookup that may miss its cache), where a switch is one PUT against the cluster.
const KL_PKG_CEILING: Duration = Duration::from_secs(20);
const KL_ENV_CEILING: Duration = Duration::from_secs(10);

/// The two ids `kl`'s own verbs own, `const` because each is skipped from more than one path.
const KL_PKG_ID: &str = "ws.kl.pkg.add";
pub(super) const KL_ENV_ID: &str = "ws.kl.env.switch";

/// Small, in every nixpkgs, and nothing else in the journey declares it — so a run that finds it
/// already declared is reading its own leftovers, which the teardown's workspace delete rules out.
const KL_PACKAGE: &str = "cowsay";

/// The disk a probe workspace asks for, well inside `Quota/slo-probe`'s `diskGb`.
pub(crate) const QUOTA_GB: u64 = 1;

/// The container in a workspace pod (`k8s::workspace_pod`). Named rather than defaulted: the pod
/// grows a second container the day anything is side-carred, and an exec with no container named
/// starts failing then for a reason that reads like the fleet being down.
pub(crate) const WS_CONTAINER: &str = "workspace";

/// Every id this stage owns after the create, in journey order.
const AFTER_CREATE: [&str; 10] = [
    "ws.exec.ok",
    "homes.rw.p95",
    "gw.tunnel.p95",
    "key.projected",
    "key.live",
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
    // The bench is independent of the workspace, so it runs whether or not the create lands —
    // and in its own hourly group (`suite::group_of`), which walks nothing else here.
    if c.walks("bench.create") {
        super::bench::fast(c).await;
    }
    if !c.walks("ws.create.p95") {
        return;
    }
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
    key_projected(c).await;
    key_live(c, &id).await;
    unregistered_refused(c, &id).await;
    push(c, &id).await;
    clone(c, &id).await;
    quota_refused(c, &id).await;
    env_quota_refused(c, &id).await;
    build_push(c, &id).await;
    promote(c, &id).await;
    kl_pkg_add(c, &id).await;
    // Last: it leaves 200 MB in the workspace, which teardown collects with the workspace itself,
    // and it must not move the disk usage the two quota ids above pinch against.
    super::quota_usage::stamped(c, &id).await;
}

/// `ws.kl.pkg.add`, hourly only: a person installs a package from inside their own workspace.
///
/// The exec is the same non-login `su -c` the build step uses — `kl` carries its own credential
/// (`/etc/kloudlite/ssh/workspace-token`) and reads its identity from the pod env, so it must work
/// from an editor's terminal as well as from a login shell.
///
/// What is judged is the API's own state, not `kl`'s output: a command that printed `added` and
/// PATCHed nothing would pass an assertion on stdout alone. The run's teardown deletes the
/// workspace, so there is nothing to undo.
async fn kl_pkg_add(c: &mut Ctx, id: &str) {
    if c.suite != Suite::Hourly {
        return;
    }
    if c.kube.is_none() {
        return c.skip(KL_PKG_ID, "no kubeconfig");
    }
    let id = id.to_string();
    c.step(KL_PKG_ID, KL_PKG_CEILING, move |c| {
        let id = id.clone();
        async move {
            let (code, out, err) =
                ws_exec(c, &id, &format!("kl pkg add {KL_PACKAGE}"), KL_PKG_CEILING).await?;
            if code != 0 {
                return Err(anyhow!("`kl pkg add` exited {code}: {} {}", out.trim(), err.trim()));
            }
            let doc = get(c, &api(c, &format!("/v1/workspaces/{id}")), &c.probe_jwt.clone()).await?;
            if !declares(&doc, KL_PACKAGE) {
                return Err(anyhow!(
                    "the workspace does not declare {KL_PACKAGE} after `kl pkg add`, which printed {:?}",
                    out.trim()
                ));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `packages` names `attr`, at whatever version it is pinned to — `kl pkg add cowsay` may be
/// stored as `cowsay` or, once the api has locked it, still as the bare entry with a lock beside
/// it, so the comparison is on the attribute half.
fn declares(doc: &Value, attr: &str) -> bool {
    doc.get("packages")
        .and_then(Value::as_array)
        .is_some_and(|a| a.iter().filter_map(Value::as_str).any(|e| e.split('@').next() == Some(attr)))
}

/// `ws.kl.env.switch`, hourly only: `kl env switch` from inside the workspace moves the person's
/// space, and `kl env clear` puts it back.
///
/// Called from the ENVIRONMENT stage, not from this one — it is the first moment both the run's
/// workspace and its environment exist — but catalogued under `5 · Workspace` with its sibling,
/// because what it probes is the CLI in the pod, not the environment.
///
/// The check is `GET /v1/me/environments` as the probe user: `kl` printing `switched to …` says
/// only that it reached the api, where the space row is what every pod of the space converges
/// from. The clear is outside the assertion's failure path on purpose — a run that left the space
/// chosen would hand stage 7 an environment nobody asked it to hold.
pub(super) async fn kl_env_switch(c: &mut Ctx, env: &str) {
    if c.suite != Suite::Hourly {
        return;
    }
    let Some(ws) = c.state.workspace.clone() else {
        return c.skip(KL_ENV_ID, "no workspace");
    };
    if c.kube.is_none() {
        return c.skip(KL_ENV_ID, "no kubeconfig");
    }
    let (env, probe) = (env.to_string(), c.probe_user.clone());
    c.step(KL_ENV_ID, KL_ENV_CEILING, move |c| {
        let (ws, env, probe) = (ws.clone(), env.clone(), probe.clone());
        async move {
            let (code, out, err) =
                ws_exec(c, &ws, &format!("kl env switch {env}"), KL_ENV_CEILING).await?;
            if code != 0 {
                return Err(anyhow!("`kl env switch` exited {code}: {} {}", out.trim(), err.trim()));
            }
            let me = get(c, &api(c, "/v1/me/environments"), &c.probe_jwt.clone()).await?;
            let chosen = follows(&me, &probe) == Some(env.clone());
            // Cleared whatever the check decided: the space is shared with the rest of the run.
            let (ccode, _, cerr) = ws_exec(c, &ws, "kl env clear", KL_ENV_CEILING).await?;
            if !chosen {
                return Err(anyhow!("the probe's own space does not follow {env} after `kl env switch`"));
            }
            if ccode != 0 {
                return Err(anyhow!("`kl env clear` exited {ccode}: {}", cerr.trim()));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// The environment `team`'s space follows, from `GET /v1/me/environments` — a row per space the
/// caller has, so the personal one is the row whose `team` is their handle.
fn follows(me: &Value, team: &str) -> Option<String> {
    me.as_array()?
        .iter()
        .find(|s| s.get("team").and_then(Value::as_str) == Some(team))?
        .get("environment")?
        .as_str()
        .map(str::to_string)
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

/// `ws.exec.ok`: a command inside the running pod, its OUTPUT, and the pod's home.
///
/// A channel that opened proves nothing a person can use, and neither does an echo on its own: the
/// script prints a word AND the filesystem `/home/kl` is on, and the step judges both.
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
            home_is_volume(&out)
        }
        .boxed()
    })
    .await;
}

/// Echo a word, then say what `/home/kl` actually is.
///
/// `stat -f -c %T` names the filesystem type and is in both coreutils and busybox; the
/// `/proc/mounts` line is the fallback and the more precise answer, since a bind of a local
/// directory and the volume are different lines there whatever `stat` decides to call them.
const EXEC_SCRIPT: &str = r#"echo slo
stat -f -c %T /home/kl 2>/dev/null || true
awk '$2 == "/home/kl" { print $3 }' /proc/mounts 2>/dev/null || true"#;

/// The exec said `slo`, and `/home/kl` is the workspace's own btrfs volume.
///
/// A pure function so the one judgement this id turns on is testable without a cluster.
///
/// The home IS the worktree subvolume (ruling 2026-09-22), hostPathed whole at `/home/kl`. `runc`
/// sees it as `btrfs`; gVisor (`runtimeClass`) passes it through its gofer, where the same mount
/// reads as `v9fs`/`9p`. What this refuses is every other shape: the container's own overlay or
/// tmpfs (the volume never mounted, so the person's files land in a layer that dies with the pod)
/// and `nfs` (the retired shared home, back from a stale pod spec).
fn home_is_volume(out: &str) -> Result<()> {
    let mut lines = out.lines().map(str::trim).filter(|l| !l.is_empty());
    if lines.next() != Some("slo") {
        return Err(anyhow!("the exec printed {:?}, not its command's output", out.trim()));
    }
    let rest: Vec<&str> = lines.collect();
    if rest.is_empty() {
        return Err(anyhow!("the pod could not say what /home/kl is"));
    }
    const VOLUME: [&str; 3] = ["btrfs", "v9fs", "9p"];
    if rest.iter().all(|l| VOLUME.contains(l)) {
        return Ok(());
    }
    Err(anyhow!("/home/kl is {rest:?}, not the workspace's btrfs volume"))
}

/// `homes.rw.p95`: write, `sync`, read back on the home volume, timed INSIDE the pod.
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
/// `set -e` and the final `[ … ]` are both load-bearing: a home that answered a write and
/// then handed back somebody else's bytes would exit 0 with a duration to report, and this SLO
/// would stay green through the one failure that loses a person's work. So the read-back is
/// compared to what was written, and the comparison decides the exit code.
///
/// `/proc/uptime` rather than `date +%s%N`: BusyBox `date` has no `%N`, and the workspace image is
/// not guaranteed to be the one with GNU coreutils. Its second field is centiseconds, so the
/// resolution is 10 ms against a 200 ms target — coarse, and the honest ceiling of what every
/// image can measure.
///
/// POSIX, not bash: `/bin/sh` is dash on the debian workspace image, and `10#` — bash's base
/// prefix, which this used to strip the centiseconds' leading zero — is a syntax error there
/// (`arithmetic expression: expecting EOF: " 10#123 "`, hourly 2026-09-17, every glibc run). The
/// seconds and the centiseconds are combined arithmetically instead, and the ONE leading zero a
/// two-digit field can carry is stripped by parameter expansion — otherwise `$(( ))` reads `05`
/// as octal.
fn home_script(want: &str) -> String {
    format!(
        r#"set -e
want={want}
up() {{ read -r a _ < /proc/uptime; s=${{a%.*}}; c=${{a#*.}}; c=${{c#0}}; [ -n "$c" ] || c=0; echo "$(( s * 100 + c ))"; }}
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
/// `kl-connect ws proxy`, which is the websocket tunnel to the region's gateway.
async fn tunnel(c: &mut Ctx, id: &str) {
    let key = c.cfg.ssh_key_path.clone();
    let id = id.to_string();
    c.step("gw.tunnel.p95", TUNNEL_CEILING, move |c| {
        async move {
            let (ssh, kl) = (c.programs.ssh.clone(), c.programs.kl.clone());
            // Retried, like the seed key is: the workspace's `authorized_keys` Secret reaches the
            // pod through the kubelet, which propagates it on its own beat — a first live drill run
            // met `Permission denied (publickey)` on a pod that was Ready and had not read it yet.
            // A refusal that never clears inside the window is still the failure; one that clears
            // in three seconds was never one.
            let start = std::time::Instant::now();
            loop {
                let session = ssh_session(c, &id).await?;
                let out = tools::run(&ssh, &ssh_args(&kl, &key, &id), &session_env(&session), None, TUNNEL_CEILING).await;
                match out {
                    Ok(_) => return Ok(()),
                    Err(e) if start.elapsed() < KEY_PROPAGATION => {
                        tracing::info!(error = %format!("{e:#}"), "slo.tunnel.retrying");
                        tokio::time::sleep(Duration::from_secs(3)).await;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        .boxed()
    })
    .await;
}

/// How long a workspace's `authorized_keys` Secret gets to reach its pod. The kubelet's own
/// propagation beat, not a guess about the gateway: everything else in this step is one request.
const KEY_PROPAGATION: Duration = Duration::from_secs(30);

/// How long the `OwnerKeys` projection gets to carry a key the directory already has, and the
/// ceiling on the step that waits for it.
const PROJECTION_BODY: Duration = Duration::from_secs(45);
const PROJECTION_CEILING: Duration = Duration::from_secs(60);

/// `KEYS_RESYNC_SECS` (300 s) plus a beat: a pod learns of a removal only when the api rewrites
/// the projection, and the catalogue bounds `key.revoked` at exactly this.
const REVOCATION_BODY: Duration = Duration::from_secs(330);
/// The re-registration runs after the body's own cap, and `Ctx::step`'s timeout drops the whole
/// future — a probe left with no key would take every later run's tunnel down with it.
const REVOCATION_CEILING: Duration =
    Duration::from_secs(REVOCATION_BODY.as_secs() + crate::drill::UNDO_SLACK);

/// The probe's public key, derived from the private half it mounts — never a `.pub` beside it,
/// which is a second file that can disagree with the first.
async fn probe_public(c: &Ctx) -> Result<String> {
    let out = tools::plain(&c.programs.ssh_keygen, &["-y", "-f", &c.cfg.ssh_key_path], Duration::from_secs(10))
        .await
        .context("could not read the probe's public key")?;
    Ok(out.trim().to_string())
}

/// The base64 field of an OpenSSH public line: what a projected `authorized_keys` is searched for,
/// because the type prefix is shared by every ed25519 key and the trailing comment is not carried.
fn key_material(public: &str) -> Result<&str> {
    public.split_whitespace().nth(1).ok_or_else(|| anyhow!("the probe's public key has no key field"))
}

/// `key.projected`: the key `id.key.usable` registered is in the owner's `OwnerKeys` AND the node
/// has acked that generation. The api writing the object is only half the path — the file a pod
/// mounts is written by the agent, and a projection nobody converged is a key nobody can use.
async fn key_projected(c: &mut Ctx) {
    let Some(k) = c.kube.clone() else {
        return c.skip("key.projected", "no kubeconfig");
    };
    let public = match probe_public(c).await {
        Ok(p) => p,
        Err(e) => return c.skip("key.projected", &format!("{e:#}")),
    };
    let material = match key_material(&public) {
        Ok(m) => m.to_string(),
        Err(e) => return c.skip("key.projected", &format!("{e:#}")),
    };
    // The probe's workspaces are personal, so the owner namespace `OwnerKeys` is named for is the
    // probe's own handle.
    let owner = c.probe_user.clone();
    c.step("key.projected", PROJECTION_CEILING, move |_| {
        async move {
            let api: kube::Api<kloudlite_workspaces::crd::OwnerKeys> = kube::Api::all(k);
            let start = std::time::Instant::now();
            loop {
                if let Some(o) = api.get_opt(&owner).await.context("could not read the OwnerKeys projection")? {
                    let synced = o.status.as_ref().is_some_and(|s| {
                        s.observed_generation == Some(o.spec.generation)
                            && s.conditions.iter().any(|c| {
                                c.type_ == kloudlite_workspaces::crd::KEYS_SYNCED && c.status == "True"
                            })
                    });
                    if synced && o.spec.authorized_keys.contains(&material) {
                        return Ok(());
                    }
                }
                if start.elapsed() >= PROJECTION_BODY {
                    return Err(anyhow!("OwnerKeys/{owner} did not carry the probe's key as Synced"));
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
        .boxed()
    })
    .await;
}

/// `key.live`: the registered key opens the workspace. `gw.tunnel.p95` measures the same path but
/// RETRIES through `KEY_PROPAGATION`, so it stays green for a projection that takes half a minute;
/// this one call, with no retry, is what says the key was already live.
async fn key_live(c: &mut Ctx, id: &str) {
    if c.kube.is_none() {
        return c.skip("key.live", "no kubeconfig");
    }
    let (key, id) = (c.cfg.ssh_key_path.clone(), id.to_string());
    c.step("key.live", TUNNEL_CEILING, move |c| {
        async move {
            let session = ssh_session(c, &id).await?;
            let (ssh, kl) = (c.programs.ssh.clone(), c.programs.kl.clone());
            tools::run(&ssh, &ssh_args(&kl, &key, &id), &session_env(&session), None, TUNNEL_CEILING)
                .await
                .map(|_| ())
                .context("a registered key was refused by the workspace")
        }
        .boxed()
    })
    .await;
}

/// `key.revoked`: removing the key locks BOTH listeners out — git immediately, because it
/// authenticates from the directory on every request, and the workspace within a resync beat.
///
/// The re-registration is the compensation, not an afterthought: the probe mounts one key, and a
/// run that removed it and stopped there would leave every later run with no way in.
///
/// Walked by the HOURLY suite's Experience stage against its own workspace, not by stage 5: the
/// wait for a resync beat is 330 s of the fast suite's 900 s deadline for a single refusal. It
/// still lives here because every other user of `ssh_session`/`ssh_args` does.
pub(crate) async fn key_revoked(c: &mut Ctx, id: &str) {
    if c.kube.is_none() {
        return c.skip("key.revoked", "no kubeconfig");
    }
    let Some(repo) = c.state.repo.clone() else {
        return c.skip("key.revoked", "no repo to check the git listener with");
    };
    let Some(name) = c.state.key.clone() else {
        return c.skip("key.revoked", "the probe's key was never registered");
    };
    let hosts = match super::git::known_hosts(c).await {
        Ok(p) => p,
        Err(e) => return c.skip("key.revoked", &format!("{e:#}")),
    };
    let public = match probe_public(c).await {
        Ok(p) => p,
        Err(e) => return c.skip("key.revoked", &format!("{e:#}")),
    };
    let fp = match tools::plain(&c.programs.ssh_keygen, &["-lf", &c.cfg.ssh_key_path], Duration::from_secs(10)).await {
        Ok(out) => match out.split_whitespace().nth(1) {
            Some(f) => f.to_string(),
            None => return c.skip("key.revoked", "ssh-keygen printed no fingerprint"),
        },
        Err(e) => return c.skip("key.revoked", &format!("could not fingerprint the probe's key: {e:#}")),
    };
    let (key_path, id) = (c.cfg.ssh_key_path.clone(), id.to_string());
    c.step("key.revoked", REVOCATION_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let del = api(c, &format!("/v1/keys/{}", super::experience_gaps::path_seg(&fp)));
        let keys = api(c, "/v1/keys");
        let url = super::git::ssh_url(c, &repo);
        let mut env = super::git::git_env(c);
        env.insert("GIT_SSH_COMMAND".into(), super::git::ssh_command(c, &key_path, &hosts));
        let git_bin = c.programs.git.clone();
        async move {
            super::call(c, reqwest::Method::DELETE, &del, &jwt, None).await.context("could not remove the probe's key")?;
            let restore = || async {
                post(c, &keys, &jwt, serde_json::json!({ "name": name, "key": public }))
                    .await
                    .map(|_| ())
                    .context("the probe's key was left REMOVED")
            };
            let body = async {
                // STRICT for git: credential hits are not cached, so the very next request must be
                // refused. Only `Permission denied` counts — a DNS or host-key failure makes `ssh`
                // fail too, and reading that as a revocation keeps this green through an outage.
                let argv = vec!["ls-remote".to_string(), url];
                match tools::run(&git_bin, &argv, &env, None, Duration::from_secs(30)).await {
                    Ok(_) => return Err(anyhow!("a removed key could still read the repo over SSH")),
                    Err(e) if format!("{e:#}").contains("Permission denied") => {}
                    Err(e) => return Err(anyhow!("git failed for some other reason than a refusal: {e:#}")),
                }
                let start = std::time::Instant::now();
                loop {
                    let session = ssh_session(c, &id).await?;
                    let (ssh, kl) = (c.programs.ssh.clone(), c.programs.kl.clone());
                    let out = tools::run(&ssh, &ssh_args(&kl, &key_path, &id), &session_env(&session), None, TUNNEL_CEILING).await;
                    match out {
                        Err(e) if format!("{e:#}").contains("Permission denied") => return Ok(()),
                        Ok(_) => {}
                        Err(e) => tracing::info!(error = %format!("{e:#}"), "slo.key.revoked.waiting"),
                    }
                    if start.elapsed() >= REVOCATION_BODY {
                        return Err(anyhow!(
                            "a removed key still opened the workspace {} s after it was deleted",
                            REVOCATION_BODY.as_secs()
                        ));
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            };
            crate::drill::undoing(REVOCATION_BODY, body, restore).await
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

/// The connect ticket `kl-connect ws ssh` mints, as the JSON `kl-connect ws proxy` reads from its environment.
pub(crate) async fn ssh_session(c: &Ctx, id: &str) -> Result<String> {
    let url = api(c, &format!("/v1/workspaces/{id}/ssh-session"));
    let doc = post(c, &url, &c.probe_jwt.clone(), Value::Null)
        .await
        .context("could not mint an ssh session")?;
    Ok(doc.to_string())
}

/// What `kl-connect ws proxy` expects to be handed: the whole `Session` document, so the child makes no api
/// call and needs no `kl-connect login` state in the pod.
pub(crate) fn session_env(session: &str) -> std::collections::HashMap<String, String> {
    std::collections::HashMap::from([(SESSION_ENV.to_string(), session.to_string())])
}

/// `kl`'s own `proxy::SESSION_ENV` (`bins/kl-connect/src/proxy.rs`). Repeated rather than imported: `kl` is
/// a binary crate with no library target, so there is nothing to depend on — and the name is part
/// of the CLI's contract with ssh, which is exactly the kind of thing this probe exists to catch.
const SESSION_ENV: &str = "KL_SSH_SESSION";

/// ssh's argv for a workspace, through `kl-connect ws proxy`.
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
        // The id is INTERPOLATED into a command ssh runs through a shell, so it is checked
        // against the shape the api hands out before it gets there (2026-09-12) — an id carrying
        // a space or a metacharacter would be a command this probe ran on its own behalf.
        &format!("ProxyCommand={kl} ws proxy {}", safe_id(id)),
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

/// `quota.refused`: a verb that fills disk answers 409 with the sentence, and nothing is allocated.
///
/// Over the DISK dimension rather than the workspace count, deliberately: the journey itself needs
/// several workspaces (the clone, then two restores in stage 7), so a probe that leaned on the
/// count would either exhaust the quota its own later steps need or stop refusing the day the
/// deployment raised it.
///
/// Since 2026-09-17 disk is charged by what the volumes OCCUPY, so asking for a huge `quota_gb`
/// is no longer over anything — a ceiling was never a reservation. The only way to stand a verb
/// against the disk gate is to bring the LIMIT below what the run already occupies, which is what
/// the admin quota write here does, and the push is then refused by the same `guard_alloc` every
/// allocating verb passes. The restore is outside the cancellable region, exactly as
/// `env.quota.refused`'s is: a probe that left the limit PINCHED would refuse its own next run.
async fn quota_refused(c: &mut Ctx, ws: &str) {
    let ws = ws.to_string();
    c.step("quota.refused", QUOTA_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let push = api(c, &format!("/v1/workspaces/{ws}/push"));
        async move {
            let spec = pinched_disk(c, &jwt).await?;
            let refused = refused_over(c, reqwest::Method::POST, &push, &jwt, Some(serde_json::json!({})), "diskGb", "a push");
            pinching(c, spec, refused).await
        }
        .boxed()
    })
    .await;
}

/// Write `spec` as the probe owner's quota, run `body` against the gate, and put the yaml's quota
/// back on EVERY path out — a probe that left the quota PINCHED would refuse its own next run's
/// every create. The undo is outside the cancellable region for exactly that reason.
async fn pinching(c: &Ctx, spec: Value, body: impl std::future::Future<Output = Result<()>>) -> Result<()> {
    let admin_jwt = c.admin_jwt();
    let write = super::admin(c, &format!("/admin/quota/{}", c.probe_user));
    pinch_quota(c, &spec).await?;
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
    crate::drill::undoing(QUOTA_BODY, body, restore_quota).await
}

/// Write `spec` as the probe owner's quota. Shared with `request.approve`, which stands its
/// refused create against the same gate and restores the same way.
pub(super) async fn pinch_quota(c: &Ctx, spec: &Value) -> Result<()> {
    let write = super::admin(c, &format!("/admin/quota/{}", c.probe_user));
    let pinch = serde_json::json!({ "spec": spec, "note": "slo probe quota refusal" });
    super::call(c, reqwest::Method::PUT, &write, &c.admin_jwt(), Some(pinch))
        .await
        .map(|_| ())
        .context("could not bring the quota down to what the run holds")
}

/// The yaml's quota with `diskGb` brought just BELOW what the owner's volumes occupy, so the next
/// verb that fills is one over. Read from `/v1/quota`'s `disk` block — the same stamps the gate
/// sums — rather than from a ceiling, which is no longer what disk is charged on.
pub(super) async fn pinched_disk(c: &Ctx, jwt: &str) -> Result<Value> {
    let seen = super::get(c, &api(c, "/v1/quota"), jwt).await.context("could not read the quota")?;
    let used = seen.pointer("/disk/usedGb").and_then(Value::as_u64);
    let Some(used) = used.filter(|u| *u > 0) else {
        return Err(anyhow!("the quota answer carries no occupied disk to pinch against"));
    };
    let mut spec = super::experience_admin::probe_quota();
    let o = spec.as_object_mut().ok_or_else(|| anyhow!("the quota spec is not an object"))?;
    o.insert("diskGb".into(), (used - 1).into());
    Ok(spec)
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

/// `ws.build.p95`, hourly only: `kl container build` from inside the probe workspace,
/// dispatched to the owner's hidden builder through the gate, and the pushed manifest read back
/// over `/v2` with the probe's own registry credential.
///
/// Gated on the builder being `stopped` BEFORE the step, and outside its timing: a build that
/// itself starts a cold builder measures something real, but a run that finds the builder already
/// RUNNING is measuring somebody else's leftover job, not this one's cold start — and that is its
/// own finding, reported as a skip rather than folded into the sample as a pass.
///
/// The build script deliberately does NOT source `/etc/profile.d/kl-build.sh`: `ws_exec` is not a
/// login shell (see its own doc), and `kl container build` has to make its own buildx builder and credential
/// config for exactly that kind of exec — an editor's terminal, a CI hook.
async fn build_push(c: &mut Ctx, id: &str) {
    if c.suite != Suite::Hourly {
        return;
    }
    // The builder is created by this run's own first workspace, seconds before this: give the
    // controller's first pass its moment rather than reading the claim's interim phase.
    let mut stopped = false;
    for _ in 0..10 {
        let doc = match get(c, &api(c, "/v1/builders/me"), &c.probe_jwt.clone()).await {
            Ok(v) => v,
            Err(e) => return c.skip("ws.build.p95", &format!("could not read the builder: {e:#}")),
        };
        if doc.get("state").and_then(Value::as_str) == Some("stopped") {
            stopped = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    if !stopped {
        return c.skip("ws.build.p95", "builder was not stopped before the step");
    }
    let Some(secret) = c.state.token_value.clone() else {
        return c.skip("ws.build.p95", "no personal registry token");
    };
    let (probe, run_id, id) = (c.probe_user.clone(), c.run_id.clone(), id.to_string());
    c.step("ws.build.p95", BUILD_CEILING, move |c| {
        let (probe, run_id, id, secret) = (probe.clone(), run_id.clone(), id.clone(), secret.clone());
        async move {
            let script = build_script(&run_id);
            let (code, out, err) = ws_exec(c, &id, &script, BUILD_CEILING).await?;
            if code != 0 {
                return Err(anyhow!("the build/push failed ({code}): {} {}", out.trim(), err.trim()));
            }
            let scope = format!("repository:{probe}/slo-build:pull");
            let bearer = super::registry::bearer(c, Some(&secret), &scope)
                .await
                .context("could not mint a registry token to read the manifest back")?;
            let url = format!("{}/v2/{probe}/slo-build/manifests/{run_id}", super::registry::base(c));
            let (status, body) = raw(
                c,
                reqwest::Method::GET,
                &url,
                &bearer,
                None,
                &[("accept", "application/vnd.oci.image.manifest.v1+json".to_string())],
            )
            .await?;
            if !status.is_success() {
                return Err(anyhow!("the pushed manifest never appeared: {status}: {}", body.chars().take(200).collect::<String>()));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// The script the step runs, in the workspace, as the person — `kl container build` from a NON-login exec,
/// with none of `kl-build.sh`'s setup: that `kl` creates its own builder and credential config
/// is the clause the spec calls self-sufficiency, and this is where it is held. `kl` carries the
/// bootstrap retry the script used to (150 s of `buildx inspect --bootstrap`), still inside the
/// measurement: waking a stopped builder IS the cold start `ws.build.p95` exists to measure.
fn build_script(run_id: &str) -> String {
    format!(
        "mkdir -p /tmp/d\n\
printf 'FROM alpine:3.20\\nRUN echo slo > /slo\\n' > /tmp/d/Dockerfile\n\
kl container build -t slo-build:{run_id} /tmp/d"
    )
}

/// `kl container push` from the same workspace, then the promoted tag's digest read back through buildx's
/// own imagetools so the copy is verified by a second, independent reader.
fn promote_script(run_id: &str) -> String {
    format!(
        "kl container push slo-build:{run_id} slo-build:{run_id}-promoted\n\
docker buildx imagetools inspect $KL_REGISTRY_HOST/$KL_OWNER/slo-build:{run_id}-promoted --format '{{{{json .Manifest.Digest}}}}'"
    )
}

/// `ws.build.promote`, hourly only, right after the build so the source tag exists. Skipped —
/// never failed — when the build step itself did not pass, since a missing source says nothing
/// about `kl container push`.
async fn promote(c: &mut Ctx, id: &str) {
    if c.suite != Suite::Hourly {
        return;
    }
    if !c.passed("ws.build.p95") {
        return c.skip("ws.build.promote", "the build step did not pass");
    }
    let (run_id, id) = (c.run_id.clone(), id.to_string());
    c.step("ws.build.promote", PROMOTE_CEILING, move |c| {
        let (run_id, id) = (run_id.clone(), id.clone());
        async move {
            let (code, out, err) = ws_exec(c, &id, &promote_script(&run_id), PROMOTE_CEILING).await?;
            if code != 0 {
                return Err(anyhow!("`kl container push` failed ({code}): {} {}", out.trim(), err.trim()));
            }
            if !out.contains("\"sha256:") {
                return Err(anyhow!("the promoted tag's digest did not read back: {}", out.trim()));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `env.quota.refused`: the OTHER three verbs behind the one gate.
///
/// `quota.refused` above probes a create; restore, clone and push all route through the same
/// `guard_alloc`, and a gate wired into one of four call sites is a gate that hands out
/// allocation nobody decided on through the other three. All three in one step, first failure
/// wins — each alone says nothing about the others.
///
/// The dimensions differ on purpose and are what each verb actually overshoots. Since 2026-09-17
/// disk is charged by what the volumes OCCUPY, so NO verb can be made over-quota by asking for a
/// big ceiling any more — a declared `quota_gb` was never a reservation, and the restore that
/// asked for `u32::MAX` was answered 202 Accepted (hourly, 2026-09-17 03:43). The only way to
/// stand any of the three against the gate is to bring the LIMIT down to what the run already
/// holds: disk for the restore, the two counts for the clone and the push. Two pinches, not one,
/// because `guard_alloc` checks the disk limit before any count. The clone and the push are aimed
/// at THIS run's own workspace, so nothing new is allocated even on the path where the gate fails
/// open.
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
            // The restore carries no `quota_gb` at all: it takes the snapshot's own frozen state,
            // which is what a person restoring theirs gets, and the disk limit under it is what
            // makes it one over.
            let spec = pinched_disk(c, &jwt).await?;
            let body = serde_json::json!({ "name": name.clone(), "snapshot_id": snapshot });
            let one = refused_over(c, reqwest::Method::POST, &restore, &jwt, Some(body), "diskGb", "a restore");
            pinching(c, spec, one).await?;
            // Then the counts, for the two verbs that carry no size at all. Not the second
            // tenant's zero quota: `may_act_on` refuses another owner's workspace long before
            // `guard_alloc` is reached, so that would measure ownership and call it quota.
            let spec = pinched(c, &jwt).await?;
            let both = async {
                refused_over(c, reqwest::Method::POST, &clone, &jwt, Some(serde_json::json!({ "name": name })), "workspaces", "a clone").await?;
                refused_over(c, reqwest::Method::POST, &push, &jwt, Some(serde_json::json!({})), "snapshots", "a push").await
            };
            pinching(c, spec, both).await
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

/// `^[a-z0-9-]+$`, or a name that cannot be one. Every id `/v1` mints is `ws-{hex}`; anything
/// else is a compromised or confused answer, and the safe thing to hand a shell is a string that
/// resolves to nothing rather than one that runs.
pub(crate) fn safe_id(id: &str) -> &str {
    match id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') && !id.is_empty() {
        true => id,
        false => "invalid-workspace-id",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;

    /// The ProxyCommand IS the gateway path: without it ssh would dial the workspace directly,
    /// which nothing routes, and the step would measure a DNS failure. The session goes down to the
    #[test]
    fn the_build_script_runs_kl_container_build_without_sourcing_the_login_setup() {
        let s = build_script("hourly-1");
        assert!(s.contains("kl container build -t slo-build:hourly-1 /tmp/d"), "{s}");
        assert!(!s.contains("kl-build.sh"), "{s}");
    }

    /// Both judgements are on the API's answer, not on what `kl` printed: a pinned entry keeps its
    /// attribute, and only the caller's OWN space row answers the switch.
    #[test]
    fn the_kl_ids_judge_the_api_answer() {
        let doc = serde_json::json!({"packages": ["jq", "cowsay@3.04"]});
        assert!(declares(&doc, "cowsay"));
        assert!(!declares(&doc, "nodejs"));
        assert!(!declares(&serde_json::json!({}), "cowsay"));
        let me = serde_json::json!([{"team": "slo-other", "environment": "e9"}, {"team": "slo-probe", "environment": "e1"}]);
        assert_eq!(follows(&me, "slo-probe").as_deref(), Some("e1"));
        assert_eq!(follows(&me, "nobody"), None);
    }

    #[test]
    fn the_promote_script_reads_the_new_tag_back_through_imagetools() {
        let s = promote_script("hourly-1");
        assert!(s.contains("kl container push slo-build:hourly-1 slo-build:hourly-1-promoted"), "{s}");
        assert!(s.contains("imagetools inspect"), "{s}");
    }

    /// proxy child through the environment, exactly as `kl-connect ws ssh` hands it over.
    #[test]
    fn gateway_step_uses_kl_proxy() {
        let args = ssh_args("kl-connect", "/etc/slo-ssh/id_ed25519", "ws-abc");
        assert!(
            args.iter().any(|a| a == "ProxyCommand=kl-connect ws proxy ws-abc"),
            "{args:?}"
        );
        assert!(args.iter().any(|a| a == "kl@ws-abc"), "{args:?}");
        // Nothing to pin: the pod's host key is minted with the pod.
        assert!(args.iter().any(|a| a == "StrictHostKeyChecking=no"), "{args:?}");
        let env = session_env(r#"{"id":"ws-abc"}"#);
        assert_eq!(env.get("KL_SSH_SESSION").map(String::as_str), Some(r#"{"id":"ws-abc"}"#));
    }

    /// The one judgement `ws.exec.ok` turns on. A pod whose volume never mounted writes the
    /// person's files into its own overlay and loses them on restart — an id that only checked
    /// the echo would pass straight through it.
    #[test]
    fn the_exec_check_wants_the_output_and_the_home_volume() {
        assert!(home_is_volume("slo\nbtrfs\nbtrfs\n").is_ok());
        // gVisor passes the very same subvolume through its gofer, where it reads as 9p.
        assert!(home_is_volume("slo\nv9fs\n9p\n").is_ok());
        // Not the volume: the rootfs, a tmpfs, or the retired NFS home.
        assert!(home_is_volume("slo\noverlay\n").is_err());
        assert!(home_is_volume("slo\ntmpfs\n").is_err());
        assert!(home_is_volume("slo\nnfs\nnfs4\n").is_err());
        assert!(home_is_volume("slo\nbtrfs\noverlay\n").is_err());
        // And the ordinary weak check: an exec that connected and said nothing useful.
        assert!(home_is_volume("").is_err());
        assert!(home_is_volume("slo\n").is_err());
        assert!(home_is_volume("btrfs\n").is_err());
        // The script asks the two questions in that order.
        assert!(EXEC_SCRIPT.starts_with("echo slo"), "{EXEC_SCRIPT}");
        assert!(EXEC_SCRIPT.contains("/proc/mounts"), "{EXEC_SCRIPT}");
    }

    /// Every script in this file is handed to `/bin/sh`, which is DASH on the debian workspace
    /// image — so a bash-only construct is a step that fails on that image alone and passes on
    /// alpine, which is the shape of the 2026-09-17 `homes.rw.p95` failure. The source is read
    /// rather than the scripts enumerated, because a new script is exactly what this must catch;
    /// each needle is split so this test is not its own counter-example.
    #[test]
    fn no_bashism_reaches_a_sh_minus_c() {
        // Comments are skipped: they NAME the constructs (that is how the reason survives), and
        // only what a shell is handed matters.
        let src: String =
            include_str!("workspace.rs").lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n");
        for bad in [concat!("10", "#"), concat!("[", "[ "), concat!("<<", "<"), concat!("$", "{!"), concat!("pipe", "fail")] {
            assert!(!src.contains(bad), "{bad:?} is bash-only and /bin/sh is dash on the debian image");
        }
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
        // Centiseconds, combined arithmetically: `10#` is bash's base prefix and dash refuses it.
        assert!(script.contains("s * 100 + c"), "{script}");
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
