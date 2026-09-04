//! `deploy/alerts.md`'s catalogue, evaluated without Prometheus.
//!
//! Every pod already serves `/metrics` on 9464 and is annotated `prometheus.io/scrape`, so the
//! console scrapes them itself on the request path and answers each catalogue rule
//! firing / ok / unknown. Nothing here invents a rule: the table below is `deploy/alerts.md`'s
//! table, in its order, by its names, and a rule whose PromQL needs a window this process cannot
//! see (a 5–10m rate, node-exporter) is `unknown` with the reason — never guessed as `ok`, which
//! is the only failure mode that would matter on a monitoring page.
//!
//! ponytail: the rate rules use a two-point window (a cached previous scrape, or a second scrape
//! 5 s later) instead of the catalogue's 5–10 m. A 5 s window sees a burst, not a trend, so only
//! the ratio rules — which are scale-free — are evaluated this way and the `for 5m`/`for 10m`
//! rules stay `unknown`. Upgrade path: deploy Prometheus and query it instead of this module.

use crate::api::{aks, kube_err, ApiState};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams, ResourceExt};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A pod that does not answer in this long is a failed scrape, not a slow page: the whole handler
/// is on a superadmin's request path and already spends 5 s on the rate window in the worst case.
const SCRAPE_TIMEOUT: Duration = Duration::from_secs(3);

/// How far apart the two points of a rate window are when there is no usable cached sample.
const RATE_WINDOW: Duration = Duration::from_secs(5);

/// A cached sample older than this is not a window, it is history — a counter delta across an
/// unknown number of restarts and rolls says nothing, so we take a fresh second point instead.
const SAMPLE_MAX_AGE: Duration = Duration::from_secs(300);

/// `prometheus.io/port`'s default and `RUSTIC_GIT_METRICS_ADDR`'s port (`crates/core/src/metrics.rs`).
const DEFAULT_METRICS_PORT: &str = "9464";

// ── the text-exposition parser ──────────────────────────────────────────────

/// Sum every series named `metric` whose label section contains `label` (`Some(("status", "5xx"))`),
/// or `None` if the scrape exposes no such series at all — absent and zero are different answers
/// here, and conflating them is what would report `ok` for a metric nobody emits.
///
/// ponytail: substring matching on the raw label section, and no unescaping of `\"`/`\\` in label
/// values. Every label this module selects on is a bare token our own binaries emit; a value that
/// needed escaping would have to be matched by a real parser. Upgrade path: if a selected label
/// ever carries user input, parse the label list properly.
pub fn sum_of(metric: &str, label: Option<(&str, &str)>, text: &str) -> Option<f64> {
    let mut total = None;
    for line in text.lines() {
        let line = line.trim();
        // `# HELP` / `# TYPE` and blank lines carry no samples.
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((series, rest)) = line.split_once(|c: char| c.is_ascii_whitespace()) else {
            continue;
        };
        let (name, labels) = match series.split_once('{') {
            Some((n, l)) => (n, l.trim_end_matches('}')),
            None => (series, ""),
        };
        if name != metric {
            continue;
        }
        if let Some((k, v)) = label {
            if !labels.contains(&format!("{k}=\"{v}\"")) {
                continue;
            }
        }
        // A sample line may carry a trailing timestamp; the value is always the first token after
        // the series. Untrusted text: an unparsable value is skipped, never a panic.
        let Some(value) = rest.split_whitespace().next().and_then(|v| v.parse::<f64>().ok()) else {
            continue;
        };
        *total.get_or_insert(0.0) += value;
    }
    total
}

// ── one scrape of the fleet ─────────────────────────────────────────────────

/// The counters the rate rules need, summed across every pod that exposes them. Kept as a small
/// map rather than the scraped bodies so the cached previous sample costs a few floats.
#[derive(Clone, Debug, Default)]
pub struct Sample {
    pub counters: BTreeMap<&'static str, f64>,
}

/// `label="value"`, the one selector shape any rule here needs.
type LabelSelector = Option<(&'static str, &'static str)>;

/// `(cache key, metric, label selector)` — one entry per counter any rate rule reads.
const COUNTERS: &[(&str, &str, LabelSelector)] = &[
    ("fence", "db_fence_detected_total", None),
    ("http_total", "http_requests_total", None),
    ("http_5xx", "http_requests_total", Some(("status", "5xx"))),
    ("reconcile_total", "reconciles_total", None),
    ("reconcile_error", "reconciles_total", Some(("result", "error"))),
];

