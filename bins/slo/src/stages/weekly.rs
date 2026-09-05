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
use crate::ctx::Ctx;
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
/// The cross-node body: a stop that converges and a start that converges, one after the other,
/// each given half. The STEP gets a minute on top (`step_cap`) so `Ctx::step`'s own timeout can
/// never fire first and drop the uncordon with it.
const CROSS_BODY: Duration = Duration::from_secs(300);
const CROSS_POLL: Duration = Duration::from_secs(150);
const EXEC_CEILING: Duration = Duration::from_secs(30);
/// The catalogue bounds `cp.failover` at 30 s, and that bound IS the wait: a lease that has not
/// moved inside it has failed the SLI whether the probe keeps looking or not.
const FAILOVER_CAP: Duration = Duration::from_secs(30);
/// Same for `settings.live` at 60 s.
const SETTINGS_CAP: Duration = Duration::from_secs(60);
/// `settings.revert` waits TWICE — for its own change, then for the revert — inside one step whose
/// ceiling is `step_cap(SETTINGS_CAP)` = 120 s. Half each keeps the body plus its undo under that,
/// so a slow settings beat is a verdict rather than the step's own timeout swallowing it.
const REVERT_HALF: Duration = Duration::from_secs(SETTINGS_CAP.as_secs() / 2);
/// How long the agent DaemonSet gets to finish the roll `settings.roll` starts, and to be seen
/// mid-roll before it does. A DaemonSet across a handful of nodes, not a deployment of one.
const ROLL_CAP: Duration = Duration::from_secs(240);

/// A step's ceiling for a body that has an undo: always the body's own plus a minute. `Ctx::step`
/// times out by DROPPING the step's future, so an outer timeout that fired first would take the
/// undo with it — the drill's cap has to be the one that wins.
fn step_cap(body: Duration) -> Duration {
    body + Duration::from_secs(60)
}

pub async fn run(c: &mut Ctx) {
    large_push(c).await;
    large_layer(c).await;
    let cold = profiles(c).await;
    cross_node(c, cold.as_deref()).await;
    env_cross_node(c).await;
    failover(c).await;
    settings_live(c).await;
    settings_revert(c).await;
    settings_roll(c).await;
    gc_sweep(c).await;
}

/// `settings.revert`: the undo beside `settings.live`'s save.
///
/// Every save keeps the last ten versions, and revert is what a person reaches for when one of
/// them was wrong — which makes it the half nobody exercises until the moment it has to work. The
/// step writes a value it can recognise, reverts, and reads the OLD one back: a revert that
/// answered 2xx and restored nothing is the whole failure, and it looks identical on the wire.
///
/// The same knob `settings.live` moves, and for the same reason: `uploadGraceSecs` is `Mark::Live`
/// (so nothing rolls), it is measured in hours, and a minute either way changes nothing a client
/// can observe.
async fn settings_revert(c: &mut Ctx) {
    c.step("settings.revert", step_cap(SETTINGS_CAP), |c| {
        // Every wait inside this step shares REVERT_HALF, not SETTINGS_CAP: there are TWO polls
        // (the change landing, then the old value coming back) plus the reads between them, and
        // two full caps plus the undo would overrun the step's own ceiling and report a timeout
        // where the fleet had given a verdict.
        let jwt = c.admin_jwt.clone();
        let url = admin(c, "/admin/settings/central");
        let revert = admin(c, "/admin/settings/central/revert");
        async move {
            let doc = get(c, &url, &jwt).await.context("could not read the settings")?;
            let was = doc.get("uploadGraceSecs").and_then(Value::as_u64).unwrap_or(DEFAULT_GRACE);
            // Two steps away from where it started, so the revert restoring `was` cannot be
            // confused with a revert that restored nothing at all.
            let to = if was + STEP * 2 <= GRACE_MAX { was + STEP * 2 } else { was - STEP * 2 };
            put(c, &url, &jwt, to).await.context("the settings write was refused")?;
            // The revert is also the compensation: it is what puts `was` back, so it runs outside
            // the cancellable region and a read-back that never converges cannot cost the fleet
            // its own value.
            let back = || async {
                let body = serde_json::json!({ "note": "slo probe settings revert" });
                match super::call(c, reqwest::Method::POST, &revert, &jwt, Some(body)).await {
                    Ok(_) => Ok(()),
                    // The revert is what should have restored it; a direct write is the fallback
                    // so a broken revert route never leaves the fleet on the probe's value.
                    Err(e) => {
                        let _ = put(c, &url, &jwt, was).await;
                        Err(e).context("the revert was refused")
                    }
                }
            };
            let seen = async {
                poll_json(c, &url, &jwt, REVERT_HALF, |v| {
                    v.get("uploadGraceSecs").and_then(Value::as_u64) == Some(to)
                })
                .await
                .context("the change the revert is meant to undo never landed")
            };
            drill::undoing(REVERT_HALF, seen, back).await?;
            // The point of the whole step: the STORED value is the one from before.
            poll_json(c, &url, &jwt, REVERT_HALF, |v| {
                v.get("uploadGraceSecs").and_then(Value::as_u64) == Some(was)
            })
            .await
            .context("the revert answered but the old value did not come back")
        }
        .boxed()
    })
    .await;
}

