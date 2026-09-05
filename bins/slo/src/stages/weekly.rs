//! Stage 12 · Weekly: the checks too expensive to run every five minutes.
//!
//! Everything here is a thing a person does occasionally and notices immediately when it breaks —
//! pushing a big commit, pushing a big layer, a workspace with a package set nothing has built
//! before, a workspace that comes back on a different node — plus the two control-plane moves
//! (leader failover, a live settings change) that a fast run has no room to wait out.
//!
//! Every id is produced exactly once on every path, like every other stage: a precondition that is
//! gone is a SKIP with the reason, never a silent absence and never a second count of a failure
//! that was already recorded where it happened.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;
use rand::RngCore;
use serde_json::Value;

use super::{admin, api, get, poll_json, post};
use crate::ctx::{Ctx, PROBE_USER};
use crate::{drill, tools};

/// 100 MiB, written a mebibyte at a time so the probe never holds the commit in memory — the pod's
/// limit is 512Mi and the emptyDir it writes into is the budget that matters.
const LARGE_COMMIT_BYTES: u64 = 100 * 1024 * 1024;
const CHUNK: usize = 1024 * 1024;
/// Ten times the fast suite's layer. Big enough that the push is a real transfer through the
/// ingress and Cloudflare rather than a round trip, small enough to fit the CronJob's 4Gi tmp.
const LARGE_LAYER_BYTES: usize = 10 * 1024 * 1024;

/// A package nix will not already have a profile for on most nodes, and tiny when it does build.
/// The point of the id is the COLD path — a hit here would measure the index, which is what
/// `ws.profile.reuse` measures on purpose one step later.
const COLD_PACKAGE: &str = "cowsay";
/// `ws.profile.reuse`'s whole assertion: the second workspace with the same inputs is published
/// from `{PROFILES_DIR}/by-inputs/{hash}` and never invokes nix. A cold evaluation of nixpkgs is
/// ~28 s, so anything under this cannot have been one.
const REUSE_CEILING_MS: u128 = 30_000;

const PUSH_CEILING: Duration = Duration::from_secs(600);
const COLD_CEILING: Duration = Duration::from_secs(600);
const REUSE_CEILING: Duration = Duration::from_secs(120);
const CROSS_CEILING: Duration = Duration::from_secs(300);
const EXEC_CEILING: Duration = Duration::from_secs(30);
/// The catalogue bounds `cp.failover` at 30 s, and that bound IS the wait: a lease that has not
/// moved inside it has failed the SLI whether the probe keeps looking or not.
const FAILOVER_CAP: Duration = Duration::from_secs(30);
/// Same for `settings.live` at 60 s.
const SETTINGS_CAP: Duration = Duration::from_secs(60);

pub async fn run(c: &mut Ctx) {
    large_push(c).await;
    large_layer(c).await;
    let cold = profiles(c).await;
    cross_node(c, cold.as_deref()).await;
    failover(c).await;
    settings_live(c).await;
}

// ── git and registry ────────────────────────────────────────────────────

