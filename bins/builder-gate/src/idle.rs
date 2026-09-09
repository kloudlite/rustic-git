//! When to stop a builder.
//!
//! Idle is counted in CONNECTIONS, not in builds: buildkit's own notion of a build is inside the
//! gRPC stream the gate deliberately does not parse, and a connection open is the one honest
//! signal that somebody is still using it. A build that finishes leaves buildx's connection
//! closed within seconds, so the two agree in practice and the gate stays a byte pump.

use crate::Gate;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

/// The tick. Coarse on purpose: the shortest `builder_idle_secs` is 60, and a stop that lands a
/// few seconds late costs nothing.
const TICK: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Slot {
    open: u64,
    /// `Some` only while nothing is connected. A new connection clears it, which is what makes an
    /// arrival during the countdown cancel the stop rather than merely delay it.
    zero_since: Option<Instant>,
    /// One stop per idle period. Without this the beat would re-POST every tick for as long as
    /// the builder stayed idle, and each of those is a write the api has to serve.
    stopped: bool,
}

#[derive(Default)]
pub struct Idle(Mutex<HashMap<String, Slot>>);

impl Idle {
    pub fn opened(&self, slug: &str) {
        let mut m = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let s = m.entry(slug.to_string()).or_default();
        s.open += 1;
        s.zero_since = None;
        // Re-armed: this builder can be stopped again once THIS connection goes.
        s.stopped = false;
    }

    pub fn closed(&self, slug: &str) {
        let mut m = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(s) = m.get_mut(slug) {
            s.open = s.open.saturating_sub(1);
            if s.open == 0 {
                s.zero_since = Some(Instant::now());
            }
        }
    }

    /// A builder the api says is running but that this process has never seen a connection for —
    /// the gate restarted. Seeded as idle-from-now, so a restart costs at most one idle period
    /// rather than leaving a builder up until somebody happens to build again.
    pub fn seed(&self, slug: &str) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(slug.to_string())
            .or_insert_with(|| Slot { open: 0, zero_since: Some(Instant::now()), stopped: false });
    }

    /// Every slug idle for long enough, marked stopped as it is returned — so the caller's POST
    /// happens exactly once per idle period even if it fails.
    fn due(&self, after: Duration) -> Vec<String> {
        let now = Instant::now();
        let mut m = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut due = Vec::new();
        for (slug, s) in m.iter_mut() {
            if s.open == 0 && !s.stopped && s.zero_since.is_some_and(|z| now.duration_since(z) >= after) {
                s.stopped = true;
                due.push(slug.clone());
            }
        }
        due
    }
}

pub async fn beat(gate: Arc<Gate>) {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let after = Duration::from_secs(gate.central.load().builder_idle_secs);
        for slug in gate.idle.due(after) {
            match gate.api.stop(&slug).await {
                Ok(()) => tracing::info!(%slug, "gate.builder.stopped"),
                // Left marked stopped: the next connection re-arms it, and retrying a failed stop
                // every five seconds would hammer the api for a builder nobody is waiting on.
                Err(e) => tracing::warn!(%slug, error = %e, "gate.stop.failed"),
            }
        }
    }
}