/// `settings.roll`: a `Mark::Boot` save is refused with 409 while one of its readers is
/// mid-rollout, and nothing is written.
///
/// Two things the review turned up, and both move this off the central scope entirely. First,
/// there is no `Mark::Boot` field in `CENTRAL_SETTING_META` at all — every central knob is `Live`
/// — so a Boot precheck could never fire on `PUT /admin/settings/central` and an id pointed there
/// was measuring a liveness check wearing the roll id. The Boot fields are the agent's three
/// (`crd::CLUSTER_SETTING_META`: `defaultImage`, `gitInitImage`, `runtimeClass`), so the guard
/// lives on `PUT /admin/settings/clusters/{region}`.
///
/// Second, accepting "2xx or 409" proved nothing: delete `precheck_readers` and the id stays
/// green. So the path is made DETERMINISTIC — the probe puts the reader mid-rollout itself and
/// then requires the refusal:
///
/// 1. wait until `kloudlite-agent` is `ready == desired`, so the roll below is this step's;
/// 2. roll it through the console's own route, which is the button an operator has;
/// 3. WHILE it is rolling, a Boot save must answer 409 — and the stored value must be unchanged
///    afterwards, which is the "nothing is written" half a status alone cannot say;
/// 4. wait for it to settle, so the next weekly run starts from a ready fleet.
///
/// It changes no configuration: the Boot save it attempts is REFUSED by design, and the roll is a
/// restart of a stateless controller that the settings machinery performs on its own whenever
/// anyone saves one of those three fields.
async fn settings_roll(c: &mut Ctx) {
    let region = c.cfg.region.clone();
    c.step("settings.roll", step_cap(ROLL_CAP), move |c| {
        let jwt = c.admin_jwt.clone();
        let workloads = admin(c, "/admin/workloads");
        let roll = admin(c, &format!("/admin/workloads/{region}/{AGENT}/roll"));
        let settings = admin(c, &format!("/admin/settings/clusters/{region}"));
        async move {
            // A fleet that is already mid-rollout is somebody else's roll, and the 409 below would
            // be theirs rather than this step's — a skip-shaped precondition, not a breach.
            settled(c, &workloads, &jwt, ROLL_CAP / 4)
                .await
                .context("the agent was not ready to begin with, so this step's roll is not its own")?;
            let before = get(c, &settings, &jwt).await.context("could not read the cluster settings")?;
            let field = boot_field_of(&before)
                .ok_or_else(|| anyhow!("the cluster settings carry none of the Boot fields"))?;
            let held = before.pointer(&format!("/spec/{field}")).cloned().unwrap_or(Value::Null);
            let body = serde_json::json!({ "reason": SETTINGS_NOTE });
            post(c, &roll, &jwt, body).await.context("the roll was refused")?;
            // The roll is a real fleet action, so it is waited out on EVERY path — including the
            // one where the refusal below never comes. `undoing` is what guarantees that.
            let settle = || async {
                settled(c, &workloads, &jwt, ROLL_CAP)
                    .await
                    .context("the agent was left mid-rollout")
            };
            let refused = async {
                // Mid-rollout: the roll has just been asked for, so the reader is below desired
                // until it comes back. Catching that window is what makes the save refusable.
                mid_rollout(c, &workloads, &jwt, ROLL_CAP / 4).await?;
                let save = serde_json::json!({ &field: boot_value(&held), "note": SETTINGS_NOTE });
                let (status, text) =
                    super::raw(c, reqwest::Method::PUT, &settings, &jwt, Some(save), &[]).await?;
                if status.as_u16() != 409 {
                    return Err(anyhow!(
                        "a Boot save of `{field}` during a rollout answered {status}, not 409: {}",
                        text.chars().take(200).collect::<String>()
                    ));
                }
                // "Nothing is written" — the half the status cannot say, and the one that makes
                // the precheck worth having: a 409 AFTER the document was persisted would leave
                // the settings describing a fleet that never read them.
                let after = get(c, &settings, &jwt).await.context("could not re-read the cluster settings")?;
                let now = after.pointer(&format!("/spec/{field}")).cloned().unwrap_or(Value::Null);
                if now != held {
                    return Err(anyhow!("the refused save still changed `{field}` from {held} to {now}"));
                }
                Ok(())
            };
            drill::undoing(ROLL_CAP, refused, settle).await
        }
        .boxed()
    })
    .await;
}

