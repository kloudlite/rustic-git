//! Stage 4 · Registry: an OCI round trip through the same front door `docker push` uses.
//!
//! The two images are built HERE, in process, as OCI layout directories — no daemon, no
//! `docker build`, no base image pulled from anywhere. That is what makes `reg.shared.layer`
//! possible at all: the probe knows the exact digest of the layer it put in both images, so the
//! check is "image A still pulls that blob after image B was deleted" rather than a guess about
//! byte counts.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;
use rand::RngCore;

use super::{api, poll_json, post};
use crate::crane::Crane;
use crate::ctx::{Ctx, PROBE_USER};
/// Per-step ceilings. Each is well above its catalogue target — a slow answer must be a BREACH
/// with a number, not a step the probe cut off — and the sum is under the fast suite's own budget
/// so a wedged registry cannot cost the later stages their samples.
const TOKEN_CEILING: Duration = Duration::from_secs(20);
const PUSH_CEILING: Duration = Duration::from_secs(90);
const MANIFEST_CEILING: Duration = Duration::from_secs(20);
const TAGS_CEILING: Duration = Duration::from_secs(15);
const SHARED_CEILING: Duration = Duration::from_secs(90);
const VISIBILITY_CEILING: Duration = Duration::from_secs(60);
const CANARY_CEILING: Duration = Duration::from_secs(30);

/// `reg.tags.visible` is bounded at 5 s by the catalogue, and that bound IS the wait: a tag that
/// took longer has failed the SLI whether the probe keeps looking or not.
const TAGS_CAP: Duration = Duration::from_secs(5);

/// Big enough that a push is a real transfer over the ingress rather than a round trip, small
/// enough that the fast suite still fits its deadline. `reg.push.large` is the weekly one.
const LAYER_BYTES: usize = 1024 * 1024;

/// Every id this stage owns, in journey order. A precondition that fails skips the tail of this
/// list with one reason — the failure was already counted where it happened.
const AFTER_PUSH: [&str; 4] =
    ["reg.manifest.p95", "reg.tags.visible", "reg.shared.layer", "reg.visibility"];

pub async fn run(c: &mut Ctx) {
    // Every id here needs the personal token, the canary included: `slo-probe/canary` is PRIVATE
    // (bootstrap pushes it and never makes it public), so pulling it is an authenticated pull.
    let Some(secret) = c.state.token_value.clone() else {
        c.skip("reg.token.p95", "no personal token");
        c.skip("reg.push.ok", "no personal token");
        for id in AFTER_PUSH {
            c.skip(id, "no personal token");
        }
        c.skip("reg.canary", "no personal token");
        return;
    };

    let (a, b) = (format!("{}-a", c.prefix()), format!("{}-b", c.prefix()));
    token(c, &secret, &a).await;

    match push(c, &secret, &a).await {
        Ok(layer) => {
            manifest(c, &secret, &a).await;
            tags(c, &secret, &a).await;
            shared_layer(c, &secret, &a, &b, &layer).await;
            visibility(c, &a).await;
        }
        Err(e) => {
            let why = format!("nothing was pushed: {e:#}");
            for id in AFTER_PUSH {
                c.skip(id, &why);
            }
        }
    }
    canary(c, &secret).await;
}

/// `reg.token.p95`: the `/v2/token` exchange every spec-following client makes before it pulls.
async fn token(c: &mut Ctx, secret: &str, image: &str) {
    c.step("reg.token.p95", TOKEN_CEILING, |c| {
        let (secret, scope) = (secret.to_string(), pull_scope(image));
        async move {
            bearer(c, Some(&secret), &scope).await.map(|_| ())
        }
        .boxed()
    })
    .await;
}

