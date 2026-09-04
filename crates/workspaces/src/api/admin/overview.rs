//! `GET /admin/overview` — the landing page's one round trip: what needs a decision (pending
//! requests, attention items) plus what just happened (recent audit) and the fleet's size, all
//! composed from the other admin modules' own readers rather than a second walk of the CRDs.
//!
//! `monitoring.rs` is off limits to this task (under separate review), so its signals are reached
//! by calling its own route handler in-process — a function call, never an HTTP round trip — and
//! reading the `signals` field back out of the `Response` it already builds. Every other section
//! reuses a `pub(crate)` function the owning module exports for exactly this purpose.

use super::*;

#[derive(serde::Serialize)]
pub(crate) struct AttentionItem {
    kind: &'static str,
    detail: String,
    href: String,
}

#[derive(serde::Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RegionFleet {
    owners: i64,
    workspaces: i64,
    environments: i64,
    snapshots: i64,
    disk_gb: i64,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FleetNumbers {
    owners: i64,
    workspaces: i64,
    environments: i64,
    snapshots: i64,
    disk_gb_total: i64,
    per_region: std::collections::BTreeMap<String, RegionFleet>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Overview {
    pending_requests: Vec<super::super::QuotaRequestDoc>,
    attention: Vec<AttentionItem>,
    recent_audit: Vec<crate::audit::AuditEntry>,
    fleet: FleetNumbers,
    /// A sub-source that could not be read (the signals scrape needs `aks`, the audit log needs
    /// an object store) degrades to a line here rather than a 5xx for the whole page — every
    /// other section still renders.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    errors: Vec<String>,
}

/// Oldest first, `None` (no creation timestamp — should not happen, but is not this handler's to
/// assume) sorted LAST rather than promoted to the front by `Option`'s default `None < Some`
/// ordering, which would put an undated request ahead of every dated one.
fn by_creation_oldest_first(a: &crd::QuotaRequest, b: &crd::QuotaRequest) -> std::cmp::Ordering {
    match (&a.metadata.creation_timestamp, &b.metadata.creation_timestamp) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(x), Some(y)) => x.cmp(y),
    }
}

/// Oldest pending first, capped at three — the landing page's queue teaser, not the full list
/// `GET /admin/quota-requests` already serves.
async fn pending_oldest_first(s: &ApiState) -> Result<Vec<super::super::QuotaRequestDoc>, Response> {
    let filter = RequestFilter { owner: None, state: Some(crd::RequestState::Pending) };
    let mut rows = list_all_quota_requests_inner(s, &filter).await?;
    rows.sort_by(by_creation_oldest_first);
    rows.truncate(3);
    Ok(rows.iter().map(super::super::request_doc).collect())
}

fn workload_attention(rows: &[super::super::workloads::WorkloadDoc]) -> impl Iterator<Item = AttentionItem> + '_ {
    rows.iter().filter(|d| d.ready < d.desired).map(|d| AttentionItem {
        kind: "workload",
        detail: format!("{} ({}): {}/{} ready", d.name, d.scope, d.ready, d.desired),
        href: "/superadmin/clusters".into(),
    })
}

fn node_attention(rows: &[super::NodeDoc]) -> impl Iterator<Item = AttentionItem> + '_ {
    rows.iter().filter(|n| !n.ready || n.decommission).map(|n| AttentionItem {
        kind: "node",
        detail: if n.ready {
            format!("{} draining", n.name)
        } else {
            format!("{} NotReady", n.name)
        },
        href: "/superadmin/clusters".into(),
    })
}

fn cluster_attention(rows: &[clusters::ClusterRow]) -> impl Iterator<Item = AttentionItem> + '_ {
    rows.iter().flat_map(|c| {
        let zero_agents = (c.agents_ready == 0).then(|| AttentionItem {
            kind: "region",
            detail: format!("{}: no agents ready", c.region),
            href: format!("/superadmin/clusters/{}", c.region),
        });
        // `present` is the only settings state that needs no attention — `absent` (never saved)
        // and `stale (lag N)` (a save that has not reached every reader yet) both belong here,
        // same as a hypothetical decode failure would.
        let bad_settings = (c.settings_status != "present").then(|| AttentionItem {
            kind: "settings",
            detail: format!("{}: settings {}", c.region, c.settings_status),
            href: format!("/superadmin/clusters/{}", c.region),
        });
        zero_agents.into_iter().chain(bad_settings)
    })
}

/// The pure half of `firing_signals` — every `state: "firing"` row of `monitoring::signals`'s own
/// JSON shape, mapped to an attention item. Split out so it is unit-testable without a live
/// scrape (`monitoring.rs`'s HTTP fetch has no mockable seam this task can add).
fn firing_from_signals_json(body: &serde_json::Value) -> Vec<AttentionItem> {
    body["signals"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r["state"] == "firing")
        .map(|r| AttentionItem {
            kind: "signal",
            detail: r["alert"].as_str().unwrap_or("unknown").to_string(),
            href: "/superadmin/monitoring".into(),
        })
        .collect()
}

/// `monitoring::signals`'s full route, called directly rather than over HTTP — reused as a black
/// box because `monitoring.rs` is under separate review and not this task's to edit. A failure
/// (no `aks` client wired, most likely) degrades to an `errors` line, matching every other
/// sub-source here, never a 5xx for the whole page.
async fn firing_signals(s: &Arc<ApiState>) -> Result<Vec<AttentionItem>, String> {
    let resp = monitoring::signals(State(s.clone())).await.map_err(|r| format!("signals: HTTP {}", r.status()))?;
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .map_err(|e| format!("signals: {e}"))?;
    let body: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| format!("signals: {e}"))?;
    Ok(firing_from_signals_json(&body))
}