/// The DaemonSet every Boot field in `crd::CLUSTER_SETTING_META` names as its reader.
const AGENT: &str = "kloudlite-agent";

/// The three cluster-scoped Boot fields, in the order `CLUSTER_SETTING_META` lists them. Repeated
/// rather than imported: what this step needs is a field the SAVE will carry, which is a fact
/// about the wire shape, and the test below is what holds the two lists together.
const BOOT_FIELDS: [&str; 3] = ["defaultImage", "gitInitImage", "runtimeClass"];

/// The first Boot field the region's settings actually carry.
fn boot_field_of(doc: &Value) -> Option<String> {
    BOOT_FIELDS
        .iter()
        .find(|f| doc.pointer(&format!("/spec/{f}")).is_some())
        .map(|f| (*f).to_string())
}

/// A value for the refused save. Deliberately DIFFERENT from what is stored — a save of the same
/// value changes no Boot field, computes an empty roll set and is never prechecked at all, which
/// is the shape that made this id unfalsifiable in the first place. It is never written: the whole
/// step is the assertion that the precheck refuses it.
fn boot_value(held: &Value) -> Value {
    match held.as_str() {
        Some(s) => Value::String(format!("{s}-slo-probe-never-written")),
        None => Value::String("slo-probe-never-written".into()),
    }
}

/// Wait until `kloudlite-agent` reports `ready == desired` in every region it is listed for.
async fn settled(c: &Ctx, url: &str, jwt: &str, cap: Duration) -> Result<()> {
    agent_rows_until(c, url, jwt, cap, "settle", |ready, desired| ready >= desired).await
}

/// Wait until it reports `ready < desired` — the window the precheck refuses a save in.
async fn mid_rollout(c: &Ctx, url: &str, jwt: &str, cap: Duration) -> Result<()> {
    agent_rows_until(c, url, jwt, cap, "start rolling", |ready, desired| ready < desired).await
}