/// `git.push.large`: a 100 MiB commit, over BOTH protocols.
///
/// Both in one step because they are one SLI — "a big push works" is false if either door is shut,
/// and two ids would let a broken SSH listener sit at 50 % attainment looking half healthy. The
/// same commit goes twice, which is also what makes the second push cheap enough to afford: the
/// objects are already there, so the SSH half measures the protocol and not the bytes again.
async fn large_push(c: &mut Ctx) {
    let Some(name) = c.state.repo.clone() else {
        return c.skip("git.push.large", "no repo");
    };
    // Stage 2's working tree, which already has the remote's history — a fresh clone here would
    // spend the step's budget re-fetching what is on disk.
    let work = c.tmp.join("git").join(&name);
    if !work.is_dir() {
        return c.skip("git.push.large", "stage 2 left no working tree");
    }
    let hosts = match crate::stages::git::known_hosts(c).await {
        Ok(p) => Some(p),
        // The SSH half needs a PINNED host key, and learning one from the host being measured is
        // the substitution `ssh.hostkey` exists to catch. Without a pin the step is the HTTP half
        // alone, which is a weaker measurement than the SLI asks for — so it says so in the log.
        Err(e) => {
            tracing::warn!(reason = "no pinned host key", error = %format!("{e:#}"), "slo.weekly.degraded");
            None
        }
    };
    let http = format!("{}/{PROBE_USER}/{name}.git", c.cfg.git_url.trim_end_matches('/'));
    c.step("git.push.large", PUSH_CEILING, move |c| {
        let key = c.cfg.ssh_key_path.clone();
        let ssh = hosts.as_ref().map(|h| (crate::stages::git::ssh_url(c, &name), crate::stages::git::ssh_command(c, &key, h)));
        let branch = format!("large-{}", c.run_id);
        let args = crate::stages::git::authed(c, &["push", "-q", &http, &branch]);
        let (git, env) = (c.programs.git.clone(), crate::stages::git::git_env(c));
        async move {
            fill(&work.join("large.bin"), LARGE_COMMIT_BYTES).context("could not write the large file")?;
            let g = |a: Vec<String>| crate::stages::git::git(c, a, Some(&work));
            g(vec!["checkout".into(), "-q".into(), "-b".into(), branch.clone()]).await?;
            g(vec!["add".into(), "-A".into()]).await?;
            g(vec!["commit".into(), "-q".into(), "-m".into(), "large".into()]).await?;
            crate::stages::git::git(c, args, Some(&work)).await.context("the HTTP push failed")?;
            let Some((url, cmd)) = ssh else { return Ok(()) };
            let mut env = env;
            env.insert("GIT_SSH_COMMAND".into(), cmd);
            let argv = vec!["push".to_string(), "-q".into(), url, branch];
            tools::run(&git, &argv, &env, Some(&work), PUSH_CEILING)
                .await
                .context("the SSH push failed")?;
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `n` bytes of random content, a mebibyte at a time.
///
/// Random rather than a pattern: git compresses a pack, and a hundred megabytes of zeroes is a
/// push of a few kilobytes wearing a large file's name.
fn fill(path: &std::path::Path, n: u64) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    let mut buf = vec![0u8; CHUNK];
    let mut left = n;
    while left > 0 {
        let take = left.min(CHUNK as u64) as usize;
        rand::thread_rng().fill_bytes(&mut buf[..take]);
        f.write_all(&buf[..take])?;
        left -= take as u64;
    }
    f.flush()?;
    Ok(())
}

/// `reg.push.large`: a 10 MiB layer through the registry ingress, which is a different path from
/// the git one — a different hostname, a different Cloudflare setting and its own body limit
/// (`max_layer`, 5 GiB).
async fn large_layer(c: &mut Ctx) {
    let Some(secret) = c.state.token_value.clone() else {
        return c.skip("reg.push.large", "no personal token");
    };
    let name = format!("{}-large", c.prefix());
    let dir = c.tmp.join("img-large");
    let host = crate::stages::registry::host(c);
    c.step("reg.push.large", PUSH_CEILING, move |c| {
        let crane = crate::stages::registry::authed(c);
        async move {
            let mut layer = vec![0u8; LARGE_LAYER_BYTES];
            rand::thread_rng().fill_bytes(&mut layer);
            crate::stages::registry::write_layout(&dir, &layer, &name).context("could not build the image")?;
            crane.login(&host, PROBE_USER, &secret).await.context("could not log in")?;
            crane.push(&dir, &format!("{host}/{PROBE_USER}/{name}:latest")).await
        }
        .boxed()
    })
    .await;
}

// ── workspaces ──────────────────────────────────────────────────────────

/// `ws.cold.profile` and `ws.profile.reuse`: one package set nix has never seen, then the same set
/// again.
///
/// The second one is the whole point of the pair: a profile is keyed by
/// `packages::hash(pin, base + spec.packages)` and indexed per node, so a repeat set must be
/// PUBLISHED from that index rather than evaluated again. The two ids are only meaningful together
/// — a cold build that failed leaves nothing to reuse, so `ws.profile.reuse` skips rather than
/// reporting a second failure for the same broken thing.
///
/// Answers the cold workspace's id, which `ws.cross.node` then moves.
async fn profiles(c: &mut Ctx) -> Option<String> {
    let id = Arc::new(Mutex::new(None::<String>));
    let cold = {
        let (name, seen) = (format!("{}-cold", c.prefix()), id.clone());
        c.step("ws.cold.profile", COLD_CEILING, move |c| {
            async move {
                let made = create(c, &name, COLD_CEILING).await?;
                *seen.lock().expect("lock") = Some(made);
                Ok(())
            }
            .boxed()
        })
        .await
    };
    let id = id.lock().expect("lock").clone();
    if !cold {
        c.skip("ws.profile.reuse", "the cold profile never built");
        return id;
    }
    let name = format!("{}-warm", c.prefix());
    c.step("ws.profile.reuse", REUSE_CEILING, move |c| {
        async move {
            let at = Instant::now();
            create(c, &name, REUSE_CEILING).await?;
            let ms = at.elapsed().as_millis();
            if ms > REUSE_CEILING_MS {
                // Not a slow fleet: a repeat package set that took this long was rebuilt, which is
                // the index having missed — the one failure this id exists to catch.
                return Err(anyhow!("the repeat package set took {ms} ms, so it was rebuilt"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
    id
}

/// One workspace with the cold package set, waited to `ready`. Answers its id.
async fn create(c: &Ctx, name: &str, cap: Duration) -> Result<String> {
    let body = serde_json::json!({
        "name": name,
        "region": c.cfg.region,
        "quota_gb": 1,
        "packages": [COLD_PACKAGE],
    });
    let jwt = c.probe_jwt.clone();
    let doc = post(c, &api(c, "/v1/workspaces"), &jwt, body).await.context("could not create it")?;
    let id = doc
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("the create answered no workspace id"))?
        .to_string();
    let url = api(c, &format!("/v1/workspaces/{id}"));
    poll_json(c, &url, &jwt, cap, |v| v.get("state").and_then(Value::as_str) == Some("ready"))
        .await
        .context("it never became ready")?;
    Ok(id)
}

/// `ws.cross.node` and `homes.cross.node`: the workspace comes back on a DIFFERENT node, and the
/// home it finds there is the same home.
///
/// The cordon is what forces the move — a stopped workspace may start on any node that is up to
/// date for its worktree, and the owner is the one it would otherwise pick — and it is undone on
/// every path out of the step, including a start that never converges (`drill::with_cordon`). The
/// home read is a second id rather than a second assertion inside the first because the two fail
/// for completely different reasons: a workspace that would not move is placement, a home that
/// reads back wrong is the NFS export.
async fn cross_node(c: &mut Ctx, ws: Option<&str>) {
    let (Some(ws), Some(k)) = (ws, c.kube.clone()) else {
        let why = if ws.is_none() { "no cold workspace" } else { "no kubeconfig" };
        c.skip("ws.cross.node", why);
        c.skip("homes.cross.node", why);
        return;
    };
    let owner = match placement(c, ws).await {
        Some(n) => n,
        None => {
            c.skip("ws.cross.node", "the workspace names no node");
            c.skip("homes.cross.node", "the workspace names no node");
            return;
        }
    };
    let moved = {
        let (ws, owner) = (ws.to_string(), owner.clone());
        c.step("ws.cross.node", CROSS_CEILING, move |c| {
            let jwt = c.probe_jwt.clone();
            let (stop, start) = (
                api(c, &format!("/v1/workspaces/{ws}/stop")),
                api(c, &format!("/v1/workspaces/{ws}/start")),
            );
            let doc = api(c, &format!("/v1/workspaces/{ws}"));
            async move {
                post(c, &stop, &jwt, Value::Null).await.context("could not stop it")?;
                poll_json(c, &doc, &jwt, CROSS_CEILING, |v| {
                    v.get("state").and_then(Value::as_str) == Some("stopped")
                })
                .await
                .context("it never stopped")?;
                let body = async {
                    post(c, &start, &jwt, Value::Null).await.context("could not start it")?;
                    // Ready AND elsewhere, in one predicate: a workspace that came back on the
                    // cordoned node is this SLI failing, not a slow start.
                    poll_json(c, &doc, &jwt, CROSS_CEILING, |v| {
                        v.get("state").and_then(Value::as_str) == Some("ready")
                            && v.get("placement").and_then(Value::as_str).is_some_and(|n| n != owner)
                    })
                    .await
                    .with_context(|| format!("it did not come back ready on a node other than {owner}"))
                };
                drill::with_cordon(&k, &owner, body).await
            }
            .boxed()
        })
        .await
    };
    if !moved {
        // The home read only means anything from the peer node; on the owner it would pass through
        // the one failure this id is about.
        return c.skip("homes.cross.node", "the workspace never moved");
    }
    let (ws, want) = (ws.to_string(), c.run_id.clone());
    c.step("homes.cross.node", EXEC_CEILING, move |c| {
        async move {
            // Written by stage 5's `homes.rw.p95` on the OWNER node, on the region-shared export.
            let (code, out, err) =
                crate::stages::workspace::ws_exec(c, &ws, "cat /home/kl/.slo", EXEC_CEILING).await?;
            if code != 0 {
                return Err(anyhow!("the home read exited {code}: {}", err.trim()));
            }
            if out.trim() != want {
                return Err(anyhow!("the peer node's home holds someone else's bytes"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// The node a workspace is on, or `None` while nothing has claimed it.
async fn placement(c: &Ctx, ws: &str) -> Option<String> {
    let url = api(c, &format!("/v1/workspaces/{ws}"));
    get(c, &url, &c.probe_jwt)
        .await
        .ok()?
        .get("placement")
        .and_then(Value::as_str)
        .map(str::to_string)
}

// ── control plane ───────────────────────────────────────────────────────

/// `cp.failover`: delete the pod holding the ownership lease, and watch another one take it.
///
/// The client here is built EXPLICITLY in-cluster rather than taken from `Ctx::kube`: that one
/// follows `KUBECONFIG` and lands in k3s, where the server tier does not run at all — without this
/// the drill would delete a workspace node's pod and then wait out a lease that never moved.
async fn failover(c: &mut Ctx) {
    let k = match kube::Config::incluster().map_err(|e| anyhow!("{e}")).and_then(|cfg| {
        kube::Client::try_from(cfg).map_err(|e| anyhow!("{e}"))
    }) {
        Ok(k) => k,
        // Not running in a cluster is a deployment gap, not a failover that did not happen.
        Err(e) => return c.skip("cp.failover", &format!("no in-cluster client: {e:#}")),
    };
    c.step("cp.failover", FAILOVER_CAP + Duration::from_secs(30), move |c| {
        let jwt = c.admin_jwt.clone();
        let url = admin(c, "/admin/slo/pipeline");
        async move {
            let was = leader(c, &url, &jwt).await.context("nothing reports holding the lease")?;
            let pods: kube::Api<k8s_openapi::api::core::v1::Pod> =
                kube::Api::namespaced(k.clone(), "kloudlite-git");
            pods.delete(&was, &kube::api::DeleteParams::default())
                .await
                .map_err(|e| anyhow!("could not delete the leader: {e}"))?;
            // The lease TTL is 10 s and the tick 3 s, so a healthy fleet re-elects well inside the
            // catalogue's 30 s; `leader_pod` is read off `ownership_is_leader == 1`, which is the
            // pod's own claim rather than anything the probe inferred.
            poll_json(c, &url, &jwt, FAILOVER_CAP, |v| {
                v.get("leader_pod").and_then(Value::as_str).is_some_and(|p| p != was)
            })
            .await
            .with_context(|| format!("the lease was still on {was}"))
        }
        .boxed()
    })
    .await;
}

async fn leader(c: &Ctx, url: &str, jwt: &str) -> Result<String> {
    get(c, url, jwt)
        .await?
        .get("leader_pod")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("the pipeline route named no leader"))
}

/// `settings.live`: a `Mark::Live` field changed through the admin API is readable back within one
/// beat, and the probe puts it back.
///
/// `uploadGraceSecs` deliberately: it is `Mark::Live` (so nothing rolls), it is a registry
/// upload-session grace measured in HOURS, and a minute's difference either way changes nothing
/// any client can observe. Every other knob is either a limit somebody is up against or an address
/// something dials. The revert is in the undo path, so an admin process that answered the write
/// and then stopped answering still leaves the fleet on its own value.
async fn settings_live(c: &mut Ctx) {
    c.step("settings.live", SETTINGS_CAP + Duration::from_secs(30), |c| {
        let jwt = c.admin_jwt.clone();
        let url = admin(c, "/admin/settings/central");
        async move {
            let doc = get(c, &url, &jwt).await.context("could not read the settings")?;
            // Absent means "nothing stored, the compiled default is in force" — restoring THAT
            // value is the closest a write-only API gets to putting the document back.
            let was = doc.get("uploadGraceSecs").and_then(Value::as_u64).unwrap_or(DEFAULT_GRACE);
            let to = if was + STEP <= GRACE_MAX { was + STEP } else { was - STEP };
            put(c, &url, &jwt, to).await.context("the settings write was refused")?;
            let body = poll_json(c, &url, &jwt, SETTINGS_CAP, |v| {
                v.get("uploadGraceSecs").and_then(Value::as_u64) == Some(to)
            })
            .await
            .context("the change never read back");
            let back = put(c, &url, &jwt, was).await.context("the settings change was NOT reverted");
            body?;
            back
        }
        .boxed()
    })
    .await;
}

async fn put(c: &Ctx, url: &str, jwt: &str, v: u64) -> Result<()> {
    let body = serde_json::json!({ "uploadGraceSecs": v });
    super::call(c, reqwest::Method::PUT, url, jwt, Some(body)).await.map(|_| ())
}

/// `crates/core/src/settings.rs`: the compiled default, and the top of the range `validate_stored`
/// enforces. Repeated rather than imported for the range — importing `validate_stored` would let
/// the probe write a value it thinks is legal and be told otherwise, which is the same 422 either
/// way; the numbers are here so the step's arithmetic is readable next to it.
const DEFAULT_GRACE: u64 = 86_400;
const GRACE_MAX: u64 = 604_800;
/// A minute: large enough to be a real change, small enough that a fleet running under it for the
/// length of a step behaves identically.
const STEP: u64 = 60;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every id exactly once, whatever the fleet is doing. With no repo, no token, no workspace
    /// and nothing reachable, this stage still owes the console eight rows — a stage that dropped
    /// ids when its preconditions were gone would make a broken run look like a short one.
    #[tokio::test]
    async fn weekly_produces_every_id_once() {
        let mut c = crate::testkit::ctx().await;
        c.kube = None;
        tokio::time::pause();
        run(&mut c).await;
        let ids: Vec<&str> = c.steps.iter().map(|s| s.slo_id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "git.push.large",
                "reg.push.large",
                "ws.cold.profile",
                "ws.profile.reuse",
                "ws.cross.node",
                "homes.cross.node",
                "cp.failover",
                "settings.live",
            ]
        );
        // A missing precondition is a skip, never a second count of a failure recorded elsewhere.
        for id in ["git.push.large", "reg.push.large", "ws.profile.reuse", "homes.cross.node"] {
            let s = c.steps.iter().find(|s| s.slo_id == id).expect(id);
            assert!(s.skipped, "{s:?}");
        }
    }

    /// The bytes have to be incompressible: git packs a commit, and a hundred megabytes of zeroes
    /// is a push of a few kilobytes wearing a large file's name.
    #[test]
    fn the_large_file_is_the_size_it_claims_and_is_not_compressible() {
        let path = std::env::temp_dir().join(format!("slo-large-{}", std::process::id()));
        fill(&path, 3 * CHUNK as u64 + 7).expect("write");
        let bytes = std::fs::read(&path).expect("read");
        assert_eq!(bytes.len(), 3 * CHUNK + 7);
        // Two chunks that happened to be equal would mean the RNG was never asked twice.
        assert_ne!(bytes[..CHUNK], bytes[CHUNK..2 * CHUNK]);
        let _ = std::fs::remove_file(&path);
    }
}