/// `reg.push.ok`: image A, and the layer digest the shared-layer check is built on.
///
/// The layouts are built INSIDE the step, not before it. A tmp tree that cannot be written is not
/// the registry's fault, but it is still `reg.push.ok` that did not happen — dropping the id
/// entirely would report a green run with no push in it. Image B is built here and pushed by
/// `reg.shared.layer`, whose whole assertion is about what B's push does NOT re-upload.
async fn push(c: &mut Ctx, secret: &str, a: &str) -> Result<String> {
    let layer = random_layer();
    let digest = sha256(&layer);
    let host = host(c);
    let (dir_a, dir_b) = (c.tmp.join("img-a"), c.tmp.join("img-b"));
    let ok = {
        let (secret, a, b) = (secret.to_string(), a.to_string(), format!("{}-b", c.prefix()));
        c.step("reg.push.ok", PUSH_CEILING, move |c| {
            let crane = authed(c);
            async move {
                write_layout(&dir_a, &layer, &a).context("could not build image a")?;
                write_layout(&dir_b, &layer, &b).context("could not build image b")?;
                crane.login(&host, PROBE_USER, &secret).await.context("could not log in")?;
                crane.push(&dir_a, &format!("{host}/{PROBE_USER}/{a}:latest")).await
            }
            .boxed()
        })
        .await
    };
    if !ok {
        return Err(anyhow!("the push failed"));
    }
    Ok(digest)
}

