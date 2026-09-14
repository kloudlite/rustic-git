//! Leader election for the cluster controller, on a `coordination.k8s.io/v1` Lease.
//!
//! The same semantics as `crates/storage/src/ownership/lease.rs` — "the store is the arbiter,
//! never the clock and never the ordinal" — with the API server's resourceVersion CAS playing
//! `UpdateVersion` and `leaseTransitions` playing the epoch. Not that module, because it needs an
//! `ObjectStore`: an S3 or Azure credential in a process whose whole design is "the API server is
//! the only dependency", and a second failure domain for no gain.
//!
//! The epoch is the FENCING TOKEN, and it is the thing not to lose. Every write checks the epoch
//! it was elected under (`may_write`); a write that finds a newer `leaseTransitions` demotes
//! rather than finishing. Unlike SlateDB there is no writer fence underneath, so each object's own
//! CAS (a server-side apply under our field manager) is the backstop.

use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use k8s_openapi::jiff::Timestamp;
use kube::api::{ObjectMeta, PostParams};
use kube::{Api, ResourceExt};
use std::time::Duration;

/// One object, one name, one namespace: the cluster's controller lease.
pub const LEASE_NAME: &str = "kloudlite-controller";
pub const LEASE_NAMESPACE: &str = "kube-system";

/// Expiry. A holder that stops renewing is replaced this long after its last `renewTime`, so a
/// crashed controller costs one TTL of convergence and nothing else — every object it owns is
/// level-triggered and already applied.
pub const TTL: Duration = Duration::from_secs(15);
/// Three renews per TTL, `LEADER_RENEW`'s rule: two may be lost to a blip without a handover.
pub const RENEW: Duration = Duration::from_secs(5);

/// What the object says, reduced to the three fields the decision reads.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub holder: String,
    pub transitions: u32,
    pub renewed_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Acquire { epoch: u32 },
    Renew { epoch: u32 },
    Wait,
}

/// The whole state machine, clock injected. Every branch is a test below.
pub fn decide(now_ms: u64, me: &str, cur: Option<&View>) -> Step {
    let ttl = TTL.as_millis() as u64;
    match cur {
        None => Step::Acquire { epoch: 1 },
        Some(c) => {
            // Expiry computed by ADDING the TTL to the renew time, never by subtracting it from
            // now: a holder whose renewTime is in the future (a skewed peer) must read as live,
            // and an underflowing subtraction would read it as expired an epoch ago and steal it.
            let live = now_ms <= c.renewed_ms.saturating_add(ttl);
            match (live, c.holder == me) {
                // Ours and live: same epoch. Advancing it here would fence our own writes.
                (true, true) => Step::Renew { epoch: c.transitions },
                (true, false) => Step::Wait,
                // Expired, ours or not: a takeover is a takeover, and the epoch advances so the
                // previous holder's next write demotes.
                (false, _) => Step::Acquire { epoch: c.transitions.saturating_add(1) },
            }
        }
    }
}

/// May a write made under `epoch` still land? Read immediately before the write.
pub fn may_write(epoch: u32, cur: Option<&View>) -> bool {
    cur.is_some_and(|c| c.transitions == epoch)
}

pub fn view(l: &Lease) -> Option<View> {
    let s = l.spec.as_ref()?;
    Some(View {
        holder: s.holder_identity.clone().unwrap_or_default(),
        // A Lease written without it is epoch 0; `decide` advances from there like any other.
        // `.max(0)` before the cast: the field is an i32 on the wire and anything may have written
        // it, and a negative one would wrap to a `u32` near the ceiling — an epoch no real term
        // could ever reach, fencing every write we make.
        transitions: s.lease_transitions.unwrap_or(0).max(0) as u32,
        renewed_ms: s
            .renew_time
            .as_ref()
            .map(|t| t.0.as_millisecond().max(0) as u64)
            .unwrap_or(0),
    })
}

pub async fn read(api: &Api<Lease>) -> kube::Result<Option<Lease>> {
    api.get_opt(LEASE_NAME).await
}

