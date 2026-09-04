//! The wake protocol: "something you replicate just changed, pull now." A wake can only make a
//! pass happen SOONER, never change what it pulls — which is why the handler trusts nothing past
//! the secret, and why the floor (`MIN_WAKE_GAP`) is the whole defence against a peer driving
//! this node's beat.

use super::pull::{agent_pod_addr, peer_http_client, replica_interval};
use crate::controller::Ctx;
use std::sync::Arc;
use std::time::Duration;

/// POST `/peer/v1/wake` to every placeable node but me, ALL AT ONCE. Serially, one dead peer cost
/// the caller its full timeout before the next was even dialled, so a stop behind N unreachable
/// nodes stalled N x 5 s; concurrently the whole fan-out is bounded by the slowest single node.
/// Every failure is a warn and never an error: the wake is an optimisation on top of the ticker,
/// and a stop that failed because a peer was unreachable would be strictly worse than a stop that
/// replicates a beat later.
///
/// The secret is a parameter, not an env read: the callers already hold one (`Ctx::peer_secret`,
/// read once at boot) and a function that reads process env is a function whose tests must write
/// process env. Tests therefore pass their own without touching the process.
pub async fn wake_peers(ctx: &Arc<Ctx>, live: &[String], secret: &str) {
    if secret.is_empty() {
        return; // fail-closed, same rule as every other dial in this file
    }
    let Ok(http) = peer_http_client() else { return };
    let dials = live.iter().filter(|n| *n != &ctx.node).map(|node| {
        let (http, secret) = (&http, &secret);
        async move {
            let addr = match agent_pod_addr(&ctx.client, node).await {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!(%node, error = %e, "peer.addr.failed");
                    return;
                }
            };
            let url = format!("http://{addr}/peer/v1/wake");
            match http.post(&url).header("x-peer-secret", *secret).timeout(Duration::from_secs(5)).send().await {
                Ok(r) if r.status().is_success() => {}
                Ok(r) => tracing::warn!(%node, status = %r.status(), reason = "refused", "wake.failed"),
                Err(e) => tracing::warn!(%node, reason = "unreachable", error = %e, "wake.failed"),
            }
        }
    });
    futures::future::join_all(dials).await;
}

/// What `spawn_pull` does when a pass ends: the coalescing rule, lifted out of the loop so it can
/// be tested without a clock. `notify_one` leaves at most ONE permit however many wakes arrived, so
/// taking it here without waiting turns a burst into exactly one extra pass.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Next {
    RunAgain,
    /// Something could not be fetched: come back after this long rather than at the next tick, and
    /// still take a wake the moment one arrives.
    RetrySoon(Duration),
    Wait,
}

/// The FIRST retry delay after a missed pass. Short enough that a source coming back is picked up
/// while the person is still watching, long enough not to hammer a peer that is simply down.
pub(crate) const RETRY_SOON: Duration = Duration::from_secs(30);

/// `RETRY_SOON` doubled per CONSECUTIVE missed pass, capped at the ordinary tick. Without the cap
/// a permanently unfetchable snapshot — a Snapshot whose only source is gone for good — pinned every
/// node placed on that volume at a 30 s beat forever, node-wide: the flag is per-PASS, so one
/// stuck volume paid the whole node's listing cost every 30 s until someone deleted the CR.
/// Capping at `replica_interval` makes the worst case exactly today's steady state.
pub(crate) fn retry_delay(misses: u32, settings: &crate::controller::Settings) -> Duration {
    RETRY_SOON.saturating_mul(1u32 << misses.saturating_sub(1).min(16)).min(replica_interval(settings))
}

/// The minimum gap between the STARTS of two wake-driven passes. `/peer/v1/wake` is
/// unauthenticated beyond a fleet-wide symmetric secret, and a peer POSTing it in a loop otherwise
/// pins this node in a back-to-back beat — six cluster-wide LISTs plus a Snapshot LIST and a
/// directory walk per interesting volume, forever. Five seconds keeps a stop or a clone
/// effectively immediate while capping one compromised or buggy agent's reach into the API server.
pub(crate) const MIN_WAKE_GAP: Duration = Duration::from_secs(5);

/// `misses` counts CONSECUTIVE passes that missed something, and is reset by any clean pass — a
/// volume that starts fetching again returns the node to its ordinary beat immediately.
///
/// `since_last_start` is measured from when the pass that just ended BEGAN, not from when it
/// ended: a slow pass has already paid the floor, and measuring from the end would let a long
/// receive earn an extra idle 5 s it does not need.
pub(crate) fn after_pass(
    wake: &tokio::sync::Notify,
    missed: bool,
    misses: &mut u32,
    since_last_start: Duration,
    settings: &crate::controller::Settings,
) -> Next {
    use futures::FutureExt;
    *misses = if missed { misses.saturating_add(1) } else { 0 };
    let woken = wake.notified().now_or_never().is_some();
    let backoff = missed.then(|| retry_delay(*misses, settings));
    match (woken, backoff) {
        // A wake inside the floor keeps its permit's effect — the pass still happens — but only
        // after the remainder. `RetrySoon` is right and `Wait` is not: the wake must not be lost.
        (true, None) if since_last_start < MIN_WAKE_GAP => Next::RetrySoon(MIN_WAKE_GAP - since_last_start),
        (true, None) => Next::RunAgain,
        // A missed pass's own backoff is always at least `RETRY_SOON` (30 s), which is longer than
        // the floor: the floor can never shorten it, and a wake during it is taken by the select.
        (_, Some(d)) => Next::RetrySoon(d),
        (false, None) => Next::Wait,
    }
}

