//! Weekly drills on the registry: an abandoned blob session, the body limits, the GC of packs.

use super::*;


/// `reg.blob.session`: the three `/v2` verbs real clients use and nothing probed.
///
/// A chunked upload resumed and finished, a session cancelled, a blob deleted, and `referrers`
/// answered — all of them paths where `Digest::parse` is the only thing between a path segment and
/// an object-store key, which is why they are worth a sample of their own rather than being left
/// to `crane`, which uses none of them.
pub(crate) async fn blob_session(c: &mut Ctx) {
    let Some(secret) = c.state.token_value.clone() else {
        return c.skip("reg.blob.session", "no personal token");
    };
    let probe = c.probe_user.clone();
    let name = format!("{}-sess", c.prefix());
    let dir = c.tmp.join("img-sess");
    let host = super::super::registry::host(c);
    c.step("reg.blob.session", READ_CEILING, move |c| {
        let crane = super::super::registry::authed(c);
        let base = super::super::registry::base(c);
        async move {
            // A real image first: `referrers` answers about a manifest that exists, and the whole
            // session dance below runs in a repository the token already has a scope for.
            let layer = super::super::registry::random_layer();
            let digest = super::super::registry::write_layout(&dir, &layer, &name).context("could not build the image")?;
            crane.login(&host, &probe, &secret).await.context("could not log in")?;
            crane.push(&dir, &format!("{host}/{probe}/{name}:latest")).await.context("could not push it")?;
            let token = super::super::registry::bearer(c, Some(&secret), &format!("repository:{probe}/{name}:pull,push"))
                .await
                .context("could not mint a registry token")?;
            let v2 = format!("{base}/v2/{probe}/{name}");

            // 1 · a chunked upload, in two PATCHes with a status read between them.
            let body: Vec<u8> = (0..2048u32).map(|i| (i % 251) as u8).collect();
            let (half, rest) = body.split_at(1024);
            let want = super::super::registry::sha256(&body);
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
pub(crate) async fn start_upload(c: &Ctx, v2: &str, token: &str) -> Result<String> {
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


pub(crate) async fn patch_chunk(c: &Ctx, session: &str, token: &str, bytes: &[u8], from: usize) -> Result<()> {
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
pub(crate) async fn raw_v2(
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
pub(crate) async fn limits(c: &mut Ctx) {
    let Some(secret) = c.state.token_value.clone() else {
        return c.skip("git.limits", "no personal token");
    };
    let probe = c.probe_user.clone();
    let name = format!("{}-limits", c.prefix());
    let dir = c.tmp.join("img-limits");
    let host = super::super::registry::host(c);
    // `step_cap`, because the blob this id uploads is now deleted in a compensation: the step's
    // own ceiling drops the future, so the body needs a ceiling of its own that fires first or a
    // slow run leaves 5 MiB of blob per week behind (2026-09-12).
    c.step("git.limits", step_cap(READ_CEILING), move |c| {
        let crane = super::super::registry::authed(c);
        let base = super::super::registry::base(c);
        async move {
            let layer = super::super::registry::random_layer();
            super::super::registry::write_layout(&dir, &layer, &name).context("could not build the image")?;
            crane.login(&host, &probe, &secret).await.context("could not log in")?;
            crane.push(&dir, &format!("{host}/{probe}/{name}:latest")).await.context("could not push it")?;
            let token = super::super::registry::bearer(c, Some(&secret), &format!("repository:{probe}/{name}:pull,push"))
                .await
                .context("could not mint a registry token")?;
            let v2 = format!("{base}/v2/{probe}/{name}");
            let digest = super::super::registry::sha256(&vec![b'x'; OVER_MANIFEST]);
            let blob = format!("{v2}/blobs/{digest}");
            let body = async {
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
            let put = format!("{session}{}digest={digest}", if session.contains('?') { "&" } else { "?" });
            let (status, text, _) = raw_v2(c, reqwest::Method::PUT, &put, &token, Some(big)).await?;
            if !status.is_success() {
                return Err(anyhow!(
                    "a {OVER_MANIFEST}-byte blob was refused {status}: the layer limit has been collapsed into the manifest one: {}",
                    text.chars().take(160).collect::<String>()
                ));
            }
            Ok(())
            };
            drill::undoing(READ_CEILING, body, || async {
                raw_v2(c, reqwest::Method::DELETE, &blob, &token, None)
                    .await
                    .map(|_| ())
                    .context("the 5 MiB probe blob was left in the registry")
            })
            .await
        }
        .boxed()
    })
    .await;
}


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
pub(crate) async fn settle<F, Fut>(cap: Duration, what: &str, mut check: F) -> Result<Duration>
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


pub(crate) async fn gc_packs(c: &mut Ctx) {
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
                let g = |a: Vec<String>| super::super::git::git(c, a, Some(&work));
                g(vec!["add".into(), "-A".into()]).await?;
                g(vec!["commit".into(), "-q".into(), "-m".into(), format!("gc {i}")]).await?;
            }
            super::super::git::git(c, super::super::git::authed(c, &["push", "-q", &http, "HEAD:refs/heads/gc"]), Some(&work))
                .await
                .context("the push failed")?;
            let want = super::super::git::git(c, vec!["rev-parse".into(), "HEAD^{tree}".into()], Some(&work)).await?;
            // A whole pass: the lane walks every repo in turn, and a check that raced it would say
            // nothing about the sweep at all.
            tokio::time::sleep(SWEEP_CAP - Duration::from_secs(60)).await;
            let _ = std::fs::remove_dir_all(&dest);
            super::super::git::git(c, super::super::git::authed(c, &["clone", "-q", "--branch", "gc", &http, &dest.display().to_string()]), None)
                .await
                .context("the repo would not clone after a consolidation pass")?;
            let got = super::super::git::git(c, vec!["rev-parse".into(), "HEAD^{tree}".into()], Some(&dest)).await?;
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