/// Write the step. Pinned to the resourceVersion we read (`replace`), so two pods that both
/// decided `Acquire` from the same read cannot both win — the loser gets a 409 and waits.
pub async fn write(
    api: &Api<Lease>,
    me: &str,
    step: &Step,
    cur: Option<&Lease>,
    now: Timestamp,
) -> kube::Result<Option<Lease>> {
    // `Wait` never reaches here — `elect` decides it without writing — and answering `None` rather
    // than echoing `cur` means a caller that ever did reach it demotes instead of being handed back
    // a term it does not hold.
    let (Step::Acquire { epoch } | Step::Renew { epoch }) = step else { return Ok(None) };
    let epoch = *epoch;
    let spec = LeaseSpec {
        holder_identity: Some(me.to_string()),
        lease_duration_seconds: Some(TTL.as_secs() as i32),
        lease_transitions: Some(epoch as i32),
        renew_time: Some(MicroTime(now)),
        // A Renew is the SAME term continuing, so it carries the term's own acquireTime forward;
        // rebuilding the spec from `Default` would blank it every five seconds and lose the one
        // field that says when this leadership began.
        acquire_time: match step {
            Step::Acquire { .. } => Some(MicroTime(now)),
            _ => cur.and_then(|l| l.spec.as_ref()).and_then(|s| s.acquire_time.clone()),
        },
        ..Default::default()
    };
    match cur {
        Some(existing) => {
            let mut next = existing.clone();
            next.spec = Some(spec);
            // `replace` and not a patch: the resourceVersion the object carries IS the CAS, and a
            // merge patch would happily overwrite a term that moved under us.
            match api.replace(LEASE_NAME, &PostParams::default(), &next).await {
                Ok(l) => Ok(Some(l)),
                // Somebody else won this round. Not an error: re-read on the next tick.
                Err(kube::Error::Api(e)) if e.code == 409 => Ok(None),
                Err(e) => Err(e),
            }
        }
        None => {
            let obj = Lease {
                metadata: ObjectMeta {
                    name: Some(LEASE_NAME.into()),
                    namespace: Some(LEASE_NAMESPACE.into()),
                    ..Default::default()
                },
                spec: Some(spec),
            };
            match api.create(&PostParams::default(), &obj).await {
                Ok(l) => Ok(Some(l)),
                Err(kube::Error::Api(e)) if e.code == 409 => Ok(None),
                Err(e) => Err(e),
            }
        }
    }
}