/// Everything a single sweep of the fleet observed. The gauges are read from this scrape only;
/// the counters also go into the cache for the next request's rate window.
#[derive(Clone, Debug, Default)]
pub struct ScrapeSample {
    pub sample: Sample,
    /// Present only if at least one pod exposed it.
    pub leaders: Option<f64>,
    pub max_tunnels: Option<f64>,
    /// `pod: error` for every pod we could not read — its rules go `unknown`, the page still 200s.
    pub failures: Vec<(String, String)>,
    pub pods_scraped: usize,
}

impl ScrapeSample {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Fold one pod's scrape body into the sweep. `pub` so the rule tests can build a canned
    /// sweep the same way a real scrape does, rather than hand-filling the maps.
    pub fn absorb(&mut self, text: &str) {
        for (key, metric, label) in COUNTERS {
            if let Some(v) = sum_of(metric, *label, text) {
                *self.sample.counters.entry(*key).or_insert(0.0) += v;
            }
        }
        if let Some(v) = sum_of("ownership_is_leader", None, text) {
            *self.leaders.get_or_insert(0.0) += v;
        }
        if let Some(v) = sum_of("gateway_open_tunnels", None, text) {
            self.max_tunnels = Some(self.max_tunnels.map_or(v, |m: f64| m.max(v)));
        }
    }
}

// ── the rules ───────────────────────────────────────────────────────────────

#[derive(Debug, serde::Serialize)]
pub struct SignalRow {
    pub alert: &'static str,
    pub state: &'static str,
    /// The catalogue's own "Why" column, verbatim in intent — why this alert exists.
    pub why: &'static str,
    /// What this evaluation actually observed, or why it could not: the numbers behind `state`.
    pub detail: Option<String>,
}

fn row(alert: &'static str, state: &'static str, why: &'static str, detail: Option<String>) -> SignalRow {
    SignalRow { alert, state, why, detail }
}

/// `sum(ownership_is_leader) != 1`. Zero means no claim in the fleet succeeds; two means the epoch
/// check failed and only the fence stands between two writers.
pub fn evaluate_no_leader(sum: f64) -> &'static str {
    if sum == 1.0 {
        "ok"
    } else {
        "firing"
    }
}

/// `increase(db_fence_detected_total[10m]) > 0` — zero is the only acceptable value, so any rise
/// between the two points fires.
pub fn evaluate_fence(before: f64, after: f64) -> &'static str {
    // A counter that went DOWN is a pod restart, not a fence: the delta is meaningless, not zero.
    if after > before {
        "firing"
    } else if after < before {
        "unknown"
    } else {
        "ok"
    }
}

/// The ratio both `Http5xxRate` (>0.05) and `ReconcileErrors` (>0.2) are: bad delta over total
/// delta. `None` when the window carries no traffic at all — nothing observed is not "ok".
pub fn evaluate_ratio(bad: (f64, f64), total: (f64, f64)) -> Option<f64> {
    let (bad, total) = (bad.1 - bad.0, total.1 - total.0);
    // A negative delta on either side is a restart inside the window; the ratio would be noise.
    if total <= 0.0 || bad < 0.0 {
        return None;
    }
    Some(bad / total)
}

/// `max by (pod) (gateway_open_tunnels) > 800`; `MAX_TUNNELS` is 1000 per gateway pod.
pub fn evaluate_tunnels(max: f64) -> &'static str {
    if max > 800.0 {
        "firing"
    } else {
        "ok"
    }
}

const WINDOW_ONLY: &str = "needs a sustained rate window a point-in-time scrape cannot compute";
const NEEDS_NODE_EXPORTER: &str = "needs node-exporter, not deployed";