/// The fleet's size, globally and per region — folded from the same six list calls
/// `owners::owner_rows` is built on (`owners::fleet`), never a second listing.
fn fleet_numbers(f: &owners::Fleet) -> FleetNumbers {
    let mut per_region: std::collections::BTreeMap<String, RegionFleet> = std::collections::BTreeMap::new();
    for w in &f.ws {
        let r = per_region.entry(w.spec.region.clone()).or_default();
        r.workspaces += 1;
    }
    for e in &f.envs {
        let r = per_region.entry(e.spec.region.clone()).or_default();
        r.environments += 1;
    }
    let vol_region: std::collections::HashMap<String, String> =
        f.vols.iter().map(|v| (v.name_any(), v.spec.region.clone())).collect();
    for v in &f.vols {
        let r = per_region.entry(v.spec.region.clone()).or_default();
        r.disk_gb += v.spec.quota_gb as i64;
    }
    for s in &f.snaps {
        if s.is_snapshot() {
            if let Some(region) = vol_region.get(&s.spec.volume) {
                per_region.entry(region.clone()).or_default().snapshots += 1;
            }
        }
    }
    // Owners per region: whoever has a workspace, environment or volume there — the same three
    // kinds `owners::fold_usage` counts, joined on region instead of folded by owner.
    let mut owners_by_region: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    for w in &f.ws {
        owners_by_region.entry(w.spec.region.clone()).or_default().insert(w.spec.owner.clone());
    }
    for e in &f.envs {
        owners_by_region.entry(e.spec.region.clone()).or_default().insert(e.spec.owner.clone());
    }
    for v in &f.vols {
        owners_by_region.entry(v.spec.region.clone()).or_default().insert(v.spec.owner.clone());
    }
    for (region, owners) in owners_by_region {
        per_region.entry(region).or_default().owners = owners.len() as i64;
    }

    FleetNumbers {
        owners: f.owners.len() as i64,
        workspaces: f.ws.len() as i64,
        environments: f.envs.len() as i64,
        snapshots: f.snaps.iter().filter(|s| s.is_snapshot()).count() as i64,
        disk_gb_total: f.vols.iter().map(|v| v.spec.quota_gb as i64).sum(),
        per_region,
    }
}

pub(crate) async fn overview_handler(State(s): State<Arc<ApiState>>) -> Result<Response, Response> {
    let mut errors = Vec::new();

    let pending_requests = match pending_oldest_first(&s).await {
        Ok(rows) => rows,
        Err(resp) => {
            errors.push(format!("pending requests: HTTP {}", resp.status()));
            Vec::new()
        }
    };

    let regions = active_regions(&s).await.unwrap_or_default();
    let workloads = super::super::workloads::list_workloads(&s, &regions).await.unwrap_or_default();

    // The one fetch of Workspace/Environment this whole page needs: `cluster_rows_degraded`
    // folds them per region and `fleet_numbers` folds them per owner, so nothing after this line
    // lists either CRD again.
    let f = match kube(&s) {
        Ok(client) => match owners::fleet(client).await {
            Ok(f) => f,
            Err(resp) => {
                errors.push(format!("fleet: HTTP {}", resp.status()));
                owners::Fleet::default()
            }
        },
        Err(resp) => {
            errors.push(format!("fleet: HTTP {}", resp.status()));
            owners::Fleet::default()
        }
    };
    let fleet = fleet_numbers(&f);

    let (cluster_rows, nodes, cluster_errors) = clusters::cluster_rows_degraded(&s, &f.ws, &f.envs).await;
    errors.extend(cluster_errors);

    let mut attention: Vec<AttentionItem> =
        workload_attention(&workloads).chain(node_attention(&nodes)).chain(cluster_attention(&cluster_rows)).collect();
    match firing_signals(&s).await {
        Ok(mut rows) => attention.append(&mut rows),
        Err(e) => errors.push(e),
    }

    let recent_audit = match s.keys.as_ref() {
        Some(store) => match crate::audit::list(&store.os, crate::audit::AuditFilter::default(), None, 10).await {
            Ok(page) => page.rows,
            Err(e) => {
                errors.push(format!("audit: {e}"));
                Vec::new()
            }
        },
        None => {
            errors.push("audit: object store not configured".into());
            Vec::new()
        }
    };

    Ok(Json(Overview { pending_requests, attention, recent_audit, fleet, errors }).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A firing row becomes one `signal` attention item; an `ok`/`unknown` row is dropped — the
    /// one behavior `monitoring::signals`'s own HTTP fetch has no seam to mock in this crate.
    #[test]
    fn only_firing_signals_become_attention_items() {
        let body = serde_json::json!({"signals": [
            {"alert": "NoLeader", "state": "firing", "why": "", "detail": null},
            {"alert": "DbFenceDetected", "state": "ok", "why": "", "detail": null},
            {"alert": "TunnelSaturation", "state": "unknown", "why": "", "detail": null},
        ]});
        let items = firing_from_signals_json(&body);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "signal");
        assert_eq!(items[0].detail, "NoLeader");
    }

    /// A missing/malformed `signals` array yields no items, never a panic — the same shape a
    /// `signals` call this task cannot exercise end to end (no `aks` scrape target in a unit
    /// test) would answer with.
    #[test]
    fn a_missing_signals_array_yields_nothing() {
        assert!(firing_from_signals_json(&serde_json::json!({})).is_empty());
    }
}