/// Release on shutdown so a rolling replacement is elected immediately instead of waiting out the
/// TTL: blank the holder, keep the epoch. Best effort — a pod that dies hard is the TTL's case.
///
/// CAS'd on the object we just read, exactly like `write`, and NOT a merge patch: leadership can
/// turn over inside the read-check-write window, and an unfenced patch would blank the NEW holder's
/// identity and renewTime — discarding a perfectly valid term and forcing a takeover at
/// `transitions + 1`. A 409 means somebody else already owns it, which is the outcome we wanted.
pub async fn release(api: &Api<Lease>, me: &str) {
    let Ok(Some(cur)) = read(api).await else { return };
    if view(&cur).is_some_and(|v| v.holder != me) {
        return;
    }
    let mut next = cur.clone();
    if let Some(spec) = next.spec.as_mut() {
        spec.holder_identity = Some(String::new());
        spec.renew_time = None;
    }
    match api.replace(LEASE_NAME, &PostParams::default(), &next).await {
        Ok(_) => {}
        Err(kube::Error::Api(e)) if e.code == 409 => {}
        Err(e) => tracing::warn!(lease = %cur.name_any(), error = %e, "leader.release.failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kloudlite_workspaces::kube_test;

    const TTL_MS: u64 = TTL.as_millis() as u64;
    fn v(holder: &str, transitions: u32, renewed_ms: u64) -> View {
        View { holder: holder.into(), transitions, renewed_ms }
    }

    /// No object at all: the first pod to look takes it, and the epoch starts at 1 so "epoch 0"
    /// can never be a real elected term.
    #[test]
    fn an_absent_lease_is_acquired_at_epoch_one() {
        assert_eq!(decide(1_000, "ctl-a", None), Step::Acquire { epoch: 1 });
    }

    /// Ours and live: renew under the SAME epoch. A renew that advanced the epoch would fence
    /// our own in-flight writes every five seconds.
    #[test]
    fn our_own_live_lease_is_renewed_under_the_same_epoch() {
        let cur = v("ctl-a", 3, 10_000);
        assert_eq!(decide(10_000 + TTL_MS - 1, "ctl-a", Some(&cur)), Step::Renew { epoch: 3 });
    }

    /// Somebody else's and still live: wait. This is what the TTL MEANS — never the clock's
    /// opinion of who is healthier.
    #[test]
    fn a_live_lease_held_by_another_pod_is_never_taken() {
        let cur = v("ctl-b", 3, 10_000);
        assert_eq!(decide(10_000 + TTL_MS - 1, "ctl-a", Some(&cur)), Step::Wait);
    }

    /// Expired on the TAKER's own read: take it, epoch advanced, which is what demotes the old
    /// holder's next write.
    #[test]
    fn an_expired_lease_is_taken_with_the_next_epoch() {
        let cur = v("ctl-b", 3, 10_000);
        assert_eq!(decide(10_000 + TTL_MS + 1, "ctl-a", Some(&cur)), Step::Acquire { epoch: 4 });
    }

    /// Our own, expired (a long stall, a paused process): re-acquire rather than renew — the
    /// epoch must advance, because any other pod was free to take it during the gap.
    #[test]
    fn our_own_expired_lease_is_re_acquired_not_renewed() {
        let cur = v("ctl-a", 3, 10_000);
        assert_eq!(decide(10_000 + TTL_MS + 1, "ctl-a", Some(&cur)), Step::Acquire { epoch: 4 });
    }

    /// Exactly at the boundary the lease is still LIVE: the expiry is `renewed + duration`, and
    /// treating `==` as expired would let two pods take it in the same millisecond.
    #[test]
    fn the_expiry_boundary_is_still_held() {
        let cur = v("ctl-b", 1, 10_000);
        assert_eq!(decide(10_000 + TTL_MS, "ctl-a", Some(&cur)), Step::Wait);
    }

    /// A holder whose `renewTime` is in the FUTURE (a skewed peer) is treated as live, never as
    /// expired: a subtraction that underflowed would read as "expired long ago" and steal it.
    #[test]
    fn a_future_renew_time_reads_as_live() {
        let cur = v("ctl-b", 1, 20_000);
        assert_eq!(decide(10_000, "ctl-a", Some(&cur)), Step::Wait);
    }

    /// An epoch check refuses a write made under a term that has since ended.
    #[test]
    fn a_demoted_epoch_may_not_write() {
        assert!(may_write(4, Some(&v("ctl-a", 4, 0))));
        assert!(!may_write(3, Some(&v("ctl-b", 4, 0))));
        assert!(!may_write(3, None));
    }

    const PATH: &str = "/apis/coordination.k8s.io/v1/namespaces/kube-system/leases/kloudlite-controller";

    fn lease_json(holder: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "coordination.k8s.io/v1",
            "kind": "Lease",
            "metadata": { "name": LEASE_NAME, "namespace": LEASE_NAMESPACE, "resourceVersion": "7" },
            "spec": { "holderIdentity": holder, "leaseTransitions": 4, "renewTime": "2026-09-14T00:00:00.000000Z" },
        })
    }

    /// Release is CAS'd, not patched: leadership that turned over inside the read-check-write
    /// window answers 409, and nothing of ours lands on the new holder's term.
    #[tokio::test]
    async fn a_release_that_loses_the_cas_writes_nothing() {
        let (client, rec) = kube_test::mock_client(vec![
            kube_test::get(PATH, lease_json("ctl-a")),
            kube_test::conflict("PUT", PATH),
        ]);
        release(&Api::namespaced(client, LEASE_NAMESPACE), "ctl-a").await;
        // One CAS attempt, no merge patch anywhere — a PATCH here would be the unfenced write.
        assert_eq!(rec.calls(), vec![format!("GET {PATH}"), format!("PUT {PATH}")]);
    }

    /// Somebody else already holds it: don't even try to write.
    #[tokio::test]
    async fn a_release_by_a_pod_that_no_longer_holds_it_is_a_no_op() {
        let (client, rec) =
            kube_test::mock_client(vec![kube_test::get(PATH, lease_json("ctl-b"))]);
        release(&Api::namespaced(client, LEASE_NAMESPACE), "ctl-a").await;
        assert_eq!(rec.calls(), vec![format!("GET {PATH}")]);
    }
}
