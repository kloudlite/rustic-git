//! Experience probes on pull requests: description, the three merge strategies, mergeability.

use super::*;


/// `repo.description`: saved, and read back off the repo.
///
/// The read-back is the SLI: `update_repo` forwards the description to the OWNING node and answers
/// 204 either way, so a 204 whose text nobody can then read is the failure a person sees as "my
/// change did not stick".
///
/// THIS ID IS EXPECTED RED — the failure is the PRODUCT's, and is deliberately not papered over.
/// `update_repo` (`crates/api/src/repos.rs:381-393`) forwards the text to the node, where
/// `api_description` (`bins/server/src/browse_api/admin.rs:203`) writes `DESCRIPTION_KEY` in the
/// repo's OWN database (`crates/storage/src/refmeta.rs:280-283`) — but `get_repo`
/// (`crates/api/src/repos.rs:284-290`) answers `description` from the listing INDEX marker, which
/// that write never touches. Worse, the reconciler rewrites the marker with
/// `description: String::new()` (`crates/registry/src/gc.rs:270`), so a description a person saves
/// on the settings page can never read back on any path. The probe is correct; the two halves of
/// the product disagree about where a description lives.
pub(crate) async fn description(c: &mut Ctx) {
    let Some(name) = c.state.repo.clone() else {
        return c.skip("repo.description", "no repo");
    };
    let probe = c.probe_user.clone();
    let want = format!("slo probe {}", c.run_id);
    c.step("repo.description", READ_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/repos/{probe}/{name}"));
        async move {
            call(c, reqwest::Method::PATCH, &url, &jwt, Some(json!({ "description": want })))
                .await
                .context("could not save the description")?;
            let seen = get(c, &url, &jwt).await.context("could not read the repo back")?;
            match seen.get("description").and_then(Value::as_str) {
                Some(d) if d == want => Ok(()),
                other => Err(anyhow!("the repo reads back {other:?}, not the description just saved")),
            }
        }
        .boxed()
    })
    .await;
}


