//! Stage 14's workspace-shaped verbs: packages, seeding, the platform key behind seeding, and the
//! home that outlives the workspace that wrote it.
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
use super::{api, call, poll_json, post};
use crate::ctx::{Ctx, PROBE_USER};

/// Per-step ceilings, each at or above its catalogue target so a slow answer is a breach with a
/// number rather than a step the probe cut off. `key.platform.regenerate` and `home.persists` are
/// availability SLOs with no target latency; theirs is a whole seeded create plus an exec, which
/// is the same 180 s the seeded step itself is given, plus room for the second create.
const ADD_CEILING: Duration = Duration::from_secs(150);
const REMOVE_CEILING: Duration = Duration::from_secs(90);
const SEEDED_CEILING: Duration = Duration::from_secs(150);
const KEY_CEILING: Duration = Duration::from_secs(150);
const HOME_CEILING: Duration = Duration::from_secs(150);

/// How long a create is given to reach `ready` INSIDE a step. Below every ceiling above, so a
/// workspace that never starts leaves room for the step to say so.
const READY: Duration = Duration::from_secs(110);

/// One exec's own ceiling. The polls below repeat it, so this bounds a single API-server round
/// trip, not the wait.
const EXEC: Duration = Duration::from_secs(20);

/// The disk each probe workspace asks for, matching stage 5's — well inside `Quota/slo-probe`.
const QUOTA_GB: u64 = 1;

/// The package the two `ws.packages.*` steps add and remove. Small, has a binary of its own name,
/// and is in nixpkgs on every channel — the step measures the profile rebuild, not a build.
const PKG: &str = "cowsay";

/// `ws.packages.add` and `ws.packages.remove`: the workspace they run against is created here and
/// kept for `home.persists`, which writes through it.
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
                // `home.persists` needs it either way.
                c.state.ux_workspace = Some(id.clone());
                set_packages(c, &id, &["bash", PKG]).await?;
                which(c, &id, true, ADD_CEILING).await
            }
            .boxed()
        })
        .await;
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
            let out = clone_subject(c, &id, &name).await;
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
    if c.kube.is_none() {
        return c.skip("key.platform.regenerate", "no kubeconfig");
    }
    let Some(repo) = c.state.repo.clone() else {
        return c.skip("key.platform.regenerate", "the run pushed no repo");
    };
    let name = format!("{}-seed2", c.prefix());
    c.step("key.platform.regenerate", KEY_CEILING, move |c| {
        let url = api(c, &format!("/v1/platform-key?owner={PROBE_USER}"));
        async move {
            // The answer carries the PUBLIC key and its fingerprint only — no private half — so
            // nothing here can reach a step detail. It is dropped regardless: the step's claim is
            // that seeding still works, and the key material is not evidence of that.
            post(c, &url, &c.probe_jwt.clone(), Value::Null)
                .await
                .context("could not regenerate the platform key")?;
            let id = seed(c, &name, &repo).await?;
            let out = clone_subject(c, &id, &name).await;
            drop_ws(c, &id).await;
            out
        }
        .boxed()
    })
    .await;
}

