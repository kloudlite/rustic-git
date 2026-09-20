//! What a bench gained by BECOMING a Workspace
//! (`docs/superpowers/specs/2026-09-16-bench-is-a-workspace-design.md`): its transcripts live in a
//! replicated volume that the ordinary push verb cuts, and the ordinary package list is editable
//! from its own shell. Both are the workspace machinery — `POST /v1/workspaces/{bench}/push` and
//! `kl pkg` — reached through the facade's id, which is the point: a bench that needed its own
//! push or its own package path would be the half-maintained second kind this plan deleted.
//!
//! Walked LAST in group 3, after `bench_tool::run` has restarted the bench: a package edit
//! recreates the pod, so nothing in this group may run after it, and group 0's tool round trip
//! waits for the whole group anyway.
//!
//! There is deliberately NO id for the one-time legacy folder migration. The legacy folder is
//! `{pool}/homes/.benches/{team}/{owner}` on the region's NFS export, which only the privileged
//! agent DaemonSet mounts — a workspace pod sees `{pool}/homes/{owner}` and nothing beside it — so
//! seeding one would mean execing into the agent pod, which the probe's role documents it never
//! does (`deploy/k3s/slo-rbac.yaml`: "only ever execs into pods it created"). A permanently
//! skipped id reads as a pass on the console, which is worse than a stated gap: add the id back
//! WITH a seeding path, never before one.

use std::time::Duration;

use anyhow::{Context, Result};
use futures::FutureExt;
use serde_json::{json, Value};

use super::bench::wait_phase;
use super::{api, call, get, poll_json, post};
use crate::ctx::Ctx;

/// `bench.push.p95`: target 60 s.
const PUSH_CEILING: Duration = Duration::from_secs(90);
/// `bench.pkg.add`: target 20 s — the id is about the spec landing, never about nix building it.
const PKG_CEILING: Duration = Duration::from_secs(30);
/// Strictly less than `PKG_CEILING` so the PATCH before it always fits inside the step's own
/// budget — the two used to share `PKG_CEILING`, so a slow poll could consume the whole step and
/// leave nothing for the PATCH ("timed out after 30000 ms" on two fleet hourlies).
const PKG_POLL: Duration = Duration::from_secs(25);
/// Cheap, tiny and nothing else in the suite declares it, so a leftover is visible rather than
/// indistinguishable from a real package somebody wanted.
const PKG: &str = "cowsay";

/// The bench's own id, which is also its Volume's name (`ws_volume` falls back to the workspace
/// name until a push publishes a pointer, exactly as `stages::workspace` relies on).
async fn bench_id(c: &Ctx) -> Result<String> {
    let doc = get(c, &api(c, "/v1/bench"), &c.probe_jwt).await?;
    Ok(doc.get("id").and_then(Value::as_str).context("GET /v1/bench answered no id")?.to_string())
}

pub async fn run(c: &mut Ctx) {
    let id = match bench_id(c).await {
        Ok(id) => id,
        Err(e) => {
            let why = format!("the bench could not be read: {}", super::clip(&format!("{e:#}")));
            c.skip("bench.push.p95", &why);
            return c.skip("bench.pkg.add", &why);
        }
    };
    push(c, &id).await;
    pkg_add(c, &id).await;
}