/// The catalogue, in `deploy/alerts.md`'s order and by its names. `before` is the cached previous
/// counter sample, if this request had one to compute a rate against.
pub fn signal_rows(now: &ScrapeSample, before: Option<&Sample>) -> Vec<SignalRow> {
    let unreadable = || {
        (!now.failures.is_empty()).then(|| {
            format!("{} of {} pods did not answer", now.failures.len(), now.pods_scraped)
        })
    };
    let pair = |key: &str| match (before.and_then(|b| b.counters.get(key)), now.sample.counters.get(key)) {
        (Some(b), Some(a)) => Some((*b, *a)),
        _ => None,
    };

    let leader = match now.leaders {
        Some(sum) => row(
            "NoLeader",
            evaluate_no_leader(sum),
            "Zero: nobody holds the lease, so no claim in the fleet succeeds; two: the epoch check failed and the fence is all that stands between two writers.",
            Some(format!("sum(ownership_is_leader) = {sum}")),
        ),
        None => row(
            "NoLeader",
            "unknown",
            "Zero: nobody holds the lease, so no claim in the fleet succeeds; two: the epoch check failed and the fence is all that stands between two writers.",
            Some(unreadable().unwrap_or_else(|| "no scraped pod exposes ownership_is_leader".into())),
        ),
    };

    let fence = match pair("fence") {
        Some((b, a)) => row(
            "DbFenceDetected",
            evaluate_fence(b, a),
            "The invariant violation: two nodes opened one SlateDB. Zero is the only acceptable value.",
            Some(format!("db_fence_detected_total {b} -> {a}")),
        ),
        None => row(
            "DbFenceDetected",
            "unknown",
            "The invariant violation: two nodes opened one SlateDB. Zero is the only acceptable value.",
            Some("no previous sample to compare against yet".into()),
        ),
    };

    let ratio_row = |alert: &'static str, why: &'static str, bad: &str, total: &str, threshold: f64| {
        match (pair(bad), pair(total)) {
            (Some(b), Some(t)) => match evaluate_ratio(b, t) {
                Some(r) => row(
                    alert,
                    if r > threshold { "firing" } else { "ok" },
                    why,
                    Some(format!("{:.1}% of {} in the window", r * 100.0, t.1 - t.0)),
                ),
                None => row(alert, "unknown", why, Some("no traffic in the window".into())),
            },
            _ => row(alert, "unknown", why, Some("no previous sample to compare against yet".into())),
        }
    };

    let tunnels = match now.max_tunnels {
        Some(max) => row(
            "TunnelSaturation",
            evaluate_tunnels(max),
            "MAX_TUNNELS is 1000 per gateway pod; refusals start with 503 past it.",
            Some(format!("max gateway_open_tunnels = {max}")),
        ),
        // The gateway is a per-region k3s Deployment, not a central pod, so an ad-hoc scrape of
        // this cluster never sees it — absent, not zero.
        None => row(
            "TunnelSaturation",
            "unknown",
            "MAX_TUNNELS is 1000 per gateway pod; refusals start with 503 past it.",
            Some("no scraped pod exposes gateway_open_tunnels".into()),
        ),
    };

    vec![
        leader,
        row(
            "LeaseRenewFailing",
            "unknown",
            "A node that cannot renew loses its leases at the TTL; another node claims, and its warm databases must close.",
            Some(WINDOW_ONLY.into()),
        ),
        fence,
        ratio_row(
            "Http5xxRate",
            "Per listener and route class so a registry outage is not hidden by healthy git traffic.",
            "http_5xx",
            "http_total",
            0.05,
        ),
        row(
            "MisdirectedWrites",
            "unknown",
            "421s during a roll are expected; sustained ones mean the pods disagree about who holds the leader lease.",
            Some(WINDOW_ONLY.into()),
        ),
        ratio_row(
            "ReconcileErrors",
            "A controller in an error loop keeps retrying with backoff; the ratio is what shows it.",
            "reconcile_error",
            "reconcile_total",
            0.2,
        ),
        tunnels,
        row(
            "WorkerHeartbeatStale",
            "unknown",
            "The liveness probe only restarts; this pages when it keeps happening.",
            Some(WINDOW_ONLY.into()),
        ),
        row(
            "PoolAlmostFull",
            "unknown",
            "btrfs past 80% starts failing allocations before df says full.",
            Some(NEEDS_NODE_EXPORTER.into()),
        ),
        row(
            "NodeDiskAlmostFull",
            "unknown",
            "The worker's merge caches and the slatedb object cache live on the root disk.",
            Some(NEEDS_NODE_EXPORTER.into()),
        ),
    ]
}

// ── the handler ─────────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
struct Restarts {
    workload: &'static str,
    /// ponytail: `restartCount` since each pod started, NOT a 1 h window — Kubernetes exposes no
    /// such number, and a true window needs either an events lookup (short retention, noisy) or a
    /// stored baseline. Named `restarts`, and the page says "since the pod started", so the field
    /// asserts no precision it does not have. Upgrade path: sample this into the object store on
    /// the existing beat and diff.
    restarts: i32,
}

#[derive(serde::Serialize)]
struct SignalsResponse {
    signals: Vec<SignalRow>,
    restarts: Vec<Restarts>,
    /// `pod: error` for every pod that did not answer — the page shows which, since the rules that
    /// went `unknown` are only explicable with it.
    scrape_failures: Vec<(String, String)>,
    pods_scraped: usize,
    /// Only when `RUSTIC_GIT_GRAFANA_URL` is set: there is no Grafana in this deployment by
    /// default, and a dead link on a monitoring page is worse than no link.
    #[serde(skip_serializing_if = "Option::is_none")]
    grafana_url: Option<String>,
}

