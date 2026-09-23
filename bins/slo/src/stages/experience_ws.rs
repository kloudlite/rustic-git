//! Stage 14's workspace-shaped verbs: packages, seeding, the platform key behind seeding, and the
//! home that travels with the workspace's own volume across a push and restore.
//!
//! A sibling file rather than lines inside `experience.rs` because four implementers fill that
//! scaffold's skips at once: `experience.rs` keeps one call per id and nothing else moves.
//!
//! Every step here needs a command INSIDE a pod — `which cowsay`, `git log`, `cat` — so a run with
//! no kubeconfig skips them all rather than failing: a missing kubeconfig is a deployment gap, not
//! an SLO breach, exactly as in stage 5.

use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;
use serde_json::{json, Value};

use super::git::BASE_BRANCH;
use super::workspace::ws_exec;
use super::{api, call, get, poll_json, post, raw};
use crate::tools;
use crate::ctx::Ctx;

/// Per-step ceilings, each at or above its catalogue target so a slow answer is a breach with a
/// number rather than a step the probe cut off. `key.platform.regenerate` and `home.travels` are
/// availability SLOs with no target latency; theirs is a whole seeded create plus an exec, which
/// is the same 180 s the seeded step itself is given, plus room for the second create.
const ADD_CEILING: Duration = Duration::from_secs(290);
const REMOVE_CEILING: Duration = Duration::from_secs(90);
const SEEDED_CEILING: Duration = Duration::from_secs(290);
const KEY_CEILING: Duration = Duration::from_secs(290);
const HOME_CEILING: Duration = Duration::from_secs(290);
/// Create, write, push, restore, read: two `ready` waits and a snapshot cut inside one ceiling.
const CACHE_CEILING: Duration = Duration::from_secs(290);
/// A create plus the server's own start (a graft build on an empty tree is seconds).
const IDE_CEILING: Duration = Duration::from_secs(240);

/// How long a create is given to reach `ready` INSIDE a step. Below every ceiling above, so a
/// workspace that never starts leaves room for the step to say so.
///
/// 150, not 110: the git-seed init container RETRIES its clone 24 times at 5 s
/// (`crates/workspaces/src/k8s.rs`, the seed command), which is a 120 s window a rotated platform
/// key is expected to be picked up inside — `key.platform.regenerate` was cut off at 108 s and
/// reported the fleet doing exactly what it is written to do. Every ceiling above it moved with
/// it, so a step still gets to say WHY rather than timing out on its own.
/// Long enough for the WHOLE seeded path after a platform-key rotation: the pod has to be
/// scheduled and pull its image (tens of seconds) and only then does the git-seed container start
/// retrying its clone for 24×5 s while the kubelet's secret cache still serves the pre-rotation
/// key (crates/workspaces/src/k8s.rs's seed command). 150 s covered the retries alone and expired
/// before the pod's own start, so `key.platform.regenerate` failed on a workspace that was fine.
const READY: Duration = Duration::from_secs(240);

/// One exec's own ceiling. The polls below repeat it, so this bounds a single API-server round
/// trip, not the wait.
const EXEC: Duration = Duration::from_secs(20);

/// The disk each probe workspace asks for, matching stage 5's — well inside `Quota/slo-probe`.
const QUOTA_GB: u64 = 1;

/// The package the two `ws.packages.*` steps add and remove. Small, has a binary of its own name,
/// and is in nixpkgs on every channel — the step measures the profile rebuild, not a build.
const PKG: &str = "cowsay";

/// The pinned entry the four `ws.packages.pin*` steps run against, and the prefix its lock must
/// carry. `jq@1.7` because jq is tiny, is on cache.nixos.org for every channel, and prints its own
/// version — so the pod itself can be asked whether the resolved version is the one that got
/// installed, which is the whole point of a pin.
const PINNED: &str = "jq@1.7";
const PINNED_PREFIX: &str = "1.7.";

/// A version nobody published. The refusal names the nearest ones, which is the assertion.
const UNKNOWN: &str = "jq@0.0.99";

/// The four ids `pin` reports, in catalogue order.
const PIN_IDS: [&str; 5] =
    ["ws.packages.pin", "ws.packages.pin.unknown", "ws.packages.pin.uncached", "ws.packages.update", "ws.packages.pin.lockshape"];

/// A release nixpkgs marks insecure (past its EOL) is one Hydra never builds, so no binary cache
/// will ever hold it: the one kind of "exists but uncached" that stays true. nodejs 20 reached
/// EOL in 2026; 20.20.2 was its last release.
const UNCACHED: &str = "nodejs@20.20.2";

/// The pin step is a whole create — schedule, pull, `nix copy` jq from cache.nixos.org, buildEnv —
/// plus an exec, which is `ws.packages.add`'s shape, so it gets `ws.packages.add`'s ceiling: at or
/// above the catalogue's 180 s target, with room for the step to say WHY rather than be cut off.
const PIN_CEILING: Duration = Duration::from_secs(290);

/// The other three are one API call each against a workspace that is already up.
const PIN_READ_CEILING: Duration = Duration::from_secs(60);

/// `ws.packages.add` and `ws.packages.remove`: the workspace they run against is created here and
/// kept for `home.travels`, which writes through it.
///
/// The create is NOT an SLO of its own (stage 5 already measures one) — it is inside the first
/// step, so a create that never becomes ready fails `ws.packages.add` with the reason instead of
/// vanishing, and `ws.packages.remove` skips.
pub async fn packages(c: &mut Ctx) {
    if c.kube.is_none() {
        c.skip("ws.packages.add", "no kubeconfig");
        return c.skip("ws.packages.remove", "no kubeconfig");
    }
    let name = format!("{}-x", c.prefix());
    let added = c
        .step("ws.packages.add", ADD_CEILING, move |c| {
            async move {
                let id = create(c, &name, json!({ "packages": ["bash"] })).await?;
                // Recorded before the wait: the workspace exists whatever the profile does, and
                // `home.travels` needs it either way.
                c.state.ux_workspace = Some(id.clone());
                set_packages(c, &id, &["bash", PKG]).await?;
                which(c, &id, true, ADD_CEILING).await
            }
            .boxed()
        })
        .await;
    c.state.ux_ready = added;
    let Some(id) = c.state.ux_workspace.clone() else {
        return c.skip("ws.packages.remove", "the workspace was never created");
    };
    if !added {
        return c.skip("ws.packages.remove", "the package was never added");
    }
    c.step("ws.packages.remove", REMOVE_CEILING, move |c| {
        async move {
            set_packages(c, &id, &["bash"]).await?;
            which(c, &id, false, REMOVE_CEILING).await
        }
        .boxed()
    })
    .await;
}

