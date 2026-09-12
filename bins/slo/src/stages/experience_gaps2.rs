//! The 2026-09-06 coverage review's Experience-stage ids.
//!
//! A second file beside `experience_gaps` for the reason that one exists: the stage is one
//! catalogue and several hands, and a 1400-line stage file is one nobody can merge. Everything
//! here follows the same rules — one sample per id on every path, a missing precondition is a SKIP
//! with its reason, and nothing is left behind that teardown's prefix sweep cannot find.

use std::time::Duration;

use anyhow::{anyhow, Context};
use futures::FutureExt;
use serde_json::{json, Value};

use super::{admin, api, get, post, raw};
use crate::ctx::Ctx;
use crate::tools;

const READ_CEILING: Duration = Duration::from_secs(20);
const KUBE_CEILING: Duration = Duration::from_secs(30);
const PAGES_CEILING: Duration = Duration::from_secs(60);
const CLI_CEILING: Duration = Duration::from_secs(30);

/// `ws.quota.namespace`: the per-namespace `ResourceQuota` is really there, and really says what
/// the owner's `Quota` says.
///
/// `guard_alloc` is read-then-write and can overshoot by one, which is precisely why the namespace
/// quota is documented as THE hard stop for cpu and memory — and nothing proved it existed, let
/// alone that it matched. Read against `GET /v1/quota`'s own effective numbers, so a `Quota` raised
/// by a request and never projected into the namespace fails here rather than silently handing out
/// allocation Kubernetes will refuse later.
///
/// The refusal half — a pod over the ceiling being rejected — is deliberately NOT probed: the
/// probe has no `pods: create` grant in a workspace namespace and giving it one to prove a
/// Kubernetes primitive works would be a bigger hole than the id is worth. What is asserted
/// instead is that Kubernetes is TRACKING the ceiling (`status.hard` and `status.used` are both
/// filled in), which is the state in which it does the refusing.
pub(super) async fn quota_namespace(c: &mut Ctx) {
    let Some(k) = c.kube.clone() else { return c.skip("ws.quota.namespace", "no kubeconfig") };
    let ns = kloudlite_workspaces::crd::ws_namespace(&c.probe_user, "");
    c.step("ws.quota.namespace", KUBE_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, "/v1/quota");
        async move {
            use k8s_openapi::api::core::v1::ResourceQuota;
            let doc = get(c, &url, &jwt).await.context("could not read the quota")?;
            let limit = |dim: &str| doc.pointer(&format!("/limit/{dim}")).and_then(Value::as_u64);
            let (cpu, mem) = (
                limit("cpu").ok_or_else(|| anyhow!("the quota answer carries no cpu limit"))?,
                limit("memoryGb").ok_or_else(|| anyhow!("the quota answer carries no memoryGb limit"))?,
            );
            let api: kube::Api<ResourceQuota> = kube::Api::namespaced(k.clone(), &ns);
            let q = api
                .get_opt("owner-quota")
                .await
                .map_err(|e| anyhow!("could not read {ns}/owner-quota: {e}"))?
                .ok_or_else(|| anyhow!("{ns} carries no `owner-quota`: nothing bounds cpu or memory there"))?;
            let hard = q
                .status
                .as_ref()
                .and_then(|s| s.hard.clone())
                .ok_or_else(|| anyhow!("Kubernetes has not tracked `owner-quota` yet: it bounds nothing until it does"))?;
            let used = q.status.as_ref().and_then(|s| s.used.clone()).unwrap_or_default();
            // Parsed, never compared as strings: the API server normalizes a Quantity, so `4`
            // comes back as `4`, `4000m` or `4` depending on what was written, and `8Gi` may read
            // as `8589934592`. A string comparison failed a namespace quota that was correct.
            let want = [("limits.cpu", millis(&format!("{cpu}")), "cpu"), ("limits.memory", bytes(&format!("{mem}Gi")), "memory")];
            for (key, value, what) in want {
                let got = hard.get(key).map(|q| q.0.as_str()).unwrap_or_default();
                let parsed = if what == "cpu" { millis(got) } else { bytes(got) };
                if parsed != value {
                    return Err(anyhow!("`owner-quota` bounds {key} at {got:?}, but the effective Quota says {value:?} (in {})", if what == "cpu" { "millicores" } else { "bytes" }));
                }
                if !used.contains_key(key) {
                    return Err(anyhow!("Kubernetes reports no usage for {key}, so it is not enforcing it"));
                }
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// A cpu Quantity in millicores, and a memory Quantity in bytes. Enough of the format to compare
/// two spellings of the same number — which is all this step does with them; anything it cannot
/// read answers `None`, and an unparsable ceiling is a mismatch rather than a silent pass.
fn millis(q: &str) -> Option<u64> {
    let q = q.trim();
    match q.strip_suffix('m') {
        Some(n) => n.parse().ok(),
        None => q.parse::<f64>().ok().map(|v| (v * 1000.0).round() as u64),
    }
}

fn bytes(q: &str) -> Option<u64> {
    let q = q.trim();
    for (suffix, mult) in [("Ki", 1u64 << 10), ("Mi", 1 << 20), ("Gi", 1 << 30), ("Ti", 1u64 << 40), ("K", 1_000), ("M", 1_000_000), ("G", 1_000_000_000)] {
        if let Some(n) = q.strip_suffix(suffix) {
            return n.parse::<f64>().ok().map(|v| (v * mult as f64).round() as u64);
        }
    }
    q.parse::<f64>().ok().map(|v| v.round() as u64)
}

/// `env.services.policies`: the `OwnerBinding`'s per-namespace NetworkPolicies exist.
///
/// The workspace namespace is default-deny plus three holes (`k8s::default_policies` and the
/// gateway's ingress rule), and it is the only thing between one person's workspace and the rest
/// of the cluster. `env.attach.pair` reads the `attach-{ws}` pair and nothing read these.
pub(super) async fn services_policies(c: &mut Ctx) {
    let Some(k) = c.kube.clone() else { return c.skip("env.services.policies", "no kubeconfig") };
    let ns = kloudlite_workspaces::crd::ws_namespace(&c.probe_user, "");
    c.step("env.services.policies", KUBE_CEILING, move |_| {
        async move {
            use k8s_openapi::api::networking::v1::NetworkPolicy;
            let api: kube::Api<NetworkPolicy> = kube::Api::namespaced(k.clone(), &ns);
            let list = api
                .list(&kube::api::ListParams::default())
                .await
                .map_err(|e| anyhow!("could not list the policies in {ns}: {e}"))?;
            let names: Vec<String> = list.items.iter().map(kube::ResourceExt::name_any).collect();
            let missing: Vec<&str> =
                BINDING_POLICIES.iter().copied().filter(|p| !names.iter().any(|n| n == p)).collect();
            if !missing.is_empty() {
                return Err(anyhow!("{ns} is missing {}", missing.join(", ")));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// The policies `apply_binding` writes into every owner namespace, by name. Repeated rather than
/// imported for the same reason `BOOT_FIELDS` is: what matters is the name on the object in the
/// cluster, and a probe that derived it from the same code that wrote it would agree with itself.
const BINDING_POLICIES: [&str; 5] =
    ["default-deny", "allow-dns", "allow-same-namespace", "allow-internet-egress", "allow-gateway-ssh"];

/// `web.pages`: every page route in the app's fixed list renders, each inside 1500 ms.
///
/// One id over a list, exactly as `admin.screens` covers the console's API: 26 of the app's 30
/// routes had no load SLO at all, including the whole PR surface and all ten `/superadmin`
/// screens, and 26 catalogue rows for one Next.js deployment would be 26 samples of one fact.
/// Each page is timed on its own, so a single slow route fails the id with its name rather than
/// disappearing into a total.
pub(super) async fn pages(c: &mut Ctx) {
    let (probe, repo) = (c.probe_user.clone(), c.state.repo.clone());
    c.step("web.pages", PAGES_CEILING, move |c| {
        let base = c.cfg.web_url.trim_end_matches('/').to_string();
        async move {
            let mut paths: Vec<String> = STATIC_PAGES.iter().map(|p| (*p).to_string()).collect();
            paths.push(format!("/{probe}"));
            paths.push(format!("/{probe}/registries"));
            if let Some(repo) = &repo {
                for tail in REPO_PAGES {
                    paths.push(format!("/{probe}/{repo}{tail}"));
                }
            }
            let mut slow = vec![];
            for path in paths {
                let at = std::time::Instant::now();
                let landing = path.split('?').next().unwrap_or(&path).to_string();
                super::git::renders(c, &format!("{base}{path}"), &landing)
                    .await
                    .with_context(|| format!("{path} did not render"))?;
                let ms = at.elapsed().as_millis();
                if ms > PAGE_MS {
                    slow.push(format!("{path} in {ms} ms"));
                }
            }
            if !slow.is_empty() {
                return Err(anyhow!("slower than {PAGE_MS} ms: {}", slow.join(", ")));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// The catalogue's own per-page ceiling. Not the step's: the step walks a dozen of them.
const PAGE_MS: u128 = 1_500;

/// Every page route that needs nothing from this run beyond a signed-out visitor, the ten
/// `/superadmin` screens included — the app renders the sign-in for a stranger, and a route that
/// was dropped or renamed lands on a 404 instead, which is what `renders` judges.
const STATIC_PAGES: [&str; 15] = [
    "/",
    "/new",
    "/new/team",
    "/settings",
    "/requests",
    "/superadmin",
    "/superadmin/requests",
    "/superadmin/owners",
    "/superadmin/clusters",
    "/superadmin/monitoring",
    "/superadmin/audit",
    "/superadmin/access",
    "/superadmin/configuration",
    "/superadmin/slo",
    "/superadmin/workloads",
];

/// The repo-scoped routes, appended to this run's own repo.
/// `/commits` takes its branch as `?ref=`, not as a path segment (`app/(shell)/[owner]/[repo]/
/// commits/page.tsx` — `searchParams: { ref, from }`), which is why the query is on the path here
/// and `renders` compares the path alone.
const REPO_PAGES: [&str; 6] =
    ["", "/tree/main", "/commits?ref=main", "/pulls", "/pulls/new", "/settings"];

/// `repo.metadata`: the browse `lastmod` route answers for a commit this run pushed.
///
/// The review's row was "`PATCH /v1/repos` beyond description"; the route accepts exactly two
/// fields, `description` and `visibility`, and both already have ids (`repo.description`,
/// `repo.visibility`) — so what is left uncovered on this line is `lastmod`, the browse route the
/// file listing's "last changed" column is built on.
pub(super) async fn metadata(c: &mut Ctx) {
    let (probe, Some(repo)) = (c.probe_user.clone(), c.state.repo.clone()) else {
        return c.skip("repo.metadata", "no repo");
    };
    c.step("repo.metadata", READ_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let refs = api(c, &format!("/api/{probe}/{repo}/refs"));
        let base = api(c, &format!("/api/{probe}/{repo}/lastmod"));
        async move {
            let refs = get(c, &refs, &jwt).await.context("could not read the refs")?;
            let oid = super::git::oid_of(&refs, "main")
                .ok_or_else(|| anyhow!("the repo has no `main` to ask about"))?;
            let doc = get(c, &format!("{base}/{oid}"), &jwt).await.context("lastmod would not answer")?;
            // An answer that names nothing is the failure the column shows as blank rows.
            if doc.is_null() || (doc.as_object().is_some_and(|o| o.is_empty())) {
                return Err(anyhow!("lastmod answered nothing for {oid}"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `id.session.reads`: the three single-route reads the review listed one by one.
///
/// Grouped because they fail together and for the same reason — the ordinary api process not
/// answering — and because three catalogue rows for three GETs is a catalogue nobody reads. The
/// passkey `used` mark is the interesting one: the web stamps it after a sign-in, so a route that
/// stopped accepting the stamp would leave every credential looking unused forever.
pub(super) async fn session_reads(c: &mut Ctx) {
    let name = format!("{}-used", c.prefix());
    c.step("id.session.reads", READ_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let (passkeys, central, legacy) = (
            api(c, "/v1/passkeys"),
            api(c, "/v1/settings/central"),
            api(c, "/v1/quota-requests"),
        );
        async move {
            // Public and display-only, so no token: a settings read that started demanding one
            // would break the clone box for every visitor of a public repo.
            let doc = get(c, &central, "").await.context("the api's own settings read")?;
            if doc.get("clone_host").is_none() && doc.get("cloneHost").is_none() {
                return Err(anyhow!("the settings read carries no clone host"));
            }
            // `NewPasskey` (crates/api/src/passkeys.rs:12) is `#[serde(rename_all = "camelCase")]`,
            // so the wire field is `publicKey` — `public_key` was still a 422, exactly as
            // `credential_id` had been.
            let made = post(c, &passkeys, &jwt, json!({ "id": name, "publicKey": name, "name": name }))
                .await
                .context("could not register a passkey to mark used")?;
            let id = made
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(&name)
                .to_string();
            // The `used` mark is PEER ONLY (`passkeys.rs:152`, `peer_only`) — it is stamped after a
            // sign-in, before a session exists — so what a session-holding probe can assert is the
            // refusal, which is the same half `id.signin.passkey` asserts for the lookup.
            let used = api(c, &format!("/v1/passkeys/{id}/used"));
            let (status, body) = raw(c, reqwest::Method::POST, &used, &jwt, Some(json!({ "counter": 1 })), &[]).await?;
            let mark = match status.as_u16() {
                401 | 403 => Ok(()),
                other => Err(anyhow!("the `used` mark answered {other} to a session, and it is peer-only: {}", body.chars().take(160).collect::<String>())),
            };
            // The credential goes whatever the mark did — a probe passkey left on the account is
            // a credential nobody owns.
            let _ = super::call(
                c,
                reqwest::Method::DELETE,
                &api(c, &format!("/v1/passkeys/{id}")),
                &jwt,
                None,
            )
            .await;
            mark?;
            // The retired create, which the console still unions in. One pending per owner per
            // kind, so a 409 here is the previous run's row and not a failure of the route.
            let (status, body) =
                raw(
                    c,
                    reqwest::Method::POST,
                    &legacy,
                    &jwt,
                    // `NewQuotaRequest` nests the dimensions under `requested` (a
                    // `RequestedQuota`, crates/workspaces/src/crd/mod.rs:935) — a flat `diskGb`
                    // is a 422 before the handler ever runs.
                    Some(json!({ "reason": "slo probe legacy create", "requested": { "diskGb": 1 } })),
                    &[],
                )
                .await?;
            if !status.is_success() && status.as_u16() != 409 {
                return Err(anyhow!("the legacy quota-request create answered {status}: {}", body.chars().take(200).collect::<String>()));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `kl.commands`: the three CLI verbs nothing walked.
///
/// `kl-connect logout` is LAST and deliberate: it forgets this pod's stored token, which every earlier
/// `kl` step has already used — and the token itself is revoked by teardown either way.
pub(super) async fn kl_commands(c: &mut Ctx) {
    let device = format!("{}-klc", c.prefix());
    let home = c.tmp.join("klhome-cmd");
    c.step("kl.commands", CLI_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let (kl, api_url) = (c.programs.kl.clone(), c.cfg.api_url.clone());
        let probe = c.probe_user.clone();
        async move {
            // A real CLI token and the config `kl-connect login` would have written: without it every one
            // of these exits 1 with "not logged in", which measures the probe, not the CLI. The
            // same staging `id.cli.sshconfig` does, and the same revoke afterwards.
            let (token, id) = super::experience_gaps::cli_login(c, &jwt, &device).await?;
            // A 404 is SUCCESS for an undo: `kl-connect logout` revokes the token itself, so the
            // credential this would take back is already gone — and "already revoked" is the state
            // the compensation wanted. Anything else still fails the step.
            let revoke = || async {
                let url = api(c, &format!("/v1/cli/tokens/{id}"));
                let (status, body) = raw(c, reqwest::Method::DELETE, &url, &jwt, None, &[]).await?;
                match status.as_u16() {
                    404 => Ok(()),
                    code if (200..300).contains(&code) => Ok(()),
                    code => Err(anyhow!("the CLI token was left LIVE: {code}: {}", body.chars().take(160).collect::<String>())),
                }
            };
            let body = async {
                let dir = home.join(".config/kl-connect");
                std::fs::create_dir_all(&dir).with_context(|| format!("could not make {}", dir.display()))?;
                let cfg = json!({
                    "api": api_url,
                    "token": token,
                    "expires_at": "2099-01-01T00:00:00Z",
                    "username": probe,
                });
                std::fs::write(dir.join("config.json"), cfg.to_string()).context("could not stage the CLI login")?;
                let env = std::collections::HashMap::from([
                    ("HOME".to_string(), home.display().to_string()),
                    ("KL_CONFIG_DIR".to_string(), dir.display().to_string()),
                ]);
                // `logout` LAST: it forgets the config the two before it read.
                for args in [vec!["ws", "list"], vec!["ws", "list", "--team", "no-such-team"], vec!["logout"]] {
                    let argv: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
                    let what = argv.join(" ");
                    if let Err(e) = tools::run(&kl, &argv, &env, None, CLI_CEILING).await {
                        let detail = format!("{e:#}");
                        // `--team` on a team nobody is in must ANSWER, empty or refused; what it
                        // may not do is fail to run at all.
                        let refused = detail.contains("no such team") || detail.contains("not a member");
                        if !(what.contains("--team") && refused) {
                            return Err(anyhow!("`kl-connect {what}` failed: {detail}"));
                        }
                    }
                }
                Ok(())
            };
            crate::drill::undoing(CLI_CEILING - Duration::from_secs(10), body, revoke).await
        }
        .boxed()
    })
    .await;
}

/// `admin.reads`: the console reads the review found unprobed, plus the allow-list's own refusal.
///
/// The 404 at the end is the half worth having: `/admin/history/{series}` interpolates the caller's
/// name into SQL through an allow-list because that path has no bound parameters, so a name that
/// is NOT on the list must be a 404 and never a query.
/// What `admin.reads` is demoted to when the history layer is not deployed: the allow-list half
/// of the SLI could not be attempted, and a `503` is not evidence it holds.
pub(super) const NO_HISTORY: &str =
    "the history layer answered 503, so the series allow-list was never consulted";

pub(super) async fn reads(c: &mut Ctx) {
    let region = c.cfg.region.clone();
    c.step("admin.reads", READ_CEILING, move |c| {
        let jwt = c.admin_jwt.clone();
        let (nodes, schema) = (admin(c, "/admin/nodes"), admin(c, "/admin/settings/schema"));
        let status = admin(c, &format!("/admin/clusters/{region}/status"));
        let unknown = admin(c, "/admin/history/no-such-series?range=1d&step=1h");
        async move {
            let rows = get(c, &nodes, &jwt).await.context("`/admin/nodes`")?;
            if rows.get("nodes").and_then(Value::as_array).or_else(|| rows.as_array()).is_none() {
                return Err(anyhow!("`/admin/nodes` did not answer a list"));
            }
            let doc = get(c, &schema, &jwt).await.context("`/admin/settings/schema`")?;
            // `{ central: [...], cluster: [...] }` — one row per meta-table entry
            // (`api/admin/schema.rs:246`). BOTH scopes, because the Configuration screen renders
            // the two tabs from them and a missing one is a blank tab.
            for scope in ["central", "cluster"] {
                let rows = doc.get(scope).and_then(Value::as_array).map(Vec::len).unwrap_or(0);
                if rows == 0 {
                    return Err(anyhow!("the settings schema names no {scope} field"));
                }
            }
            // `active` is what the region already is — the write is the route being exercised,
            // never a change to the fleet's own state.
            super::call(c, reqwest::Method::PUT, &status, &jwt, Some(json!({ "status": "active", "note": "slo probe status read" })))
                .await
                .context("the cluster status write was refused")?;
            let (code, body) = raw(c, reqwest::Method::GET, &unknown, &jwt, None, &[]).await?;
            match code.as_u16() {
                404 => Ok(()),
                // `503 history unavailable` is the contract for a deployment with no ClickHouse —
                // the allow-list was never consulted, so nothing about it was measured. It used to
                // pass, which made the id green on every cluster that has no history layer at all
                // (2026-09-12). The step reports it, and the stage demotes it to a skip.
                503 => Err(anyhow!("{NO_HISTORY}")),
                other => Err(anyhow!("an unknown history series answered {other}, not 404: {}", body.chars().take(200).collect::<String>())),
            }
        }
        .boxed()
    })
    .await;
    // No ClickHouse is a deployment's shape, not a broken allow-list — the step names it and the
    // sample becomes a skip rather than either a pass or a failure.
    if c.steps.iter().rev().find(|s| s.slo_id == "admin.reads").is_some_and(|s| s.detail.contains(NO_HISTORY)) {
        c.demote_to_skip("admin.reads", NO_HISTORY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;

    /// Two spellings of one ceiling are one ceiling. A string comparison failed a namespace quota
    /// that was correct, which is the whole reason these two exist.
    #[test]
    fn a_quantity_is_compared_as_a_number() {
        assert_eq!(millis("4"), millis("4000m"));
        assert_eq!(bytes("8Gi"), bytes("8589934592"));
        assert_ne!(bytes("8Gi"), bytes("8G"));
        assert_eq!(millis("nonsense"), None);
    }

    /// Every id this file owns is produced exactly once with nothing reachable — a run that
    /// measured nothing is still a complete run, which is what lets the console tell a grey stage
    /// from a broken one.
    #[tokio::test]
    async fn every_id_is_produced_once_with_nothing_reachable() {
        let app = axum::Router::new();
        let mut c = testkit::ctx_against(app).await;
        c.kube = None;
        quota_namespace(&mut c).await;
        services_policies(&mut c).await;
        pages(&mut c).await;
        metadata(&mut c).await;
        session_reads(&mut c).await;
        kl_commands(&mut c).await;
        reads(&mut c).await;
        let ids: Vec<&str> = c.steps.iter().map(|s| s.slo_id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "ws.quota.namespace",
                "env.services.policies",
                "web.pages",
                "repo.metadata",
                "id.session.reads",
                "kl.commands",
                "admin.reads",
            ]
        );
    }
}