/// The `metrics` endpoint of one pod, or `None` if it is not scrapeable (no IP yet, opted out).
fn metrics_url(p: &Pod) -> Option<String> {
    let ann = p.annotations();
    if ann.get("prometheus.io/scrape").map(String::as_str) != Some("true") {
        return None;
    }
    let ip = p.status.as_ref()?.pod_ip.as_ref()?;
    let port = ann.get("prometheus.io/port").map(String::as_str).unwrap_or(DEFAULT_METRICS_PORT);
    Some(format!("http://{ip}:{port}/metrics"))
}

/// Every pod, concurrently, each bounded by `SCRAPE_TIMEOUT`. A pod that fails is recorded and
/// skipped — the rules it would have contributed to go `unknown`, and the page still answers 200.
async fn scrape(client: &reqwest::Client, pods: &[(String, String)]) -> ScrapeSample {
    let bodies = futures::future::join_all(pods.iter().map(|(name, url)| async move {
        let r = client.get(url).timeout(SCRAPE_TIMEOUT).send().await;
        let text = match r {
            Ok(r) if r.status().is_success() => r.text().await.map_err(|e| e.to_string()),
            Ok(r) => Err(format!("HTTP {}", r.status())),
            Err(e) => Err(e.to_string()),
        };
        (name.clone(), text)
    }))
    .await;

    let mut out = ScrapeSample { pods_scraped: bodies.len(), ..Default::default() };
    for (name, body) in bodies {
        match body {
            Ok(text) => out.absorb(&text),
            Err(e) => out.failures.push((name, e)),
        }
    }
    out
}

pub(crate) async fn signals(State(s): State<Arc<ApiState>>) -> Result<Response, Response> {
    let client = aks(&s)?;
    let pods = Api::<Pod>::namespaced(client.clone(), "rustic-git")
        .list(&ListParams::default())
        .await
        .map_err(kube_err)?;

    let targets: Vec<(String, String)> =
        pods.iter().filter_map(|p| Some((p.name_any(), metrics_url(p)?))) .collect();

    // Pods that ARE ours but have no IP yet (or opted out) still belong in the failure list: their
    // metrics are missing from every sum below, and silently dropping them would make a partial
    // scrape look complete.
    let mut skipped: Vec<(String, String)> = pods
        .iter()
        .filter(|p| metrics_url(p).is_none())
        .map(|p| (p.name_any(), "no pod IP or not annotated for scraping".to_string()))
        .collect();

    let http = reqwest::Client::new();
    let now = scrape(&http, &targets).await;

    // The rate window: reuse a cached sample when it is recent enough, otherwise pay 5 s for a
    // second point. ponytail: single-process, in-memory cache — a restart just means the first
    // request after boot reports the rate rules `unknown` until a second sample exists.
    let cached = {
        let g = s.metrics_sample.lock().unwrap_or_else(|p| p.into_inner());
        g.as_ref().and_then(|(at, sample)| (at.elapsed() <= SAMPLE_MAX_AGE).then(|| sample.clone()))
    };
    let (before, now) = match cached {
        Some(b) => (Some(b), now),
        // Nothing answered, so a second point would compare two empty sums: skip the 5 s wait.
        None if targets.is_empty() => (None, now),
        None => {
            tokio::time::sleep(RATE_WINDOW).await;
            let second = scrape(&http, &targets).await;
            (Some(now.sample.clone()), second)
        }
    };
    *s.metrics_sample.lock().unwrap_or_else(|p| p.into_inner()) = Some((Instant::now(), now.sample.clone()));

    let mut all = now;
    all.failures.append(&mut skipped);
    all.pods_scraped = pods.items.len();

    let restarts = crate::api::workloads::KNOWN_CENTRAL
        .iter()
        .map(|(workload, _)| Restarts {
            workload,
            restarts: pods
                .iter()
                // Pod names are `{workload}-…` for both a Deployment's ReplicaSet and a
                // StatefulSet's ordinal, which ties a pod to the KNOWN entry without needing a
                // per-workload label selector (the server tier's labels differ from the rest).
                .filter(|p| p.name_any().starts_with(&format!("{workload}-")))
                .flat_map(|p| p.status.iter())
                .flat_map(|st| st.container_statuses.iter().flatten())
                .map(|c| c.restart_count)
                .sum(),
        })
        .collect();

    let signals = signal_rows(&all, before.as_ref());
    Ok(Json(SignalsResponse {
        signals,
        restarts,
        scrape_failures: all.failures,
        pods_scraped: all.pods_scraped,
        grafana_url: std::env::var("RUSTIC_GIT_GRAFANA_URL").ok().filter(|u| !u.is_empty()),
    })
    .into_response())
}