/// `bench.push.p95`: the bench's transcripts are cut by the ordinary push and the ordinary history
/// lists the cut. A `working` row is not a push anybody could restore from, so the row must be
/// `ready` — the same judgement `ws.push.p95` makes, through `row_ready`.
async fn push(c: &mut Ctx, id: &str) {
    let cut: std::sync::Arc<std::sync::Mutex<Option<String>>> = Default::default();
    let (id, taken) = (id.to_string(), cut.clone());
    let (push_path, history_path) = (format!("/v1/workspaces/{id}/push"), format!("/v1/volumes/{id}/history"));
    c.step("bench.push.p95", PUSH_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &push_path);
        let history = api(c, &history_path);
        let taken = taken.clone();
        async move {
            let doc = post(c, &url, &jwt, json!({})).await.context("could not push the bench")?;
            let snap = doc.get("id").and_then(Value::as_str).context("the push answered no snapshot id")?.to_string();
            // Recorded before the wait: a cut that never turned ready is still a snapshot holding
            // a quota slot, and teardown has to know about it either way.
            *taken.lock().unwrap() = Some(snap.clone());
            poll_json(c, &history, &jwt, PUSH_CEILING, |v| super::workspace::row_ready(v, &snap))
                .await
                .context("the bench's snapshot never turned ready")
        }
        .boxed()
    })
    .await;
    // Untimed teardown, and the reason this id needs one where `ws.push.p95` does not: the bench is
    // LONG-LIVED, so its volume keeps every hour's push until the owner's `snapshots` quota is full
    // — "snapshots: 20 of 20" is what failed this id AND `vol.history` in hourly-manual-0456.
    //
    // Never this run's own cut: a push advances `status.head`, so the newest snapshot is the running
    // bench's base and `/v1` refuses it 409 (`delete_snapshot`). What goes is every OLDER push —
    // last hour's and the ones before it — which is exactly the leak, and leaves the volume with one
    // restorable point.
    if cut.lock().unwrap().is_some() {
        prune_pushes(c, &id).await;
    }
}

/// Every push on the bench's volume but the newest, deleted. Warnings only: a teardown that failed
/// a sample would report the state of last hour's run.
async fn prune_pushes(c: &Ctx, id: &str) {
    let history = match get(c, &api(c, &format!("/v1/volumes/{id}/history")), &c.probe_jwt).await {
        Ok(v) => v,
        Err(e) => return tracing::warn!(error = %format!("{e:#}"), "slo.bench_ws.push.history"),
    };
    // Newest first (`vol.history`'s own contract), so the head is row zero.
    let older: Vec<String> = history
        .as_array()
        .map(|rows| rows.iter().skip(1).filter_map(|r| r.get("id").and_then(Value::as_str)).map(str::to_string).collect())
        .unwrap_or_default();
    for snap in older {
        let url = api(c, &format!("/v1/volumes/{id}/snapshots/{snap}"));
        if let Err(e) = call(c, reqwest::Method::DELETE, &url, &c.probe_jwt, None).await {
            tracing::warn!(error = %format!("{e:#}"), snapshot = %snap, "slo.bench_ws.push.teardown");
        }
    }
}

