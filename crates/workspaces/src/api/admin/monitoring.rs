//! `GET /admin/monitoring/signals`: the alert catalogue's CURRENT state, read from `rustic.alerts`.
//!
//! This used to scrape every pod on the request path and evaluate the rules from one instant, which
//! meant nine of the catalogue's ten rules were permanently `unknown` — a `for 5m` window cannot be
//! computed from a point. The evaluation moved to `history::alerts` (a 30 s beat over the
//! collector's samples, with real windows); this handler only reads what it wrote.
//!
//! The response SHAPE is unchanged — the web already consumes `signals`, `restarts` and the counts
//! — with one field added, `source`, so the page can say whether it is showing measurements
//! (`"history"`) or a region nothing is reporting for (`"none"`).

use crate::api::{admin::history_or_503, aks, kube_err, ApiState};
use crate::history::alerts::{current_signals, SignalRow, CATALOGUE};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams, ResourceExt};
use std::sync::Arc;

#[derive(serde::Serialize)]
struct Restarts {
    workload: &'static str,
    /// ponytail: `restartCount` since each pod started, NOT a 1 h window — Kubernetes exposes no
    /// such number. The page says "since the pod started", so the field asserts no precision it
    /// does not have. Upgrade path: `k8s.container.restarts` is in the collector's tables now, so
    /// this can become a windowed query like the alert rules once the page wants one.
    restarts: i32,
}

#[derive(serde::Serialize)]
struct SignalsResponse {
    signals: Vec<SignalRow>,
    restarts: Vec<Restarts>,
    /// Kept for the web's existing rendering. Nothing is scraped on this path any more, so it is
    /// always empty — removing the field would break the page before sub-project C replaces it.
    scrape_failures: Vec<(String, String)>,
    pods_listed: usize,
    /// `"history"` when at least one rule has a recorded state, `"none"` when nothing is reporting.
    source: &'static str,
    /// Only when `RUSTIC_GIT_HYPERDX_URL` is set: a dead link on a monitoring page is worse than
    /// no link.
    #[serde(skip_serializing_if = "Option::is_none")]
    hyperdx_url: Option<String>,
}

pub(crate) async fn signals(State(s): State<Arc<ApiState>>) -> Result<Response, Response> {
    let h = history_or_503(&s)?;
    let recorded = current_signals(h).await.map_err(|e| {
        (axum::http::StatusCode::BAD_GATEWAY, format!("history: {e}")).into_response()
    })?;
    let source = if recorded.is_empty() { "none" } else { "history" };
    // A region nothing has reported for still shows every rule, `unknown` with the reason — an
    // empty table would read as "nothing is wrong".
    let mut signals = recorded;
    for rule in CATALOGUE {
        if !signals.iter().any(|r| r.alert == rule.name) {
            signals.push(SignalRow {
                alert: rule.name.to_string(),
                region: String::new(),
                state: "unknown".into(),
                why: rule.why.to_string(),
                detail: Some("no collector reporting for this region".into()),
            });
        }
    }

    let client = aks(&s)?;
    let pods = Api::<Pod>::namespaced(client.clone(), "rustic-git")
        .list(&ListParams::default())
        .await
        .map_err(kube_err)?;
    let restarts = crate::api::workloads::KNOWN_CENTRAL
        .iter()
        .map(|(workload, _)| Restarts {
            workload,
            // Pod names are `{workload}-…` for both a Deployment's ReplicaSet and a StatefulSet's
            // ordinal, which ties a pod to the KNOWN entry without a per-workload label selector
            // (the server tier's labels differ from the rest).
            restarts: pods
                .iter()
                .filter(|p| p.name_any().starts_with(&format!("{workload}-")))
                .flat_map(|p| p.status.iter())
                .flat_map(|st| st.container_statuses.iter().flatten())
                .map(|c| c.restart_count)
                .sum(),
        })
        .collect();

    Ok(Json(SignalsResponse {
        signals,
        restarts,
        scrape_failures: Vec::new(),
        pods_listed: pods.items.len(),
        source,
        hyperdx_url: std::env::var("RUSTIC_GIT_HYPERDX_URL")
            .ok()
            .filter(|u| !u.is_empty()),
    })
    .into_response())
}