/// `home.persists`: a file written in one workspace's home is read from a FRESH workspace's.
///
/// The last id in the stage, because it asserts something about what everything before it did: the
/// home is region-shared NFS with no `Volume` behind it, so the only proof it persists is a second
/// pod — possibly on a second node — reading what the first wrote.
pub async fn home_persists(c: &mut Ctx) {
    if c.kube.is_none() {
        return c.skip("home.persists", "no kubeconfig");
    }
    let Some(src) = c.state.ux_workspace.clone() else {
        return c.skip("home.persists", "no workspace to write the home from");
    };
    let name = format!("{}-home", c.prefix());
    let want = c.run_id.clone();
    c.step("home.persists", HOME_CEILING, move |c| {
        async move {
            // `sync` before the second pod ever exists: NFS caches, and a read-back that raced the
            // write would fail for a reason that is not the SLO.
            let write = format!("set -e\nprintf %s {want} > {HOME_FILE}\nsync {HOME_FILE}");
            let (code, _, err) = ws_exec(c, &src, &write, EXEC).await?;
            if code != 0 {
                return Err(anyhow!("writing the home file exited {code}: {}", err.trim()));
            }
            let fresh = create(c, &name, json!({ "packages": [] })).await?;
            let (code, out, err) = ws_exec(c, &fresh, &format!("cat {HOME_FILE}"), EXEC).await?;
            // Both workspaces go whatever the read said: a failed step must not leave two pods
            // holding the run's quota for the rest of the stage.
            drop_ws(c, &fresh).await;
            drop_ws(c, &src).await;
            if code != 0 {
                return Err(anyhow!("reading the home file exited {code}: {}", err.trim()));
            }
            if out.trim() != want {
                return Err(anyhow!("the fresh workspace read back {:?}", out.trim()));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// The file the two halves of `home.persists` agree on. Under the shared home, and dot-prefixed so
/// a person who opens the same account's workspace does not find probe litter in their listing.
const HOME_FILE: &str = "/home/kl/.slo-home";

/// Create a workspace and wait for `ready`. Answers its id.
async fn create(c: &Ctx, name: &str, extra: Value) -> Result<String> {
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
    // `owner/name`, never a URL: that is what `/v1` accepts, deliberately (a URL here would be an
    // egress primitive for anyone who can create a workspace).
    let extra = json!({ "repo": format!("{PROBE_USER}/{repo}"), "branch": BASE_BRANCH, "packages": [] });
    create(c, name, extra).await
}

/// The subject of the checked-out clone's last commit, compared to the one this run pushed.
///
/// The path is `k8s::workspace_dir(name)` — the seeder clones into the workspace's own subvolume,
/// which is mounted at `~/workspaces/{name}`, NOT into a directory named after the repo.
async fn clone_subject(c: &Ctx, id: &str, name: &str) -> Result<()> {
    let dir = kloudlite_workspaces::k8s::workspace_dir(name);
    let script = format!("git -C {dir} log -1 --format=%s");
    let (code, out, err) = ws_exec(c, id, &script, EXEC).await?;
    if code != 0 {
        return Err(anyhow!("the clone is not there: exit {code}: {}", err.trim()));
    }
    if out.trim() != SEED_SUBJECT {
        return Err(anyhow!("the checked-out commit is {:?}", out.trim()));
    }
    Ok(())
}

/// `git.push.ok`'s own commit message. Repeated rather than imported because it is a literal in the
/// push step's argv; the test below is what keeps the two from drifting apart.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;
    use axum::routing::{get, patch, post as apost};

    /// The five ids this file owns, in the order `experience.rs` calls them.
    const MINE: [&str; 5] =
        ["ws.packages.add", "ws.packages.remove", "ws.seeded", "key.platform.regenerate", "home.persists"];

    async fn all(c: &mut Ctx) {
        packages(c).await;
        seeded(c).await;
        platform_key(c).await;
        home_persists(c).await;
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
        for id in ["ws.packages.add", "ws.seeded", "key.platform.regenerate"] {
            let s = once(&c, id);
            assert!(!s.ok && !s.skipped, "{s:?}");
            assert!(s.detail.contains("409"), "the refusal is in the detail: {s:?}");
        }
        for id in ["ws.packages.remove", "home.persists"] {
            let s = once(&c, id);
            assert!(s.skipped, "{s:?}");
        }
        // Nothing anywhere carries a credential.
        for s in &c.steps {
            assert!(!s.detail.contains(&c.probe_jwt), "a jwt reached a detail: {s:?}");
        }
    }

    /// `ws.seeded` reads the clone from the workspace's own subvolume — `~/workspaces/{name}` —
    /// and compares it to the subject stage 2 pushed. Both halves are literals somewhere else in
    /// the tree, so this is what catches either one moving.
    #[test]
    fn the_seed_check_reads_the_workspace_directory_and_stage_twos_subject() {
        assert_eq!(
            kloudlite_workspaces::k8s::workspace_dir("run-fast-1-seed"),
            "/home/kl/workspaces/run-fast-1-seed"
        );
        assert_eq!(BASE_BRANCH, "main");
        // `git.push.ok` commits with `-m seed`; if that changes, this test is the reminder.
        let git = include_str!("git.rs");
        assert!(git.contains(&format!(r#""commit".into(), "-q".into(), "-m".into(), "{SEED_SUBJECT}".into()"#)), "stage 2's seed subject moved");
    }
}