/// `bench.pkg.add` reads the bench's CURRENT package list and appends, never overwrites — the
/// bench is long-lived and shared, so a fixed PATCH here wiped whatever the owner had already
/// installed, and the teardown's `{ "packages": [] }` wiped everything a second time. `original`
/// is read before the timed step (this probe is about the spec landing, not about a GET), and the
/// teardown restores exactly that snapshot, not the empty list.
///
/// The wait is on the SPEC, never on `PackagesReady`: a nix build is minutes and is
/// `ws.packages.add`'s sample, not this one's.
async fn pkg_add(c: &mut Ctx, id: &str) {
    let workspace_id = id.to_owned();
    let ws = api(c, &format!("/v1/workspaces/{id}"));
    let original: Vec<String> = match get(c, &ws, &c.probe_jwt).await {
        Ok(doc) => doc
            .get("packages")
            .and_then(Value::as_array)
            .map(|ps| ps.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default(),
        Err(e) => {
            let why = format!("could not read the bench's current packages: {}", super::clip(&format!("{e:#}")));
            return c.skip("bench.pkg.add", &why);
        }
    };
    let wanted = with_package(&original, PKG);
    let landed = c
        .step("bench.pkg.add", PKG_CEILING, move |c| {
            let jwt = c.probe_jwt.clone();
            let ws = ws.clone();
            let workspace_id = workspace_id.clone();
            let wanted = wanted.clone();
            async move {
                set_packages(c, &workspace_id, &wanted).await.context("the authenticated workspace package update failed")?;
                // Strictly shorter than the step's own ceiling: the PATCH above must always have
                // room left in the step's budget, or a slow poll starves it (the "timed out after
                // 30000 ms" the two shared PKG_CEILING produced on the fleet).
                poll_json(c, &ws, &jwt, PKG_POLL, |v| declares(v, PKG)).await.context("the package never reached the bench's spec")
            }
            .boxed()
        })
        .await;
    if !landed {
        return;
    }
    // Untimed teardown: the bench is long-lived, so a package left behind would be rebuilt into
    // every later run's profile. Through `/v1` rather than the shell — the pod is being recreated
    // for the package that just landed, and a second shell would race the kubelet. Restores the
    // ORIGINAL list read before the step, never `[]` — the bench may have carried packages of its
    // own before this run touched it.
    let removed = async {
        // `patch_ws_packages` IS the PATCH on the workspace and takes packages only; there is no
        // `/packages` route (`api::mod`'s note on the spelling).
        set_packages(c, id, &original).await?;
        wait_phase(c, "ready", Duration::from_secs(180)).await
    };
    if let Err(e) = removed.await {
        tracing::warn!(error = %format!("{e:#}"), "slo.bench_ws.pkg.teardown");
    }
}

/// `original` with `pkg` appended if it is not already present — order preserved, no duplicate.
fn with_package(original: &[String], pkg: &str) -> Vec<String> {
    if original.iter().any(|p| p == pkg) {
        return original.to_vec();
    }
    let mut wanted = original.to_vec();
    wanted.push(pkg.to_string());
    wanted
}

async fn set_packages(c: &Ctx, id: &str, packages: &[String]) -> Result<()> {
    call(
        c,
        reqwest::Method::PATCH,
        &api(c, &format!("/v1/workspaces/{id}")),
        &c.probe_jwt,
        Some(json!({ "packages": packages })),
    )
    .await
    .map(|_| ())
}

/// Whether a `/v1/workspaces/{id}` doc declares `want`, pinned or not (`attr@version`).
fn declares(v: &Value, want: &str) -> bool {
    v.get("packages").and_then(Value::as_array).is_some_and(|ps| {
        ps.iter().filter_map(Value::as_str).any(|p| p == want || p.split('@').next() == Some(want))
    })
}

const _: () = assert!(PKG_POLL.as_secs() < PKG_CEILING.as_secs());

#[cfg(test)]
mod tests {
    use super::*;
    use kloudlite_workspaces::slo::catalogue::find;

    #[test]
    fn the_package_judgement_reads_pins_and_the_ceilings_cover_the_targets() {
        assert!(declares(&json!({"packages": ["jq", "cowsay"]}), PKG));
        assert!(declares(&json!({"packages": ["cowsay@3.04"]}), PKG), "a pinned entry still declares the attr");
        assert!(!declares(&json!({"packages": ["cowsay-extra"]}), PKG));
        assert!(!declares(&json!({"packages": []}), PKG));
        assert!(!declares(&json!({}), PKG));
        for (id, cap) in [("bench.push.p95", PUSH_CEILING), ("bench.pkg.add", PKG_CEILING)] {
            let slo = find(id).expect(id);
            assert!(cap.as_millis() >= slo.target.max_ms.unwrap() as u128, "{id}");
            assert_eq!(crate::suite::group_of(id), 3, "{id}");
        }
    }

    #[test]
    fn with_package_appends_once_and_preserves_order() {
        assert_eq!(with_package(&[], PKG), vec![PKG.to_string()], "absent: appended");
        assert_eq!(
            with_package(&["jq".to_string(), "yq".to_string()], PKG),
            vec!["jq".to_string(), "yq".to_string(), PKG.to_string()],
            "absent: appended after the existing list, order preserved"
        );
        assert_eq!(
            with_package(&["jq".to_string(), PKG.to_string()], PKG),
            vec!["jq".to_string(), PKG.to_string()],
            "present: list untouched"
        );
    }
}