/// `pr.merge.strategies`: all four, each on its own change, each judged on the TREE it left.
///
/// Only one strategy was ever probed and `merge_worker.rs` implements four; squash's retry guard
/// (merged-tree == base-tree, not ancestry) and rebase's throwaway worktree are the two paths that
/// have actually broken. Every strategy gets a branch of its own with a file only it writes, so
/// "it landed" is that file being on `main` afterwards — a merge that reported success and moved
/// nothing, or moved the wrong thing, fails here rather than passing on a status.
///
/// One step, first failure wins: "the merge button works" is false if any of the four is broken,
/// and four ids would let one dead strategy sit at 75 % attainment looking mostly healthy.
pub(crate) async fn merge_strategies(c: &mut Ctx) {
    let Some(name) = c.state.repo.clone() else {
        return c.skip("pr.merge.strategies", "no repo");
    };
    let probe = c.probe_user.clone();
    c.step("pr.merge.strategies", MERGE_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let commits = api(c, &format!("/v1/repos/{probe}/{name}/commits"));
        let pulls = api(c, &format!("/v1/repos/{probe}/{name}/pulls"));
        let files = api(c, &format!("/api/{probe}/{name}/refs"));
        let blobs = api(c, &format!("/api/{probe}/{name}/blob"));
        let run = c.run_id.clone();
        async move {
            for strategy in STRATEGIES {
                one_strategy(c, &commits, &pulls, &files, &blobs, &jwt, &run, strategy)
                    .await
                    .with_context(|| format!("the {strategy} strategy"))?;
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}


/// One strategy: a branch with a file of its own, a change, the merge, and the file on `main`.
///
/// The branch is cut by the web's own commit endpoint rather than by git, so this needs no working
/// tree — and a fast-forward is only offered when the branch has not diverged, which is why each
/// runs to completion before the next one starts.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn one_strategy(
    c: &Ctx,
    commits: &str,
    pulls: &str,
    refs: &str,
    blobs: &str,
    jwt: &str,
    run: &str,
    strategy: &str,
) -> Result<()> {
    use base64::Engine as _;
    let branch = format!("run-{run}-{strategy}");
    let path = format!("{strategy}.txt");
    let content = base64::engine::general_purpose::STANDARD.encode(format!("{run} {strategy}\n"));
    let body = json!({
        "branch": BASE_BRANCH,
        "newBranch": branch,
        "message": format!("slo {strategy} {run}"),
        "changes": [{ "path": path, "contentBase64": content }],
    });
    post(c, commits, jwt, body).await.context("could not make the branch")?;
    let opened = post(
        c,
        pulls,
        jwt,
        json!({ "title": format!("slo {strategy} {run}"), "base": BASE_BRANCH, "head": branch }),
    )
    .await
    .context("could not open the change")?;
    let number = opened
        .get("number")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("the answer carried no number"))?;
    post(c, &format!("{pulls}/{number}/merge?strategy={strategy}"), jwt, Value::Null)
        .await
        .context("the merge was refused")?;
    poll_json(c, &format!("{pulls}/{number}"), jwt, Duration::from_secs(60), |p| {
        p.get("state").and_then(Value::as_str) == Some("merged")
    })
    .await
    .context("the change never reached `merged`")?;
    // The TREE, not the status. `merged` is the record the worker wrote; what the SLI promises is
    // that the strategy LANDED the expected tree, and the four strategies build that tree in four
    // different ways (a merge commit, a squashed one, a replayed one, a moved ref). So the file
    // this branch alone wrote is read back off `main` and its CONTENT compared: a merge that
    // answered 202 and moved nothing, or replayed the wrong branch, gets no further than here.
    let want = format!("{run} {strategy}\n");
    landed_on_main(c, refs, blobs, jwt, &path, &want).await
}


/// The file `path` as `main` now has it, compared to what the branch wrote.
///
/// Polled rather than read once: the merge is recorded by the owner and the refs move with it, so
/// a read that raced the ref update would report the base branch's old tree and blame the strategy.
pub(crate) async fn landed_on_main(
    c: &Ctx,
    refs: &str,
    blobs: &str,
    jwt: &str,
    path: &str,
    want: &str,
) -> Result<()> {
    use base64::Engine as _;
    let start = std::time::Instant::now();
    let mut why;
    loop {
        let seen = async {
            let r = get(c, refs, jwt).await.context("could not read the refs")?;
            let head = super::super::git::oid_of(&r, BASE_BRANCH)
                .ok_or_else(|| anyhow!("`{BASE_BRANCH}` has no tip"))?;
            let blob = get(c, &format!("{blobs}/{head}/{path}"), jwt)
                .await
                .with_context(|| format!("`{path}` is not on `{BASE_BRANCH}` at {head}"))?;
            let b64 = blob
                .get("bytes_base64")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("the blob answer carried no bytes"))?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .context("the blob is not base64")?;
            let got = String::from_utf8_lossy(&bytes).to_string();
            if got == want {
                return Ok(());
            }
            Err(anyhow!("`{path}` on `{BASE_BRANCH}` holds {got:?}, not {want:?}"))
        }
        .await;
        match seen {
            Ok(()) => return Ok(()),
            Err(e) => why = format!("{e:#}"),
        }
        if start.elapsed() >= MERGE_LAND {
            return Err(anyhow!("the merge did not land the expected tree: {why}"));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}


/// `pr.mergeability`: the answer the web's merge button is drawn from.
///
/// Both verdicts, because either alone is worthless: a fleet that answered `clean` to everything
/// would offer a merge that fails at the click, and one that answered `dirty` to everything would
/// hide the button on every change. The conflicting branch is built by writing the SAME path from
/// the same base twice, which is the one shape `merge-tree` cannot combine on its own.
pub(crate) async fn mergeability(c: &mut Ctx) {
    let Some(name) = c.state.repo.clone() else {
        return c.skip("pr.mergeability", "no repo");
    };
    let probe = c.probe_user.clone();
    c.step("pr.mergeability", MERGEABILITY_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let commits = api(c, &format!("/v1/repos/{probe}/{name}/commits"));
        let pulls = api(c, &format!("/v1/repos/{probe}/{name}/pulls"));
        let run = c.run_id.clone();
        async move {
            use base64::Engine as _;
            let enc = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
            // A file only this run touches, so nothing else in the journey can make the two agree.
            let path = format!("conflict-{run}.txt");
            // BOTH branches are cut BEFORE either lands, off the same base. That order is the
            // whole experiment: `commit_patch` cuts `newBranch` from `branch`'s CURRENT tip, so a
            // second branch made after the first had merged would start from a `main` that already
            // held the first's line — a clean fast-forward, which is what this reported before.
            let a = format!("run-{run}-clean");
            let b = format!("run-{run}-dirty");
            branch_with(c, &commits, &jwt, &a, &path, &enc("one\n"), &format!("slo clean {run}")).await?;
            branch_with(c, &commits, &jwt, &b, &path, &enc("two\n"), &format!("slo dirty {run}")).await?;
            let clean = open(c, &pulls, &jwt, &a, &format!("slo clean {run}")).await?;
            verdict(c, &pulls, &jwt, clean, "clean").await?;
            post(c, &format!("{pulls}/{clean}/merge?strategy=fast-forward"), &jwt, Value::Null)
                .await
                .context("could not land the clean change")?;
            poll_json(c, &format!("{pulls}/{clean}"), &jwt, Duration::from_secs(60), |p| {
                p.get("state").and_then(Value::as_str) == Some("merged")
            })
            .await
            .context("the clean change never merged, so nothing can conflict with it")?;
            // `b` now writes the same path from a base `main` has moved past — the one shape a
            // trial merge cannot combine on its own.
            let dirty = open(c, &pulls, &jwt, &b, &format!("slo dirty {run}")).await?;
            verdict(c, &pulls, &jwt, dirty, "dirty").await
        }
        .boxed()
    })
    .await;
}


/// A new branch off `main` carrying one file, through the web's own commit endpoint.
pub(crate) async fn branch_with(
    c: &Ctx,
    commits: &str,
    jwt: &str,
    branch: &str,
    path: &str,
    content: &str,
    message: &str,
) -> Result<()> {
    let body = json!({
        "branch": BASE_BRANCH,
        "newBranch": branch,
        "message": message,
        "changes": [{ "path": path, "contentBase64": content }],
    });
    post(c, commits, jwt, body).await.map(|_| ()).with_context(|| format!("could not make {branch}"))
}


pub(crate) async fn open(c: &Ctx, pulls: &str, jwt: &str, branch: &str, title: &str) -> Result<i64> {
    let body = json!({ "title": title, "base": BASE_BRANCH, "head": branch });
    let out = post(c, pulls, jwt, body).await.context("could not open the change")?;
    out.get("number").and_then(Value::as_i64).ok_or_else(|| anyhow!("the answer carried no number"))
}


/// Wait for the owner's own mergeability verdict to be the one asked for.
///
/// `unknown` is "not worked out yet", so the wait is for a REAL verdict and the comparison is what
/// judges it — reading `unknown` as either answer would make this pass on a fleet whose checker
/// never ran at all.
pub(crate) async fn verdict(c: &Ctx, pulls: &str, jwt: &str, number: i64, want: &str) -> Result<()> {
    let url = format!("{pulls}/{number}");
    let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let last = seen.clone();
    poll_json(c, &url, jwt, Duration::from_secs(25), move |p| {
        let state = p.pointer("/mergeability/state").and_then(Value::as_str).unwrap_or("unknown");
        *last.lock().expect("lock") = state.to_string();
        state != "unknown"
    })
    .await
    .with_context(|| format!("change {number}'s mergeability was never worked out"))?;
    let got = seen.lock().expect("lock").clone();
    if got == want {
        return Ok(());
    }
    Err(anyhow!("change {number} reports `{got}`, not `{want}`"))
}

// ── teams and environments ──────────────────────────────────────────────────