/// `ws.seeded`: a workspace created from the probe repo has that clone checked out.
///
/// The subject is `git.push.ok`'s own seed commit on `main` — the repo this run pushed, read back
/// from inside the pod, so this passes only if the init container cloned OUR repo at OUR commit.
pub async fn seeded(c: &mut Ctx) {
    if c.kube.is_none() {
        return c.skip("ws.seeded", "no kubeconfig");
    }
    let Some(repo) = c.state.repo.clone() else {
        return c.skip("ws.seeded", "the run pushed no repo");
    };
    let name = format!("{}-seed", c.prefix());
    c.step("ws.seeded", SEEDED_CEILING, move |c| {
        async move {
            let id = seed(c, &name, &repo).await?;
            // Kept for teardown's prefix sweep either way; deleted here so the run does not hold a
            // workspace of its quota for the rest of the stage.
            let out = match clone_subject(c, &id, &name).await {
                Ok(()) => names_its_seed(c, &id, &repo).await,
                e => e,
            };
            drop_ws(c, &id).await;
            out
        }
        .boxed()
    })
    .await;
}

/// `key.platform.regenerate`: the key seeding runs on is replaced, and a workspace created AFTER
/// the rotation still clones.
///
/// The second seeded workspace is the whole point: `POST /v1/platform-key` answering 200 proves
/// only that a key was written, and the failure this exists to catch is a rotation that leaves the
/// git tier authorising the old fingerprint — which nothing but a fresh clone can show.
pub async fn platform_key(c: &mut Ctx) {
    let probe = c.probe_user.clone();
    if c.kube.is_none() {
        return c.skip("key.platform.regenerate", "no kubeconfig");
    }
    let Some(repo) = c.state.repo.clone() else {
        return c.skip("key.platform.regenerate", "the run pushed no repo");
    };
    let name = format!("{}-seed2", c.prefix());
    c.step("key.platform.regenerate", KEY_CEILING, move |c| {
        let url = api(c, &format!("/v1/platform-key?owner={probe}"));
        async move {
            // The answer carries the PUBLIC key and its fingerprint only — no private half — so
            // nothing here can reach a step detail. It is dropped regardless: the step's claim is
            // that seeding still works, and the key material is not evidence of that.
            post(c, &url, &c.probe_jwt.clone(), Value::Null)
                .await
                .context("could not regenerate the platform key")?;
            let out = match seed(c, &name, &repo).await {
                Ok(id) => {
                    let out = clone_subject(c, &id, &name).await;
                    drop_ws(c, &id).await;
                    out
                }
                // The seed never came up. What is worth knowing then is the SERVER's view, not the
                // probe's guess: which fingerprint the api reports now, whether that key
                // authenticates from here, and what the seed container last said. Bounded, and
                // fingerprints only — key material never reaches a step detail.
                Err(e) => Err(anyhow!("{e:#}; {}", why_seeding_failed(c, &name).await)),
            };
            out
        }
        .boxed()
    })
    .await;
}

/// The server's own account of a failed seed, for the failure detail. Never an error of its own:
/// this runs only when the step has already failed, and a diagnostic that could fail would replace
/// the real reason with its own.
async fn why_seeding_failed(c: &Ctx, name: &str) -> String {
    let mut parts = vec![];
    let probe = c.probe_user.clone();
    // (a) the fingerprint the api reports NOW — a rotation the git tier never saw shows up as this
    // disagreeing with what the seed pod presented.
    let url = api(c, &format!("/v1/platform-key?owner={probe}"));
    let fp = tokio::time::timeout(DIAG_STEP, get(c, &url, &c.probe_jwt))
        .await
        .ok()
        .and_then(|r| r.ok())
        .and_then(|v| v.get("fingerprint").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "unreadable".into());
    parts.push(format!("the api now reports platform key {fp}"));
    // (b) whether the git tier accepts THIS pod's own key. `id.key.usable` uses the same binary
    // and the same host; a refusal here says the git tier is refusing a key the api says is
    // installed, which is the fault, and an acceptance says the seed pod's copy is stale.
    let (host, port) = c.cfg.ssh_endpoint();
    let target = format!("git@{host}");
    let argv: Vec<String> = ["-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-p"]
        .iter()
        .map(|a| (*a).to_string())
        .chain([port.to_string(), "-i".into(), c.cfg.ssh_key_path.clone(), "-T".into(), target])
        .collect();
    let said = match tokio::time::timeout(DIAG_STEP, tools::run(&c.programs.ssh, &argv, &Default::default(), None, DIAG_STEP)).await {
        Ok(Ok(_)) => "the probe's own key authenticates".to_string(),
        Ok(Err(e)) => {
            let detail = format!("{e:#}");
            match detail.contains("Permission denied") {
                true => "the git tier REFUSES the probe's own key".to_string(),
                false => format!("ssh said {}", detail.chars().take(120).collect::<String>()),
            }
        }
        Err(_) => "the ssh check timed out".to_string(),
    };
    parts.push(said);
    // (c) the seed container's own last line.
    if let Some(k) = &c.kube {
        let ns = kloudlite_workspaces::crd::ws_namespace(&probe, "");
        let pods: kube::Api<k8s_openapi::api::core::v1::Pod> = kube::Api::namespaced(k.clone(), &ns);
        let logs = tokio::time::timeout(
            DIAG_STEP,
            pods.logs(name, &kube::api::LogParams { container: Some(SEED_CONTAINER.into()), tail_lines: Some(3), ..Default::default() }),
        )
        .await;
        let last = match logs {
            Ok(Ok(text)) => text.lines().last().unwrap_or("").chars().take(160).collect::<String>(),
            _ => "unreadable".to_string(),
        };
        parts.push(format!("the seed container last said {last:?}"));
    }
    parts.join("; ")
}

/// Each diagnostic read's own bound. Three of them, so the whole capture stays inside 20 s and can
/// never turn a failed step into a timed-out one.
const DIAG_STEP: Duration = Duration::from_secs(6);

/// The init container `k8s::seed_container` names.
const SEED_CONTAINER: &str = "git-seed";