/// Poll `/admin/workloads` until every agent row satisfies `want`.
///
/// A listing that FAILS is not an answer: reading an error as "the condition holds" would let this
/// step pass through an admin process that stopped answering, which is the failure mode the whole
/// id exists inside of.
async fn agent_rows_until(
    c: &Ctx,
    url: &str,
    jwt: &str,
    cap: Duration,
    what: &str,
    want: impl Fn(i64, i64) -> bool,
) -> Result<()> {
    let start = Instant::now();
    let mut seen;
    loop {
        let doc = get(c, url, jwt).await.context("could not read the workloads")?;
        let rows = doc.get("workloads").and_then(Value::as_array).or_else(|| doc.as_array()).cloned().unwrap_or_default();
        let agents: Vec<&Value> =
            rows.iter().filter(|r| r.get("name").and_then(Value::as_str) == Some(AGENT)).collect();
        if agents.is_empty() {
            return Err(anyhow!("`{AGENT}` is not listed as a roll target at all"));
        }
        let n = |r: &Value, k: &str| r.get(k).and_then(Value::as_i64).unwrap_or(0);
        if agents.iter().all(|r| want(n(r, "ready"), n(r, "desired"))) {
            return Ok(());
        }
        seen = agents.iter().map(|r| format!("{}/{}", n(r, "ready"), n(r, "desired"))).collect::<Vec<_>>().join(" ");
        if start.elapsed() >= cap {
            return Err(anyhow!("`{AGENT}` did not {what} after {} ms: ready/desired {seen}", cap.as_millis()));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// `reg.gc.sweep`: a blob a sibling still references survives that image's deletion and a GC pass.
///
/// Only the KEEP-BIASED half, deliberately. `BLOB_GRACE` is a fixed hour and the weekly CronJob's
/// own `activeDeadlineSeconds` is 3600, so no run can watch an unreferenced blob actually be
/// reclaimed — waiting one out is a thing this probe cannot do in band, and inventing a shorter
/// grace for it would change the system to suit the measurement. What it CAN prove is the rule
/// `gc.rs` is written around and the one whose failure loses somebody's image: siblings share
/// layers, so a sweep that took a referenced blob is the worst bug the registry has.
///
/// `reg.shared.layer` in the fast suite makes the same two images and deletes one — but it pulls
/// straight afterwards, before any sweep has run. This one waits out a full GC pass first, which is
/// the difference between "the delete path did not take it" and "the sweep did not take it either".
async fn gc_sweep(c: &mut Ctx) {
    let probe = c.probe_user.clone();
    let Some(secret) = c.state.token_value.clone() else {
        return c.skip("reg.gc.sweep", "no personal token");
    };
    let (a, b) = (format!("{}-gca", c.prefix()), format!("{}-gcb", c.prefix()));
    let (dir_a, dir_b) = (c.tmp.join("img-gca"), c.tmp.join("img-gcb"));
    let host = crate::stages::registry::host(c);
    c.step("reg.gc.sweep", step_cap(GC_PASS + Duration::from_secs(120)), move |c| {
        let crane = crate::stages::registry::authed(c);
        let jwt = c.probe_jwt.clone();
        let del = api(c, &format!("/api/{probe}/{b}/imagedelete"));
        let dest = c.tmp.join("pull-gca");
        async move {
            let mut layer = vec![0u8; 64 * 1024];
            rand::thread_rng().fill_bytes(&mut layer);
            let digest = sha256(&layer);
            crate::stages::registry::write_layout(&dir_a, &layer, &a).context("could not build image a")?;
            crate::stages::registry::write_layout(&dir_b, &layer, &b).context("could not build image b")?;
            crane.login(&host, &probe, &secret).await.context("could not log in")?;
            crane.push(&dir_a, &format!("{host}/{probe}/{a}:latest")).await.context("could not push a")?;
            crane.push(&dir_b, &format!("{host}/{probe}/{b}:latest")).await.context("could not push b")?;
            post(c, &del, &jwt, Value::Null).await.context("could not delete the sibling")?;
            // A whole pass, so the sweep has certainly visited this owner: `gc_lane` walks every
            // owner with a gap between them, and a check that raced it would be measuring the
            // delete path all over again.
            tokio::time::sleep(GC_PASS).await;
            let _ = std::fs::remove_dir_all(&dest);
            crane
                .pull(&format!("{host}/{probe}/{a}:latest"), &dest)
                .await
                .context("the surviving image would not pull after the sweep")?;
            let got = std::fs::read(dest.join("blobs/sha256").join(digest.trim_start_matches("sha256:")))
                .context("the sweep took a layer the surviving image still references")?;
            if sha256(&got) != digest {
                return Err(anyhow!("the shared layer came back with different bytes"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// Long enough that `gc_lane` has certainly swept this owner — it walks every owner in turn with a
/// gap between them — and short enough that the weekly run still fits its hour.
const GC_PASS: Duration = Duration::from_secs(180);

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    format!("sha256:{:x}", sha2::Sha256::digest(bytes))
}

// ── git and registry ────────────────────────────────────────────────────

/// `git.push.large`: a 100 MiB commit, over BOTH protocols.
///
/// Both in one step because they are one SLI — "a big push works" is false if either door is shut,
/// and two ids would let a broken SSH listener sit at 50 % attainment looking half healthy. The
/// same commit goes twice, which is also what makes the second push cheap enough to afford: the
/// objects are already there, so the SSH half measures the protocol and not the bytes again.
async fn large_push(c: &mut Ctx) {
    let probe = c.probe_user.clone();
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
    let http = format!("{}/{probe}/{name}.git", c.cfg.git_url.trim_end_matches('/'));
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
    let probe = c.probe_user.clone();
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
            crane.login(&host, &probe, &secret).await.context("could not log in")?;
            crane.push(&dir, &format!("{host}/{probe}/{name}:latest")).await
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
        c.step("ws.cross.node", step_cap(CROSS_BODY), move |c| {
            let (jwt, tmp) = (c.probe_jwt.clone(), c.tmp.clone());
            let (stop, start) = (
                api(c, &format!("/v1/workspaces/{ws}/stop")),
                api(c, &format!("/v1/workspaces/{ws}/start")),
            );
            let doc = api(c, &format!("/v1/workspaces/{ws}"));
            async move {
                post(c, &stop, &jwt, Value::Null).await.context("could not stop it")?;
                poll_json(c, &doc, &jwt, CROSS_POLL, |v| {
                    v.get("state").and_then(Value::as_str) == Some("stopped")
                })
                .await
                .context("it never stopped")?;
                let body = async {
                    post(c, &start, &jwt, Value::Null).await.context("could not start it")?;
                    // Ready AND elsewhere, in one predicate: a workspace that came back on the
                    // cordoned node is this SLI failing, not a slow start.
                    poll_json(c, &doc, &jwt, CROSS_POLL, |v| {
                        v.get("state").and_then(Value::as_str) == Some("ready")
                            && v.get("placement").and_then(Value::as_str).is_some_and(|n| n != owner)
                    })
                    .await
                    .with_context(|| format!("it did not come back ready on a node other than {owner}"))
                };
                drill::with_cordon(&k, &tmp, &owner, CROSS_BODY, body).await
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

/// `env.cross.node`: `ws.cross.node`'s twin — the fast journey's environment comes back on a
/// DIFFERENT node, with its service running there off the replica that node holds.
///
/// The exec is the "reads its replica correctly" half: an environment that reports `running` on a
/// peer node whose subvolume never arrived would have no pod to answer it. Same cordon-with-undo
/// as the workspace's, for the same reason — the undo must outlive a start that never converges.
async fn env_cross_node(c: &mut Ctx) {
    let (Some(env), Some(k)) = (c.state.environment.clone(), c.kube.clone()) else {
        let why = if c.kube.is_none() { "no kubeconfig" } else { "no environment" };
        return c.skip("env.cross.node", why);
    };
    let Some(owner) = env_placement(c, &env).await else {
        return c.skip("env.cross.node", "the environment names no node");
    };
    c.step("env.cross.node", step_cap(CROSS_BODY), move |c| {
        let (jwt, tmp) = (c.probe_jwt.clone(), c.tmp.clone());
        let (stop, start) = (
            api(c, &format!("/v1/environments/{env}/stop")),
            api(c, &format!("/v1/environments/{env}/start")),
        );
        let doc = api(c, &format!("/v1/environments/{env}"));
        async move {
            post(c, &stop, &jwt, Value::Null).await.context("could not stop it")?;
            poll_json(c, &doc, &jwt, CROSS_POLL, |v| {
                v.get("state").and_then(Value::as_str) == Some("stopped")
            })
            .await
            .context("it never stopped")?;
            let body = async {
                post(c, &start, &jwt, Value::Null).await.context("could not start it")?;
                poll_json(c, &doc, &jwt, CROSS_POLL, |v| {
                    v.get("state").and_then(Value::as_str) == Some("running")
                        && v.get("placement").and_then(Value::as_str).is_some_and(|n| n != owner)
                })
                .await
                .with_context(|| format!("it did not come back running on a node other than {owner}"))?;
                let ns = kloudlite_workspaces::crd::env_namespace(&env);
                let k = c.kube.as_ref().ok_or_else(|| anyhow!("no kubeconfig"))?;
                // `redis-0`: stage 6's one service, one StatefulSet, one replica.
                let (code, out, err) =
                    crate::kube::exec(k, &ns, "redis-0", None, &["sh", "-c", "echo slo"], EXEC_CEILING).await?;
                if code != 0 || out.trim() != "slo" {
                    return Err(anyhow!("the service on the peer node exited {code}: {}", err.trim()));
                }
                Ok(())
            };
            drill::with_cordon(&k, &tmp, &owner, CROSS_BODY, body).await
        }
        .boxed()
    })
    .await;
}

/// The node an environment is on, or `None` while nothing has claimed it.
async fn env_placement(c: &Ctx, env: &str) -> Option<String> {
    let url = api(c, &format!("/v1/environments/{env}"));
    get(c, &url, &c.probe_jwt).await.ok()?.get("placement").and_then(Value::as_str).map(str::to_string)
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
    let k = match drill::incluster() {
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
                kube::Api::namespaced(k.clone(), "kloudlite");
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
    c.step("settings.live", step_cap(SETTINGS_CAP), |c| {
        let jwt = c.admin_jwt.clone();
        let url = admin(c, "/admin/settings/central");
        async move {
            let doc = get(c, &url, &jwt).await.context("could not read the settings")?;
            // Absent means "nothing stored, the compiled default is in force" — restoring THAT
            // value is the closest a write-only API gets to putting the document back.
            let was = doc.get("uploadGraceSecs").and_then(Value::as_u64).unwrap_or(DEFAULT_GRACE);
            let to = if was + STEP <= GRACE_MAX { was + STEP } else { was - STEP };
            put(c, &url, &jwt, to).await.context("the settings write was refused")?;
            // The same undo mechanism the drills use, for the same reason: the revert must survive
            // a read-back that never converges, and `Ctx::step`'s timeout would drop it.
            let read = poll_json(c, &url, &jwt, SETTINGS_CAP, |v| {
                v.get("uploadGraceSecs").and_then(Value::as_u64) == Some(to)
            });
            drill::undoing(SETTINGS_CAP, async { read.await.context("the change never read back") }, || async {
                put(c, &url, &jwt, was).await.context("the settings change was NOT reverted")
            })
            .await
        }
        .boxed()
    })
    .await;
}

/// One settings write. The `note` is NOT optional: `put_central` calls `require_note` on the
/// body before it validates anything else, so a write without one is a 422 — a probe bug that
/// would have failed `settings.live`, `settings.revert` and `settings.roll` on every run and read
/// as the fleet refusing the save.
async fn put(c: &Ctx, url: &str, jwt: &str, v: u64) -> Result<()> {
    let body = serde_json::json!({ "uploadGraceSecs": v, "note": SETTINGS_NOTE });
    super::call(c, reqwest::Method::PUT, url, jwt, Some(body)).await.map(|_| ())
}

/// The reason every settings write this probe makes carries onto its audit row. One constant: a
/// human reading the log should see the same sentence whichever of the three ids wrote it.
const SETTINGS_NOTE: &str = "slo probe settings check";

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
    /// and nothing reachable, this stage still owes the console nine rows — a stage that dropped
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
                "env.cross.node",
                "cp.failover",
                "settings.live",
                "settings.revert",
                "settings.roll",
                "reg.gc.sweep",
            ]
        );
        // A missing precondition is a skip, never a second count of a failure recorded elsewhere.
        for id in ["git.push.large", "reg.push.large", "ws.profile.reuse", "homes.cross.node", "reg.gc.sweep"] {
            let s = c.steps.iter().find(|s| s.slo_id == id).expect(id);
            assert!(s.skipped, "{s:?}");
        }
    }

    /// `BOOT_FIELDS` is the wire half of `crd::CLUSTER_SETTING_META`'s Boot entries; a field that
    /// changed mark there without changing here would leave `settings.roll` saving a `Live` knob
    /// and calling a missing 409 a breach.
    #[test]
    fn the_boot_fields_are_the_crds_boot_fields() {
        let want: Vec<&str> = kloudlite_workspaces::crd::CLUSTER_SETTING_META
            .iter()
            .filter(|(_, mark, _)| matches!(mark, kloudlite_core::settings::Mark::Boot))
            .map(|(name, _, _)| *name)
            .collect();
        assert_eq!(BOOT_FIELDS.to_vec(), want);
        // And every one of them names the DaemonSet this step rolls.
        for (name, _, readers) in kloudlite_workspaces::crd::CLUSTER_SETTING_META {
            if BOOT_FIELDS.contains(name) {
                assert!(readers.contains(&AGENT), "{name} does not name {AGENT}");
            }
        }
    }

    /// The refused save must carry a DIFFERENT value: a Boot save of the stored value changes no
    /// field, computes an empty roll set and is never prechecked — the shape that made this id
    /// impossible to fail.
    #[test]
    fn the_refused_save_is_never_the_value_already_stored() {
        let held = Value::String("ghcr.io/kloudlite/workspace:v1".into());
        assert_ne!(boot_value(&held), held);
        assert_ne!(boot_value(&Value::Null), Value::Null);
        // And the first Boot field the document actually carries is the one that is tried.
        let doc = serde_json::json!({ "spec": { "gitInitImage": "x", "runtimeClass": "gvisor" } });
        assert_eq!(boot_field_of(&doc).as_deref(), Some("gitInitImage"));
        assert!(boot_field_of(&serde_json::json!({ "spec": { "nodeDeadSecs": 180 } })).is_none());
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