/// `reg.manifest.p95`: the manifest GET itself, and nothing else.
///
/// The bearer is fetched BEFORE the step: `reg.token.p95` is the token call's own SLI, and folding
/// it into this sample would make one slow token look like a slow manifest read.
async fn manifest(c: &mut Ctx, secret: &str, image: &str) {
    let scope = pull_scope(image);
    let bearer = match bearer(c, Some(secret), &scope).await {
        Ok(t) => t,
        Err(e) => return c.skip("reg.manifest.p95", &format!("no registry token: {e:#}")),
    };
    let url = format!("{}/v2/{PROBE_USER}/{image}/manifests/latest", base(c));
    c.step("reg.manifest.p95", MANIFEST_CEILING, move |c| {
        async move {
            let (status, body) = super::raw(
                c,
                reqwest::Method::GET,
                &url,
                &bearer,
                None,
                // Without this the registry may answer a v2 manifest the client did not ask for;
                // a real pull always states which media types it can read.
                &[("accept", MANIFEST_ACCEPT.to_string())],
            )
            .await?;
            if !status.is_success() {
                return Err(anyhow!("{status}: {}", body.chars().take(200).collect::<String>()));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `reg.tags.visible`: the tag in the image's own tag list, and the image in the catalogue.
///
/// Both, sharing ONE 5 s budget: the SLI is how long after a push the fleet agrees the image is
/// there, and two separate 5 s waits would report 10 s as a pass.
async fn tags(c: &mut Ctx, secret: &str, image: &str) {
    let bearer = match bearer(c, Some(secret), &format!("{} registry:catalog:*", pull_scope(image)))
        .await
    {
        Ok(t) => t,
        Err(e) => return c.skip("reg.tags.visible", &format!("no registry token: {e:#}")),
    };
    let tags_url = format!("{}/v2/{PROBE_USER}/{image}/tags/list", base(c));
    let catalog_url = format!("{}/v2/_catalog", base(c));
    let want = format!("{PROBE_USER}/{image}");
    c.step("reg.tags.visible", TAGS_CEILING, move |c| {
        async move {
            let start = Instant::now();
            poll_json(c, &tags_url, &bearer, TAGS_CAP, |v| has(v, "tags", "latest"))
                .await
                .context("the tag never appeared")?;
            let left = TAGS_CAP.saturating_sub(start.elapsed());
            poll_json(c, &catalog_url, &bearer, left, |v| has(v, "repositories", &want))
                .await
                .context("the image never appeared in the catalogue")
        }
        .boxed()
    })
    .await;
}

/// `reg.shared.layer`: the sibling does not re-upload the layer, and deleting the sibling does not
/// take it away.
///
/// Two facts, one step, first failure wins — because either one alone is worthless. `HEAD` on B's
/// blob endpoint BEFORE B is pushed is the "not re-uploaded" half: blobs are per-owner
/// (`blobs/{owner}/{algo}/{hex}`), so a 200 there means B's push will mount the layer rather than
/// send a megabyte again. The delete-and-pull half is the one that catches the worst registry bug
/// there is — a manifest path deleting a blob a sibling still references (`crates/registry/src/gc.rs`'s
/// "only two things ever delete a blob").
async fn shared_layer(c: &mut Ctx, secret: &str, a: &str, b: &str, layer: &str) {
    let bearer = match bearer(c, Some(secret), &pull_scope(b)).await {
        Ok(t) => t,
        Err(e) => return c.skip("reg.shared.layer", &format!("no registry token: {e:#}")),
    };
    let dest = c.tmp.join("pull-a");
    let dir_b = c.tmp.join("img-b");
    let head = format!("{}/v2/{PROBE_USER}/{b}/blobs/{layer}", base(c));
    let (host, a, b, layer) = (host(c), a.to_string(), b.to_string(), layer.to_string());
    c.step("reg.shared.layer", SHARED_CEILING, move |c| {
        let crane = authed(c);
        let (jwt, del) = (c.probe_jwt.clone(), api(c, &format!("/api/{PROBE_USER}/{b}/imagedelete")));
        async move {
            let (status, _) =
                super::raw(c, reqwest::Method::HEAD, &head, &bearer, None, &[]).await?;
            if !status.is_success() {
                return Err(anyhow!("the layer image a pushed is not there for b to mount: {status}"));
            }
            crane.push(&dir_b, &format!("{host}/{PROBE_USER}/{b}:latest")).await.context("could not push the sibling")?;
            post(c, &del, &jwt, serde_json::Value::Null).await.context("could not delete the sibling")?;
            let _ = std::fs::remove_dir_all(&dest);
            crane.pull(&format!("{host}/{PROBE_USER}/{a}:latest"), &dest).await.context("could not pull")?;
            let got = std::fs::read(dest.join("blobs/sha256").join(layer.trim_start_matches("sha256:")))
                .context("the shared layer is not in the pulled image")?;
            if sha256(&got) != layer {
                return Err(anyhow!("the shared layer came back with different bytes"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `reg.visibility`: private refuses an anonymous pull, public admits one.
///
/// Both halves, in one step and in that order: a registry that answered everything would pass the
/// public half alone, and a registry that answered nothing would pass the private half alone.
async fn visibility(c: &mut Ctx, image: &str) {
    let host = host(c);
    let dest = c.tmp.join("pull-anon");
    c.step("reg.visibility", VISIBILITY_CEILING, move |c| {
        let anon = anonymous(c);
        let (jwt, image) = (c.probe_jwt.clone(), image.to_string());
        let flip = api(c, &format!("/api/{PROBE_USER}/{image}/imagevisibility?visibility=public"));
        let reference = format!("{host}/{PROBE_USER}/{image}:latest");
        async move {
            let _ = std::fs::remove_dir_all(&dest);
            if anon.pull(&reference, &dest).await.is_ok() {
                return Err(anyhow!("a private image pulled anonymously"));
            }
            post(c, &flip, &jwt, serde_json::Value::Null).await.context("could not make it public")?;
            let _ = std::fs::remove_dir_all(&dest);
            anon.pull(&reference, &dest).await.context("a public image refused an anonymous pull")?;
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `reg.canary`: the long-lived image `bootstrap` pushed still pulls, and is still the same image.
///
/// AUTHENTICATED: the canary is private, like every other image this probe owns, so the pull needs
/// the personal token. The digest is PINNED in the environment rather than read from the registry,
/// for the same reason `ssh.hostkey` is pinned: a probe that trusted whatever the registry answered
/// would report green through exactly the substitution this exists to catch. Unset means no pin,
/// and the step skips.
async fn canary(c: &mut Ctx, secret: &str) {
    let Some(want) = c.cfg.canary_digest.clone() else {
        return c.skip("reg.canary", "KLOUDLITE_SLO_CANARY_DIGEST is not set");
    };
    let host = host(c);
    let reference = format!("{host}/{PROBE_USER}/canary:latest");
    let secret = secret.to_string();
    c.step("reg.canary", CANARY_CEILING, move |c| {
        let crane = authed(c);
        async move {
            // Repeated rather than assumed: a canary that only passed after `reg.push.ok` had
            // logged in would go red for the wrong reason the day the push fails.
            crane.login(&host, PROBE_USER, &secret).await.context("could not log in")?;
            let got = crane.digest(&reference).await?;
            if got != want {
                return Err(anyhow!("the canary is {got}, not the pinned {want}"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `bootstrap`: push `slo-probe/canary` if it is not there, and answer its digest for a human to
/// pin into `KLOUDLITE_SLO_CANARY_DIGEST`.
///
/// Its layer is FIXED bytes, not random: bootstrap re-runs on every deploy, and a canary whose
/// digest moved would fail `reg.canary` on every probe until somebody re-pinned it.
pub async fn ensure_canary(c: &Ctx) -> Result<String> {
    let host = host(c);
    let reference = format!("{host}/{PROBE_USER}/canary:latest");
    let crane = authed(c);
    // A token of its own, minted and revoked here: the registry's Basic auth takes a personal
    // token and nothing else (`registry::auth::caller`), and bootstrap runs before any run has
    // one. Leaving it behind would be a standing credential nobody owns. It is minted before the
    // digest probe rather than after, because the canary is private — an unauthenticated `crane
    // digest` fails whether the image is there or not, and bootstrap would re-push every deploy.
    let minted = post(
        c,
        &api(c, "/v1/tokens"),
        &c.probe_jwt.clone(),
        serde_json::json!({ "owner": PROBE_USER, "name": "slo-bootstrap-canary" }),
    )
    .await
    .context("could not mint a registry credential")?;
    let secret = minted
        .get("token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("the answer carried no token"))?
        .to_string();
    let out = push_canary(c, &crane, &host, &reference, &secret).await;
    match minted.get("_id").and_then(|v| v.as_str()) {
        Some(id) => {
            let url = api(c, &format!("/v1/tokens/{id}"));
            // Best effort, and never allowed to mask the push's own outcome.
            if let Err(e) = super::call(c, reqwest::Method::DELETE, &url, &c.probe_jwt.clone(), None).await {
                tracing::warn!(op = "revoke", error = %format!("{e:#}"), "slo.bootstrap.failed");
            }
        }
        // Nothing to revoke it BY: a live credential is now unowned, and that must be visible.
        None => tracing::error!(reason = "token id missing", "slo.bootstrap.failed"),
    }
    out
}

/// Log in, then push only if what is there is not already this exact image. The layer is FIXED
/// bytes, so the digest a fresh push would produce is knowable without pushing: a re-run on every
/// deploy must not move the digest a human pinned into the CronJob.
async fn push_canary(
    c: &Ctx,
    crane: &Crane,
    host: &str,
    reference: &str,
    secret: &str,
) -> Result<String> {
    crane.login(host, PROBE_USER, secret).await.context("could not log in")?;
    let dir = c.tmp.join("canary");
    std::fs::create_dir_all(&c.tmp)?;
    let want = write_layout(&dir, &vec![0x5c; LAYER_BYTES], "canary")?;
    if crane.digest(reference).await.ok().as_deref() == Some(want.as_str()) {
        return Ok(want);
    }
    crane.push(&dir, reference).await.context("could not push the canary")?;
    crane.digest(reference).await
}

const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json,application/vnd.oci.image.index.v1+json,application/vnd.docker.distribution.manifest.v2+json,application/vnd.docker.distribution.manifest.list.v2+json";

/// `{registry}` as an HTTP base. The deployment sets a bare host, because that is what a `docker
/// pull` line carries; a value with a scheme is accepted so a test can point it at a stub.
fn base(c: &Ctx) -> String {
    let r = c.cfg.registry.trim_end_matches('/');
    if r.starts_with("http://") || r.starts_with("https://") {
        r.to_string()
    } else {
        format!("https://{r}")
    }
}

/// The same value as an image-reference prefix: `crane` takes a host, never a URL.
pub(crate) fn host(c: &Ctx) -> String {
    base(c).trim_start_matches("https://").trim_start_matches("http://").to_string()
}

fn pull_scope(image: &str) -> String {
    format!("repository:{PROBE_USER}/{image}:pull,push")
}

pub(crate) fn authed(c: &Ctx) -> Crane {
    Crane::new(&c.programs.crane, c.tmp.join("docker"))
}

/// A `crane` with its own empty config directory — the whole of what makes `reg.visibility`'s
/// first pull anonymous. A flag could be forgotten; an empty directory holds no credential.
fn anonymous(c: &Ctx) -> Crane {
    Crane::new(&c.programs.crane, c.tmp.join("docker-anon"))
}

/// A registry bearer for `scope`. `secret: None` asks anonymously, which the registry answers with
/// a token for nobody — the one a public pull uses.
async fn bearer(c: &Ctx, secret: Option<&str>, scope: &str) -> Result<String> {
    use base64::Engine;
    let url = format!("{}/v2/token?service={}&scope={}", base(c), host(c), urlencoding(scope));
    let mut req = c.http.get(&url);
    if let Some(s) = secret {
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("{PROBE_USER}:{s}"));
        req = req.header("authorization", format!("Basic {basic}"));
    }
    // `without_url`: the URL is not a secret here, but the rule is the module's, not the caller's.
    let r = req.send().await.map_err(|e| anyhow!("{}", e.without_url()))?;
    let status = r.status();
    let body = r.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("{status}: {}", body.chars().take(200).collect::<String>()));
    }
    serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("token").and_then(|t| t.as_str()).map(str::to_string))
        .filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow!("the token endpoint answered no token"))
}

/// Enough encoding for a scope: only `:` and `,` and `*` appear in one, and reqwest will not
/// escape them for us. A dependency for three characters would be the wrong trade.
fn urlencoding(s: &str) -> String {
    s.chars()
        .map(|ch| match ch {
            ':' => "%3A".to_string(),
            ',' => "%2C".to_string(),
            '*' => "%2A".to_string(),
            ' ' => "+".to_string(),
            other => other.to_string(),
        })
        .collect()
}

/// Whether `field` (an array of strings) holds `want`.
fn has(v: &serde_json::Value, field: &str, want: &str) -> bool {
    v.get(field)
        .and_then(|f| f.as_array())
        .is_some_and(|rows| rows.iter().any(|r| r.as_str() == Some(want)))
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    format!("sha256:{:x}", sha2::Sha256::digest(bytes))
}

/// One layer's worth of bytes nothing else has. Random rather than a pattern so a registry that
/// deduplicated by content across runs could not make `reg.push.ok` measure nothing.
fn random_layer() -> Vec<u8> {
    let mut buf = vec![0u8; LAYER_BYTES];
    rand::thread_rng().fill_bytes(&mut buf);
    buf
}

/// Write a minimal OCI layout at `dir`: the shared layer, a config naming this image, a manifest
/// and an index tagged `latest`. `crane push` reads exactly this.
///
/// The layer is raw bytes with the uncompressed `…tar` media type rather than a real tar: nothing
/// in the journey ever unpacks it, and building a tar to satisfy a reader that does not exist is
/// code that can only rot.
pub(crate) fn write_layout(dir: &Path, layer: &[u8], image: &str) -> Result<String> {
    let blobs = dir.join("blobs/sha256");
    std::fs::create_dir_all(&blobs)?;
    let layer_digest = blob(&blobs, layer)?;
    // The image name is in the config, so the two images differ in every blob but the layer —
    // which is exactly the shape `reg.shared.layer` needs.
    let config = serde_json::json!({
        "architecture": "amd64",
        "os": "linux",
        "config": { "Labels": { "io.kloudlite.slo": image } },
        "rootfs": { "type": "layers", "diff_ids": [layer_digest] },
    })
    .to_string();
    let config_digest = blob(&blobs, config.as_bytes())?;
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config.len(),
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar",
            "digest": layer_digest,
            "size": layer.len(),
        }],
    })
    .to_string();
    let manifest_digest = blob(&blobs, manifest.as_bytes())?;
    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": manifest_digest,
            "size": manifest.len(),
            "annotations": { "org.opencontainers.image.ref.name": "latest" },
        }],
    });
    write(&dir.join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#)?;
    write(&dir.join("index.json"), index.to_string().as_bytes())?;
    Ok(manifest_digest)
}

/// Write `bytes` under its own digest and answer that digest.
fn blob(blobs: &Path, bytes: &[u8]) -> Result<String> {
    let d = sha256(bytes);
    write(&blobs.join(d.trim_start_matches("sha256:")), bytes)?;
    Ok(d)
}

fn write(path: &PathBuf, bytes: &[u8]) -> Result<()> {
    let mut f = std::fs::File::create(path)
        .with_context(|| format!("could not create {}", path.display()))?;
    f.write_all(bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer_of(dir: &Path) -> (String, String) {
        let index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("index.json")).expect("index")).unwrap();
        let m = index["manifests"][0]["digest"].as_str().expect("manifest digest");
        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.join("blobs/sha256").join(m.trim_start_matches("sha256:"))).unwrap(),
        )
        .unwrap();
        (
            manifest["layers"][0]["digest"].as_str().unwrap().to_string(),
            manifest["config"]["digest"].as_str().unwrap().to_string(),
        )
    }

    #[test]
    fn shared_layer_check_builds_two_images_from_one_layer() {
        let root = std::env::temp_dir().join(format!("slo-layout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let layer = vec![7u8; 4096];
        write_layout(&root.join("a"), &layer, "run-x-a").expect("a");
        write_layout(&root.join("b"), &layer, "run-x-b").expect("b");
        let (layer_a, config_a) = layer_of(&root.join("a"));
        let (layer_b, config_b) = layer_of(&root.join("b"));
        // The whole point: one blob, two images.
        assert_eq!(layer_a, layer_b);
        assert_eq!(layer_a, sha256(&layer));
        // And they are genuinely two images, not the same one pushed twice.
        assert_ne!(config_a, config_b);
        // The layer really is on disk under its digest, which is what `crane push` uploads.
        let path = root.join("a/blobs/sha256").join(layer_a.trim_start_matches("sha256:"));
        assert_eq!(std::fs::read(path).expect("layer blob").len(), layer.len());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A tmp tree that cannot be written is not the registry's fault, but it is still
    /// `reg.push.ok` that did not happen — and a dropped id reports a green run with no push in
    /// it. The four ids the push is a precondition for are skipped, never counted twice.
    #[tokio::test]
    async fn a_layout_that_cannot_be_written_fails_the_push_id_rather_than_vanishing() {
        let mut c = crate::testkit::ctx().await;
        // A file where the stage wants a directory: every `create_dir_all` under it fails.
        let blocked = std::env::temp_dir().join(format!("slo-blocked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&blocked);
        std::fs::write(&blocked, b"not a directory").expect("blocker");
        c.tmp = blocked.join("tmp");
        c.state.token_value = Some("t".into());
        run(&mut c).await;
        let push = c.steps.iter().find(|s| s.slo_id == "reg.push.ok").expect("reg.push.ok");
        assert!(!push.ok && !push.skipped, "{push:?}");
        assert!(push.detail.contains("could not build image a"), "{}", push.detail);
        for id in AFTER_PUSH {
            let s = c.steps.iter().find(|s| s.slo_id == id).unwrap_or_else(|| panic!("{id}"));
            assert!(s.skipped, "{id} should be skipped, not counted twice");
        }
        let _ = std::fs::remove_file(&blocked);
    }

    #[test]
    fn a_scope_survives_the_query_string() {
        assert_eq!(urlencoding("repository:a/b:pull,push"), "repository%3Aa/b%3Apull%2Cpush");
        assert_eq!(urlencoding("registry:catalog:*"), "registry%3Acatalog%3A%2A");
    }

    /// The deployment sets a bare host, because that is what a `docker pull` line carries, and the
    /// two consumers need opposite halves of it: `crane` a host, `reqwest` a URL.
    #[tokio::test]
    async fn a_bare_registry_host_is_a_url_for_http_and_a_host_for_crane() {
        let mut c = crate::testkit::ctx().await;
        c.cfg.registry = "cr.example.com".into();
        assert_eq!(base(&c), "https://cr.example.com");
        assert_eq!(host(&c), "cr.example.com");
        // A scheme is accepted so a test can point the stage at a stub, and never reaches crane.
        c.cfg.registry = "http://127.0.0.1:8080/".into();
        assert_eq!(base(&c), "http://127.0.0.1:8080");
        assert_eq!(host(&c), "127.0.0.1:8080");
    }
}