/// `home.travels`: a dotfile and a file under `~/workspace` written in workspace A are present in
/// B, restored from A's push, and absent from C, a fresh workspace of the same owner.
///
/// The last id in the stage, because it asserts something about what everything before it did:
/// since the 2026-09-22 ruling, `/home/kl` IS the workspace's own btrfs volume, no separate mount, no
/// shared NFS home — so what proves the home travels WITH the workspace (not with the owner) is a
/// restore that carries the dotfile, and a fresh, unrelated create that does NOT.
pub async fn home_travels(c: &mut Ctx) {
    if c.kube.is_none() {
        return c.skip("home.travels", "no kubeconfig");
    }
    let Some(a) = c.state.ux_workspace.clone() else {
        return c.skip("home.travels", "no workspace to write the home from");
    };
    let b = format!("{}-home-b", c.prefix());
    let c_name = format!("{}-home-c", c.prefix());
    let want = c.run_id.clone();
    c.step("home.travels", HOME_CEILING, move |c| {
        async move {
            // `~/workspace` may not exist yet (A never seeded a repo): `mkdir -p` it first.
            let write = format!(
                "set -e\nmkdir -p ~/.config ~/workspace\nprintf %s {want} > {DOTFILE}\nprintf %s {want} > {TREE_FILE}\nsync {DOTFILE} {TREE_FILE}"
            );
            let (code, _, err) = ws_exec(c, &a, &write, EXEC).await?;
            if code != 0 {
                drop_ws(c, &a).await;
                return Err(anyhow!("writing the home files exited {code}: {}", err.trim()));
            }
            let restored = push_then_restore(c, &a, &b).await;
            drop_ws(c, &a).await;
            let b = restored?;
            let (code, dot_out, err) = ws_exec(c, &b, &format!("cat {DOTFILE}"), EXEC).await?;
            if code != 0 {
                drop_ws(c, &b).await;
                return Err(anyhow!("reading the dotfile in the restored copy exited {code}: {}", err.trim()));
            }
            let (code, tree_out, err) = ws_exec(c, &b, &format!("cat {TREE_FILE}"), EXEC).await?;
            drop_ws(c, &b).await;
            if code != 0 {
                return Err(anyhow!("reading the tree file in the restored copy exited {code}: {}", err.trim()));
            }
            if dot_out.trim() != want {
                return Err(anyhow!("the restored copy's dotfile read back {:?}", dot_out.trim()));
            }
            if tree_out.trim() != want {
                return Err(anyhow!("the restored copy's tree file read back {:?}", tree_out.trim()));
            }
            let fresh = create(c, &c_name, json!({ "packages": [] })).await?;
            let (code, out, _) = ws_exec(c, &fresh, &format!("test -e {DOTFILE}"), EXEC).await?;
            drop_ws(c, &fresh).await;
            if code == 0 {
                return Err(anyhow!("a fresh workspace of the same owner already had the dotfile: {:?}", out.trim()));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `ws.cache.travels`: build output written under `/home/kl/.cache` travels with a push and is there
/// on a restore — the property the 2026-09-11 move of `CARGO_TARGET_DIR` into the tree exists for.
/// Read on the RESTORED copy: the source proves nothing.
pub async fn cache_in_tree(c: &mut Ctx) {
    if c.kube.is_none() {
        return c.skip("ws.cache.travels", "no kubeconfig");
    }
    let name = format!("{}-cache", c.prefix());
    let want = c.run_id.clone();
    c.step("ws.cache.travels", CACHE_CEILING, move |c| {
        async move {
            let src = create(c, &name, json!({ "packages": [] })).await?;
            // Before the push: once pushed the volume outlives both workspaces, and it is named by
            // the workspace id, so no prefix sweep ever sees it — teardown deletes it by name.
            c.state.extra_volumes.push(src.clone());
            let write = format!("set -e\nmkdir -p \"$CARGO_TARGET_DIR\"\nprintf %s {want} > \"$CARGO_TARGET_DIR/marker\"\nsync \"$CARGO_TARGET_DIR/marker\"");
            let (code, _, err) = ws_exec(c, &src, &write, EXEC).await?;
            if code != 0 {
                drop_ws(c, &src).await;
                drop_vol(c, &src).await;
                return Err(anyhow!("writing under the cache exited {code}: {}", err.trim()));
            }
            let out = push_then_restore(c, &src, &format!("{name}-r"), &name).await;
            drop_ws(c, &src).await;
            let restored = match out {
                Ok(r) => r,
                Err(e) => {
                    drop_vol(c, &src).await;
                    return Err(e);
                }
            };
            let (code, out, err) = ws_exec(c, &restored, "cat \"$CARGO_TARGET_DIR/marker\"", EXEC).await?;
            drop_ws(c, &restored).await;
            drop_vol(c, &src).await;
            if code != 0 {
                return Err(anyhow!("the restored copy has no marker under its cache: exit {code}: {}", err.trim()));
            }
            if out.trim() != want {
                return Err(anyhow!("the restored copy read back {:?}", out.trim()));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `ide.serve.up` and `ide.exec`: the workspace tool server (`kl ide serve`) is up inside a
/// fresh pod and answers an exec through MCP. Both read from INSIDE the pod over loopback — the
/// same bytes the ssh tunnel carries — so a broken prelude, a missing node/graft, or a server that
/// starts and dies all show here rather than in a person's first session.
pub async fn ide_server(c: &mut Ctx) {
    if c.kube.is_none() {
        c.skip("ide.serve.up", "no kubeconfig");
        return c.skip("ide.exec", "no kubeconfig");
    }
    let name = format!("{}-ide", c.prefix());
    let mut ws_id: Option<String> = None;
    let up = c
        .step("ide.serve.up", IDE_CEILING, |c| {
            let name = name.clone();
            async move {
                let id = create(c, &name, json!({ "packages": [] })).await?;
                c.state.extra_workspaces.push(id.clone());
                let start = Instant::now();
                let mut last = String::new();
                while start.elapsed() < IDE_CEILING - Duration::from_secs(10) {
                    let (code, out, err) = ws_exec(c, &id, "curl -sf http://127.0.0.1:7788/healthz", EXEC).await?;
                    if code == 0 && out.contains("\"ok\":true") {
                        return Ok(());
                    }
                    last = format!("exit {code}: {} {}", out.trim(), err.trim());
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(anyhow!("the tool server never answered /healthz; last: {last}"))
            }
            .boxed()
        })
        .await;
    // `step` answers whether it passed; the id travels through the closure's return, so re-read
    // it from the state the create pushed rather than threading a second channel.
    if up {
        ws_id = c.state.extra_workspaces.last().cloned();
    }
    let Some(id) = ws_id else {
        return c.skip("ide.exec", "the tool server never came up");
    };
    // Cloned: the step's closure takes one, and the sandbox check after it needs the same pod.
    let for_step = id.clone();
    c.step("ide.exec", EXEC, move |c| {
        async move {
            // Output, not only an exit code: build fb3673f1's login shell died in /etc/profile with
            // status 0 and `true` passed on a server that ran nothing.
            let body = r#"{"cmd":"echo ide-$(id -un)"}"#;
            // The token from the file this container mounts: the tool server takes nothing else.
            let script = format!(
                "curl -sf -X POST http://127.0.0.1:7788/tools/exec -H 'content-type: application/json' \
                 -H \"authorization: Bearer $(cat {WS_TOKEN_FILE})\" -d '{body}'"
            );
            let (code, out, err) = ws_exec(c, &for_step, &script, EXEC).await?;
            if code != 0 {
                return Err(anyhow!("the tool call exited {code}: {}", err.trim()));
            }
            if !out.contains("\"exit_code\":0") || !out.contains("ide-kl") {
                return Err(anyhow!("exec through the tool server did not run as kl with exit_code 0: {}", out.trim()));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
    sandbox_active(c, &id).await;
    exec_https(c, &id).await;
    drop_ws(c, &id).await;
}

/// `ide.exec.https`: a wrapped exec can still fetch over HTTPS.
///
/// The wrapper's first working roll took every HTTPS fetch in the fleet with it — inside the
/// sandbox `/etc` held only `passwd` and `resolv.conf`, so no CA bundle was reachable and
/// git-over-https, npm, cargo, pip and go mod all failed at once (2026-09-18). The sandbox's own
/// preflight asks whether a bundle is READABLE; this asks whether a fetch actually works, which is
/// the thing people lost.
///
/// Through the tool server, so it measures what a session gets — a `kubectl exec` runs outside the
/// wrapper and would pass while every real exec failed, which is exactly the shape of check that
/// has now missed three of these.
async fn exec_https(c: &mut Ctx, id: &str) {
    const ID: &str = "ide.exec.https";
    if c.kube.is_none() {
        return c.skip(ID, "no kubeconfig");
    }
    let ws = id.to_string();
    c.step(ID, EXEC, move |c| {
        let ws = ws.clone();
        async move {
            // Our OWN endpoint, not a third party's: a probe that fails when somebody else's
            // site is down would report our outage for their incident. `web_url` is https in
            // every deployed region, which is the whole point of the fetch.
            let url = c.cfg.web_url.clone();
            if !url.starts_with("https://") {
                return Err(anyhow!("{url} is not https, so this proves nothing about the trust store"));
            }
            let cmd = format!("curl -sS -m 10 -o /dev/null -w '%{{http_code}}' {url}");
            let (status, body) = ws_tool(c, &ws, "exec", &serde_json::json!({ "cmd": cmd })).await?;
            if status != 200 {
                return Err(anyhow!("the tool server answered {status}: {}", body.trim()));
            }
            let v: Value = serde_json::from_str(&body).context("parsing the exec answer")?;
            let (code, out, err) = (
                v["exit_code"].as_i64().unwrap_or(-1),
                v["stdout"].as_str().unwrap_or("").trim().to_string(),
                v["stderr"].as_str().unwrap_or("").trim().to_string(),
            );
            if code != 0 {
                // curl 60 is the certificate failure this id exists for; say so rather than
                // leaving a number for somebody to look up.
                let hint = if err.contains("certificate") || code == 60 {
                    " — the sandbox has no readable CA bundle"
                } else {
                    ""
                };
                return Err(anyhow!("an HTTPS fetch from inside a wrapped exec failed (curl {code}){hint}: {err}"));
            }
            if !out.starts_with('2') && !out.starts_with('3') {
                return Err(anyhow!("{url} answered {out} from inside the sandbox"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `ide.sandbox.active`: the tool server says, in its own log, that it wrapped an exec.
///
/// A POSITIVE signal on purpose. Three outages came from believing the sandbox was on because
/// nothing said it was off — a log with no `ide.sandbox.unavailable` is equally what a server that
/// has run no execs looks like. The line is written once, by the preflight that actually started
/// `bwrap` with the real flags, so its presence is the only thing that means execs are sandboxed.
///
/// Runs AFTER `ide.exec`, which is the first exec: the preflight is lazy, so before it there is
/// nothing to have said anything.
///
/// A failure reads "execs are not sandboxed" and carries the reason the server gave, because
/// `ide.sandbox.unavailable` names WHY on the same line — today, on the fleet, that is
/// `preflight:bwrap: setting up uid map: Operation not permitted` (spec §4.7), which is a known
/// open item rather than a regression.
async fn sandbox_active(c: &mut Ctx, id: &str) {
    const ID: &str = "ide.sandbox.active";
    if c.kube.is_none() {
        return c.skip(ID, "no kubeconfig");
    }
    let ws = id.to_string();
    c.step(ID, EXEC, move |c| {
        let ws = ws.clone();
        async move {
            // The FILE, not `kubectl logs`: the pod prelude starts the server with its output
            // redirected to this path, so the container's stdout carries none of it and a probe
            // reading the container log finds nothing whatever the sandbox did (2026-09-18).
            let script = format!("cat {IDE_LOG} 2>&1 || true");
            let (code, out, _) = ws_exec(c, &ws, &script, EXEC).await?;
            if code != 0 {
                return Err(anyhow!("could not read {IDE_LOG}: exit {code}"));
            }
            if out.contains("ide.sandbox.active") {
                return Ok(());
            }
            // The server's own reason if it gave one, so a failure is a sentence rather than an
            // absence somebody has to go and look up.
            let why = out
                .lines()
                .rev()
                .find(|l| l.contains("ide.sandbox.unavailable"))
                .map(|l| l.trim().chars().take(200).collect::<String>())
                .unwrap_or_else(|| "the server said nothing about the sandbox at all".to_string());
            Err(anyhow!("execs are not sandboxed: {why}"))
        }
        .boxed()
    })
    .await;
}

/// Where the pod prelude sends the tool server's output — the crate that WRITES the prelude owns
/// the path, so the two cannot drift into a probe reading an empty file and calling it a failure.
use kloudlite_workspaces::k8s::IDE_LOG;

/// One `POST /tools/{name}` against the workspace's own tool server, from INSIDE the pod over
/// loopback. Answers the HTTP status and the body, both, because a tree probe cares which refusal
/// it got (a 403 that should be a 400 is the whole point of §3.5) and not only that it failed.
///
/// `curl -s -o - -w` rather than `-f`: `-f` swallows the body on a 4xx, which is exactly the body
/// these steps assert on.
/// The token file the keys beat projects into every workspace container, and the credential the
/// tool server checks (`kloudlite_ide::auth::TOKEN_PATH`, `k8s::secrets::USER_KEY_PATH` + the
/// Secret key). Spelled here rather than imported: `bins/slo` does not depend on `kloudlite-ide`,
/// and one path constant is not a reason to make it. `the_token_file_is_the_one_the_pod_mounts`
/// holds this equal to the workspaces crate's own.
const WS_TOKEN_FILE: &str = "/etc/kloudlite/ssh/workspace-token";

pub(crate) async fn ws_tool(c: &Ctx, id: &str, tool: &str, args: &Value) -> Result<(u16, String)> {
    ws_tool_within(c, id, tool, args, EXEC).await
}

/// The same, with the caller's own bound — a 200 MB `dd` through `exec` is not a `read`.
pub(crate) async fn ws_tool_within(c: &Ctx, id: &str, tool: &str, args: &Value, bound: Duration) -> Result<(u16, String)> {
    // Single-quoted into a shell, so a single quote inside the JSON would end the string early.
    // None of ours carry one today; escaped anyway, because a probe that mangles its own request
    // reports a fleet failure that is its own.
    let body = serde_json::to_string(args)?.replace('\'', r"'\''");
    // The token comes from the file the keys beat projects into THIS container — read inside the
    // pod, never carried in from the probe, so the call is exactly the one a session makes. The
    // shell sidecar has no such file, which is the whole fence (`shell.no_tools`).
    let script = format!(
        "curl -s -o /tmp/kl-tool.out -w '%{{http_code}}' -X POST http://127.0.0.1:7788/tools/{tool} \
         -H 'content-type: application/json' \
         -H \"authorization: Bearer $(cat {WS_TOKEN_FILE})\" -d '{body}'; echo; cat /tmp/kl-tool.out"
    );
    let (code, out, err) = ws_exec(c, id, &script, bound).await?;
    if code != 0 {
        return Err(anyhow!("the tool call could not be made: exit {code}: {}", err.trim()));
    }
    let (status, body) = out.split_once('\n').ok_or_else(|| anyhow!("no status line in {out:?}"))?;
    let status: u16 = status.trim().parse().with_context(|| format!("status was {status:?}"))?;
    Ok((status, body.to_string()))
}

/// Push `src`, wait for the snapshot to turn ready, restore it under `name`, wait for `ready`;
/// answers the restored workspace's id. The volume is named after the workspace.
pub(crate) async fn push_then_restore(c: &mut Ctx, src: &str, name: &str, message: &str) -> Result<String> {
    let jwt = c.probe_jwt.clone();
    let doc = post(c, &api(c, &format!("/v1/workspaces/{src}/push")), &jwt, json!({ "message": message })).await.context("could not push")?;
    let snap = doc.get("id").and_then(Value::as_str).ok_or_else(|| anyhow!("the push answered no snapshot id"))?.to_string();
    let history = api(c, &format!("/v1/volumes/{src}/history"));
    poll_json(c, &history, &jwt, CACHE_CEILING / 2, |v| super::workspace::row_ready(v, &snap)).await.context("the snapshot never turned ready")?;
    let body = json!({ "name": name, "snapshot_id": snap });
    let doc = post(c, &api(c, "/v1/workspaces/restore"), &jwt, body).await.context("could not restore")?;
    let id = doc.get("id").and_then(Value::as_str).ok_or_else(|| anyhow!("the restore answered no workspace id"))?.to_string();
    c.state.extra_workspaces.push(id.clone());
    let ws = api(c, &format!("/v1/workspaces/{id}"));
    poll_json(c, &ws, &jwt, CACHE_CEILING / 2, |v| v.get("state").and_then(Value::as_str) == Some("ready")).await?;
    Ok(id)
}

/// The two files `home.travels` writes in A and reads back in B: a dotfile under `~/.config` and
/// a file under `~/workspace`, so the SLI covers both the config path and the tree the volume IS.
const DOTFILE: &str = "~/.config/kl-probe";
const TREE_FILE: &str = "~/workspace/probe.txt";

/// Create a workspace and wait for `ready`. Answers its id.
pub(crate) async fn create(c: &Ctx, name: &str, extra: Value) -> Result<String> {
    let mut body = json!({ "name": name, "region": c.cfg.region, "quota_gb": QUOTA_GB });
    let (Some(o), Some(e)) = (body.as_object_mut(), extra.as_object()) else {
        return Err(anyhow!("bad request body"));
    };
    o.extend(e.iter().map(|(k, v)| (k.clone(), v.clone())));
    let doc = post(c, &api(c, "/v1/workspaces"), &c.probe_jwt, body)
        .await
        .with_context(|| format!("could not create {name}"))?;
    let id = doc
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("the create answered no workspace id"))?
        .to_string();
    let url = api(c, &format!("/v1/workspaces/{id}"));
    poll_json(c, &url, &c.probe_jwt, READY, |v| {
        v.get("state").and_then(Value::as_str) == Some("ready")
    })
    .await
    .with_context(|| format!("{name} never became ready"))?;
    Ok(id)
}

/// A workspace seeded from the run's own repo, on the branch stage 2 pushed first.
async fn seed(c: &Ctx, name: &str, repo: &str) -> Result<String> {
    let probe = c.probe_user.clone();
    // `owner/name`, never a URL: that is what `/v1` accepts, deliberately (a URL here would be an
    // egress primitive for anyone who can create a workspace).
    let extra = json!({ "repo": format!("{probe}/{repo}"), "branch": BASE_BRANCH, "packages": [] });
    create(c, name, extra).await
}

/// The doc says where the workspace came from — the web's "open in a workspace" matches on it,
/// and a workspace with no `repo` in its doc was created empty rather than seeded and lost.
async fn names_its_seed(c: &Ctx, id: &str, repo: &str) -> Result<()> {
    let doc = super::get(c, &api(c, &format!("/v1/workspaces/{id}")), &c.probe_jwt).await?;
    let want = format!("{}/{repo}", c.probe_user);
    let (r, b) = (doc.get("repo").and_then(Value::as_str), doc.get("branch").and_then(Value::as_str));
    if r != Some(want.as_str()) || b != Some(BASE_BRANCH) {
        return Err(anyhow!("the doc names {r:?} at {b:?}, not {want} at {BASE_BRANCH}"));
    }
    Ok(())
}

/// `ws.seed.failed`: a workspace seeded from a repository that does not exist says so —
/// `Ready=False/SeedFailed` — instead of sitting at `Creating` with nothing to act on.
///
/// The clone retries in place for two minutes before the init container fails once, so the
/// condition takes at least that long to appear; the ceiling leaves room above it.
pub async fn seed_failed(c: &mut Ctx) {
    if c.kube.is_none() {
        return c.skip("ws.seed.failed", "no kubeconfig");
    }
    let name = format!("{}-noseed", c.prefix());
    let probe = c.probe_user.clone();
    c.step("ws.seed.failed", SEED_FAILED_CEILING, move |c| {
        async move {
            let body = json!({
                "name": name, "region": c.cfg.region, "quota_gb": QUOTA_GB,
                "repo": format!("{probe}/{name}-does-not-exist"), "branch": BASE_BRANCH, "packages": [],
            });
            let doc = post(c, &api(c, "/v1/workspaces"), &c.probe_jwt, body).await.context("could not create the workspace")?;
            let id = doc.get("id").and_then(Value::as_str).ok_or_else(|| anyhow!("the create answered no workspace id"))?.to_string();
            let out = seed_failed_condition(c, &id).await;
            drop_ws(c, &id).await;
            out
        }
        .boxed()
    })
    .await;
}

const SEED_FAILED_CEILING: Duration = Duration::from_secs(240);

async fn seed_failed_condition(c: &Ctx, id: &str) -> Result<()> {
    let k = c.kube.as_ref().ok_or_else(|| anyhow!("no kubeconfig"))?;
    let ws: kube::Api<kloudlite_workspaces::crd::Workspace> = kube::Api::all(k.clone());
    let started = std::time::Instant::now();
    let mut last = String::new();
    while started.elapsed() < SEED_FAILED_CEILING - Duration::from_secs(20) {
        let w = ws.get(id).await.map_err(|e| anyhow!("could not read the Workspace {id}: {e}"))?;
        let ready = w.status.as_ref().and_then(|s| s.conditions.iter().find(|c| c.type_ == "Ready").cloned());
        if let Some(r) = ready {
            if r.reason == "SeedFailed" {
                return if r.message.contains("git-seed") { Ok(()) } else { Err(anyhow!("SeedFailed without the log pointer: {}", r.message)) };
            }
            last = format!("{}/{}", r.reason, r.message);
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    Err(anyhow!("never reported SeedFailed; last Ready was {last}"))
}

/// The subject of the checked-out clone's last commit, compared to the one this run pushed.
///
/// The path is `k8s::WORKSPACE_DIR` — the seeder clones into the workspace's own volume, the whole
/// home now, NOT into a directory named after the repo.
async fn clone_subject(c: &Ctx, id: &str, _name: &str) -> Result<()> {
    let dir = kloudlite_workspaces::k8s::WORKSPACE_DIR;
    let script = format!("git -C {dir} rev-parse HEAD");
    let (code, out, err) = ws_exec(c, id, &script, EXEC).await?;
    if code != 0 {
        return Err(anyhow!("the clone is not there: exit {code}: {}", err.trim()));
    }
    // Against the branch's CURRENT tip, not a remembered subject: by stage 14 the fast journey
    // has merged a change into `main`, so "seed" is no longer what a fresh clone checks out.
    let refs = api(c, &format!("/api/{}/{}/refs", c.probe_user, c.state.repo.clone().unwrap_or_default()));
    let listed = super::get(c, &refs, &c.probe_jwt).await.context("could not read the branch tip")?;
    let tip = listed
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r.get("name").and_then(Value::as_str) == Some("refs/heads/main"))
        .and_then(|r| r.get("oid").and_then(Value::as_str))
        .ok_or_else(|| anyhow!("the branch has no tip"))?;
    if out.trim() != tip {
        return Err(anyhow!("the checked-out commit is {} but main is {tip}", out.trim()));
    }
    Ok(())
}

/// `git.push.ok`'s own commit message. Repeated rather than imported because it is a literal in the
/// push step's argv; the test below is what keeps the two from drifting apart.
#[cfg(test)]
const SEED_SUBJECT: &str = "seed";

/// Replace the declared package list. A merge patch on `spec.packages` alone, which is what
/// `PATCH /v1/workspaces/{id}` is.
async fn set_packages(c: &Ctx, id: &str, packages: &[&str]) -> Result<()> {
    let url = api(c, &format!("/v1/workspaces/{id}"));
    let body = json!({ "packages": packages });
    call(c, reqwest::Method::PATCH, &url, &c.probe_jwt, Some(body))
        .await
        .context("could not change the package list")
        .map(|_| ())
}

/// Poll `which cowsay` inside the pod until it says what `want` expects.
///
/// Both directions matter and neither is instant: a profile rebuild publishes a new generation the
/// pod's `PATH` picks up on the next exec, so "added" and "removed" are both waits, not reads.
async fn which(c: &Ctx, id: &str, want: bool, cap: Duration) -> Result<()> {
    let script = format!("command -v {PKG}");
    // Inside the step's own ceiling, so a poll that never converges reports WHAT it last saw
    // rather than the step's bare "timed out" swallowing the evidence.
    let cap = cap.saturating_sub(Duration::from_secs(10));
    let start = Instant::now();
    let mut why;
    loop {
        match ws_exec(c, id, &script, EXEC).await {
            Ok((code, _, _)) if (code == 0) == want => return Ok(()),
            Ok((code, _, _)) => {
                why = format!("`{script}` exits {code}");
            }
            Err(e) => why = format!("{e:#}"),
        }
        if start.elapsed() >= cap {
            let wanted = if want { "runnable" } else { "gone" };
            return Err(anyhow!("{PKG} is not {wanted} after {} ms: {why}", cap.as_millis()));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// `ws.packages.pin`, `ws.packages.pin.unknown`, `ws.packages.update` and
/// `ws.packages.pin.lockshape`: one pinned workspace, four assertions about it.
///
/// One journey rather than four creates: the pin is the expensive part (a real substitution from
/// cache.nixos.org), and the other three read or PATCH the workspace it left running. The create
/// is inside the first step, as `packages` does it, so a workspace that never becomes ready fails
/// `ws.packages.pin` with the reason and the rest skip.
pub async fn pin(c: &mut Ctx) {
    if c.kube.is_none() {
        for id in PIN_IDS {
            c.skip(id, "no kubeconfig");
        }
        return;
    }
    let name = format!("{}-pin", c.prefix());
    let pinned = c
        .step("ws.packages.pin", PIN_CEILING, move |c| {
            async move {
                let id = create(c, &name, json!({ "packages": [PINNED] })).await?;
                // Recorded before the assertion: the workspace exists whatever the lock says, and
                // teardown finds it by name either way.
                c.state.pin_workspace = Some(id.clone());
                let version = locked_version(c, &id).await?;
                if !version.starts_with(PINNED_PREFIX) {
                    return Err(anyhow!("{PINNED} locked {version}, which is not a {PINNED_PREFIX}x"));
                }
                // The pod, not the lock: a lock nobody installed is a row in an object. `jq
                // --version` prints `jq-1.7.1`, so the locked version has to be IN it.
                let (code, out, err) = ws_exec(c, &id, "jq --version", EXEC).await?;
                if code != 0 {
                    return Err(anyhow!("`jq --version` exits {code}: {}", err.trim()));
                }
                if !out.contains(&version) {
                    return Err(anyhow!("the lock says {version} but the pod says {}", out.trim()));
                }
                Ok(())
            }
            .boxed()
        })
        .await;
    let Some(id) = c.state.pin_workspace.clone() else {
        for id in PIN_IDS.iter().skip(1) {
            c.skip(id, "the pinned workspace was never created");
        }
        return;
    };

    // A refusal, so it does not need the pin to have succeeded — only the workspace to exist.
    let refused = id.clone();
    c.step("ws.packages.pin.unknown", PIN_READ_CEILING, move |c| {
        async move {
            let url = api(c, &format!("/v1/workspaces/{refused}"));
            let body = json!({ "packages": [UNKNOWN] });
            let (status, text) =
                raw(c, reqwest::Method::PATCH, &url, &c.probe_jwt.clone(), Some(body), &[]).await?;
            if status != reqwest::StatusCode::UNPROCESSABLE_ENTITY {
                return Err(anyhow!("{UNKNOWN} answered {status}, not 422: {}", text.trim()));
            }
            // The nearest versions are the whole point of the refusal: "no" without them leaves a
            // person guessing what to type instead.
            if !text.contains("nearest:") {
                return Err(anyhow!("the refusal names no nearer version: {}", text.trim()));
            }
            Ok(())
        }
        .boxed()
    })
    .await;

    // Also a refusal against the same workspace: a version that exists but was never built.
    let uncached = id.clone();
    c.step("ws.packages.pin.uncached", PIN_READ_CEILING, move |c| {
        async move {
            let url = api(c, &format!("/v1/workspaces/{uncached}"));
            let body = json!({ "packages": [UNCACHED] });
            let (status, text) =
                raw(c, reqwest::Method::PATCH, &url, &c.probe_jwt.clone(), Some(body), &[]).await?;
            if status != reqwest::StatusCode::UNPROCESSABLE_ENTITY {
                return Err(anyhow!("{UNCACHED} answered {status}, not 422: {}", text.trim()));
            }
            // The alternatives are the point: a person pinning an EOL release needs the nearest
            // release that has a binary, not a bare no.
            if !text.contains("no cached build") || !text.contains("nearest cached:") {
                return Err(anyhow!("the refusal names no cached alternative: {}", text.trim()));
            }
            Ok(())
        }
        .boxed()
    })
    .await;

    if pinned {
        let updated = id.clone();
        c.step("ws.packages.update", PIN_READ_CEILING, move |c| {
            async move {
                let before = locked_version(c, &updated).await?;
                let url = api(c, &format!("/v1/workspaces/{updated}/packages/update"));
                post(c, &url, &c.probe_jwt.clone(), Value::Null)
                    .await
                    .context("the update pass was refused")?;
                let after = locked_version(c, &updated).await?;
                // An exact pin has one answer: a re-resolution that moved it would rebuild a
                // workspace nobody asked to change.
                if before != after {
                    return Err(anyhow!("the update moved {PINNED} from {before} to {after}"));
                }
                Ok(())
            }
            .boxed()
        })
        .await;

        let shaped = id.clone();
        c.step("ws.packages.pin.lockshape", PIN_READ_CEILING, move |c| {
            async move { lock_shape(c, &shaped).await }.boxed()
        })
        .await;
    } else {
        for id in ["ws.packages.update", "ws.packages.pin.lockshape"] {
            c.skip(id, "the pin never resolved");
        }
    }

    // The assertions are made; the pod is 2 vCPU of a pool node the rest of the stage needs.
    super::lifecycle::park(c, &id).await;
}

/// What `PINNED` currently resolves to, read from the workspace doc.
async fn locked_version(c: &Ctx, id: &str) -> Result<String> {
    let doc = get(c, &api(c, &format!("/v1/workspaces/{id}")), &c.probe_jwt).await.context("could not read the workspace")?;
    let lock = doc
        .get("locks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|l| l.get("entry").and_then(Value::as_str) == Some(PINNED))
        .ok_or_else(|| anyhow!("the workspace carries no lock for {PINNED}"))?;
    let version = lock.get("version").and_then(Value::as_str).unwrap_or_default();
    if version.is_empty() {
        return Err(anyhow!("the lock for {PINNED} names no version"));
    }
    // The doc's `rev` is the nixpkgs commit every node rebuilds from, so its shape is an
    // assertion, not a formality: a short or non-hex rev is not a revision anyone can fetch.
    let rev = lock.get("rev").and_then(Value::as_str).unwrap_or_default();
    if rev.len() != 40 || !rev.chars().all(|ch| ch.is_ascii_digit() || ('a'..='f').contains(&ch)) {
        return Err(anyhow!("the lock's rev is {rev:?}, not a 40-character nixpkgs revision"));
    }
    Ok(version.to_string())
}

/// `ws.packages.pin.lockshape`: the lock names a revision AND a store path.
///
/// The store path is deliberately not in the doc — it is the agent's business — so this reads the
/// `Workspace` CR, which is where the agent reads it from too.
async fn lock_shape(c: &Ctx, id: &str) -> Result<()> {
    // The rev half is `locked_version`'s, so both halves are asserted in one place.
    locked_version(c, id).await?;
    let k = c.kube.as_ref().ok_or_else(|| anyhow!("no kubeconfig"))?;
    let ws: kube::Api<kloudlite_workspaces::crd::Workspace> = kube::Api::all(k.clone());
    let w = ws.get(id).await.map_err(|e| anyhow!("could not read the Workspace {id}: {e}"))?;
    let lock = w
        .spec
        .locks
        .iter()
        .find(|l| l.entry == PINNED)
        .ok_or_else(|| anyhow!("the spec carries no lock for {PINNED}"))?;
    // Only a Nixhub lock carries a store path; a mirror lock's is evaluated by the agent at build
    // time, so a day the index answered from the mirror is not a broken lock.
    let nixhub = lock.source == kloudlite_workspaces::crd::LockSource::Nixhub;
    if nixhub && !lock.store_path.starts_with("/nix/store/") {
        return Err(anyhow!("the lock's store path is {:?}, not a /nix/store one", lock.store_path));
    }
    Ok(())
}

/// Delete a workspace this stage is done with, best effort.
///
/// Best effort on purpose: teardown's `run-{run_id}` prefix sweep finds every one of these by name
/// anyway. Deleting as we go only keeps the run from holding four workspaces of its own quota
/// while the rest of the stage runs — a leak here is litter, never a failed step.
async fn drop_ws(c: &Ctx, id: &str) {
    let url = api(c, &format!("/v1/workspaces/{id}"));
    if let Err(e) = call(c, reqwest::Method::DELETE, &url, &c.probe_jwt, None).await {
        tracing::warn!(kind = "workspace", op = "delete", name = %id, error = %format!("{e:#}"), "slo.experience.failed");
    }
}

/// Delete a volume this stage registered, best effort. A 409 is expected while the restored
/// workspace's finalizer still runs; `drop_extra_volumes` in teardown is the guarantee.
async fn drop_vol(c: &Ctx, name: &str) {
    let url = api(c, &format!("/v1/volumes/{name}"));
    if let Err(e) = call(c, reqwest::Method::DELETE, &url, &c.probe_jwt, None).await {
        tracing::warn!(kind = "volume", op = "delete", name = %name, error = %format!("{e:#}"), "slo.experience.failed");
    }
}

#[cfg(test)]
mod tests {
    /// The path this probe reads is the one the pod actually mounts. `USER_KEY_PATH` is the mount
    /// and `workspace-token` the Secret key, both from the crate that writes them — so a move
    /// there fails here rather than on the fleet as a 401 nobody expected.
    #[test]
    fn the_token_file_is_the_one_the_pod_mounts() {
        assert_eq!(
            super::WS_TOKEN_FILE,
            format!("{}/workspace-token", kloudlite_workspaces::k8s::USER_KEY_PATH)
        );
    }

    use super::*;
    use crate::testkit;
    use axum::routing::{get, patch, post as apost};

    /// The nine ids this file owns, in the order `experience.rs` calls them.
    const MINE: [&str; 11] = [
        "ws.packages.add",
        "ws.packages.remove",
        "ws.packages.pin",
        "ws.packages.pin.unknown",
        "ws.packages.pin.uncached",
        "ws.packages.update",
        "ws.packages.pin.lockshape",
        "ws.seeded",
        "ws.seed.failed",
        "key.platform.regenerate",
        "home.travels",
    ];

    async fn all(c: &mut Ctx) {
        packages(c).await;
        pin(c).await;
        seeded(c).await;
        seed_failed(c).await;
        platform_key(c).await;
        home_travels(c).await;
    }

    fn once<'a>(c: &'a Ctx, id: &str) -> &'a kloudlite_workspaces::history::slo::StepReport {
        let mut hits = c.steps.iter().filter(|s| s.slo_id == id);
        let s = hits.next().unwrap_or_else(|| panic!("{id} was not reported"));
        assert!(hits.next().is_none(), "{id} was reported twice");
        s
    }

    /// A kubeconfig-less run still reports every id, exactly once, as a skip: the whole stage is
    /// exactly-once complete on every path, and a deployment gap is not an SLO breach.
    #[tokio::test]
    async fn every_id_reports_once_without_a_kubeconfig() {
        let mut c = testkit::ctx().await;
        c.kube = None;
        c.state.repo = Some("run-fast-1".into());
        all(&mut c).await;
        for id in MINE {
            let s = once(&c, id);
            assert!(s.skipped && s.detail == "no kubeconfig", "{s:?}");
        }
        assert_eq!(c.failed(), 0, "a skip is not a failure");
    }

    /// A create that never answers an id is a precondition failure: the FIRST dependent id fails
    /// with the reason and the rest skip — never four failures for one broken thing.
    #[tokio::test]
    async fn a_refused_create_fails_the_first_id_and_skips_the_dependent_ones() {
        let app = axum::Router::new()
            .route("/v1/workspaces", apost(|| async { (axum::http::StatusCode::CONFLICT, "over quota") }))
            .route("/v1/workspaces/{id}", get(|| async { axum::Json(json!({"state": "ready"})) }).patch(patch(|| async { axum::Json(json!({})) })))
            .route("/v1/platform-key", apost(|| async { axum::Json(json!({"fingerprint": "SHA256:x"})) }));
        let mut c = testkit::ctx_against(app).await;
        // A client, not a cluster: nothing here execs, and the point is that the kubeconfig guard
        // is not what produced these reports.
        c.kube = Some(kube::Client::try_from(kube::Config::new("http://127.0.0.1:1".parse().unwrap())).expect("client"));
        c.state.repo = Some("run-fast-1".into());
        all(&mut c).await;
        for id in ["ws.packages.add", "ws.packages.pin", "ws.seeded", "ws.seed.failed", "key.platform.regenerate"] {
            let s = once(&c, id);
            assert!(!s.ok && !s.skipped, "{s:?}");
            assert!(s.detail.contains("409"), "the refusal is in the detail: {s:?}");
        }
        for id in ["ws.packages.remove", "ws.packages.pin.unknown", "ws.packages.pin.uncached", "ws.packages.update", "ws.packages.pin.lockshape", "home.travels"] {
            let s = once(&c, id);
            assert!(s.skipped, "{s:?}");
        }
        // Nothing anywhere carries a credential.
        for s in &c.steps {
            assert!(!s.detail.contains(&c.probe_jwt), "a jwt reached a detail: {s:?}");
        }
    }

    /// The volume outlives both workspaces once pushed and is named by id, so it has to be
    /// registered the moment the create answers — before any exec or push can fail the step.
    #[tokio::test]
    async fn cache_travels_registers_its_volume_before_the_push() {
        let app = axum::Router::new()
            .route("/v1/workspaces", apost(|| async { axum::Json(json!({"id": "ws-1"})) }))
            .route("/v1/workspaces/{id}", get(|| async { axum::Json(json!({"state": "ready"})) }))
            .route("/v1/workspaces/{id}/push", apost(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }));
        let mut c = testkit::ctx_against(app).await;
        c.kube = Some(kube::Client::try_from(kube::Config::new("http://127.0.0.1:1".parse().unwrap())).expect("client"));
        cache_in_tree(&mut c).await;
        assert!(c.state.extra_volumes.contains(&"ws-1".to_string()), "{:?}", c.state.extra_volumes);
        let s = once(&c, "ws.cache.travels");
        assert!(!s.ok && !s.skipped, "{s:?}");
    }

    /// The push carries the run prefix: the message is the only caller-chosen string a detached
    /// volume keeps, and the teardown backstop keys on it.
    #[tokio::test]
    async fn cache_travels_pushes_under_the_run_prefix() {
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<Option<Value>>> = Arc::default();
        let s2 = seen.clone();
        let app = axum::Router::new()
            .route(
                "/v1/workspaces/{id}/push",
                apost(move |axum::Json(b): axum::Json<Value>| {
                    let s2 = s2.clone();
                    async move {
                        *s2.lock().unwrap() = Some(b);
                        axum::Json(json!({"id": "snap-1"}))
                    }
                }),
            )
            .route("/v1/volumes/{v}/history", get(|| async { axum::Json(json!([{"id": "snap-1", "phase": "ready"}])) }))
            .route("/v1/workspaces/restore", apost(|| async { axum::Json(json!({"id": "ws-2"})) }))
            .route("/v1/workspaces/{id}", get(|| async { axum::Json(json!({"state": "ready"})) }));
        let mut c = testkit::ctx_against(app).await;
        let name = format!("{}-cache", c.prefix());
        assert_eq!(push_then_restore(&mut c, "ws-1", "r", &name).await.expect("restored"), "ws-2");
        let body = seen.lock().unwrap().clone().expect("pushed");
        let msg = body.get("message").and_then(Value::as_str).unwrap_or_default();
        assert!(msg.starts_with(&c.prefix()), "{body}");
    }

    /// `ws.seeded` reads the clone from the workspace's own subvolume — `~/workspaces/{name}` —
    /// and compares it to the subject stage 2 pushed. Both halves are literals somewhere else in
    /// `ws.seeded` reads the clone from the workspace's own volume — the whole home now — and
    /// compares it to the subject stage 2 pushed. Both halves are literals somewhere else in
    /// the tree, so this is what catches either one moving.
    #[test]
    fn the_seed_check_reads_the_workspace_directory_and_stage_twos_subject() {
        assert_eq!(kloudlite_workspaces::k8s::WORKSPACE_DIR, "/home/kl/workspace");
        assert_eq!(BASE_BRANCH, "main");
        // `git.push.ok` commits with `-m seed`; if that changes, this test is the reminder.
        let git = include_str!("git.rs");
        assert!(git.contains(&format!(r#""commit".into(), "-q".into(), "-m".into(), "{SEED_SUBJECT}".into()"#)), "stage 2's seed subject moved");
    }
}
