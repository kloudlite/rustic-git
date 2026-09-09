//! The ownership-map writer is ELECTED: every node ticks `election_tick`, the holder of the
//! `cluster/leader` lease opens the map as writer, a fenced write demotes, and a draining node
//! hands its lease and its repos over before it exits. Followers reach the leader through
//! `ask_leader`. See the project guide's "The one invariant everything hangs off".

use super::*;

impl App {
    pub fn leader(&self) -> Option<String> {
        self.leader.lock().unwrap().clone()
    }

    pub fn set_leader(&self, node: Option<&str>) {
        *self.leader.lock().unwrap() = node.map(str::to_string);
    }

    /// Leading means holding an epoch. Nothing here is derived from a name: two nodes cannot
    /// both hold one, because the store hands the lease to exactly one put.
    pub fn is_leader(&self) -> bool {
        self.leader_epoch() != 0
    }

    pub fn leader_epoch(&self) -> u64 {
        self.leader_epoch.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// A live leader exists: this node, or a lease read within `LEADER_TTL` that has not expired.
    /// What `/healthz` gates readiness on — a node that knows nobody who can grant cannot take a
    /// cold repo, and must not take traffic. Cached reads; the probe costs nothing.
    pub fn leader_live(&self) -> bool {
        if self.is_leader() {
            return true;
        }
        use std::sync::atomic::Ordering::Relaxed;
        let now = self.now_ms();
        now.saturating_sub(self.lease_seen_ms.load(Relaxed)) < LEADER_TTL.as_millis() as u64
            && now < self.lease_expires_ms.load(Relaxed)
    }

    pub(super) fn note_live(&self, l: &Lease) {
        use std::sync::atomic::Ordering::Relaxed;
        self.set_leader(Some(&l.node));
        self.lease_seen_ms.store(self.now_ms(), Relaxed);
        self.lease_expires_ms.store(l.expires_ms, Relaxed);
    }

    /// One beat of the election, run every `LEADER_RENEW` on every fleet node (and once at boot).
    ///
    /// Read the lease. If it names me and is live, renew it — the store refuses a renewal pinned
    /// to a version somebody else has since overwritten, which is how "my renewal raced an expiry"
    /// resolves: by the store's answer, not by our clock. If it names somebody else and is live,
    /// follow them, and stop leading if we thought we did. If it is absent or expired, try to take
    /// it with the next epoch; exactly one candidate's put lands, and the rest read the winner on
    /// their next tick. Solo mode has no store to read and returns at once.
    pub async fn election_tick(&self) -> Result<()> {
        let Some(os) = self.ownership.object_store() else { return Ok(()) };
        // A draining pod resigns and never leads again: `drain` sets `retiring` through `resign`,
        // and this is the beat that would otherwise take a lease it is about to hand back.
        if self.retiring.load(std::sync::atomic::Ordering::Relaxed) || self.is_draining() {
            return Ok(());
        }
        let now = self.now_ms();
        // Our own expiry is honoured before the store is consulted at all: the read below can
        // fail (or hang and fail) for as long as the store is unreachable, and a leader that
        // keeps granting past the expiry a peer is already counting down is the two-writer bug.
        if self.is_leader() && now >= self.held_expires_ms.load(std::sync::atomic::Ordering::Relaxed) {
            self.demote("our own lease expired").await;
        }
        let cur = lease::read(os.as_ref()).await?;
        match cur {
            Some(c) if c.lease.node == self.self_name && !lease::is_expired(&c.lease, now) => {
                match lease::renew(os.as_ref(), &c, now).await? {
                    // `promote` is idempotent: on the beat after winning this only refreshes the
                    // expiry we cache; after a restart within one TTL it resumes our own lease.
                    Some(h) => self.promote(h).await?,
                    None => self.demote("renewal refused by the store").await,
                }
            }
            Some(c) if !lease::is_expired(&c.lease, now) => {
                if self.is_leader() {
                    self.demote(&format!("{} holds the lease at epoch {}", c.lease.node, c.lease.epoch)).await;
                }
                self.note_live(&c.lease);
            }
            c => {
                if let Some(h) = lease::take(os.as_ref(), &self.self_name, now, c.as_ref()).await? {
                    self.promote(h).await?;
                }
            }
        }
        // One gauge per pod; the alert is `sum(...) != 1`.
        metrics::gauge!("ownership_is_leader").set(if self.is_leader() { 1.0 } else { 0.0 });
        Ok(())
    }

    /// Hold the lease `h` names: open the writer FIRST, then publish the epoch. A grant that sees
    /// the epoch must find a writer behind it. Opening fences any previous writer of the map, so a
    /// stale leader that has not yet noticed losing the lease cannot write.
    ///
    /// Under `leader_lock`, like `demote_locked`: a fence-demote from a grant must not interleave
    /// with this and leave a non-zero epoch published over a reader.
    pub(super) async fn promote(&self, h: Held) -> Result<()> {
        let _g = self.leader_lock.lock().await;
        // `resign` may have set this after the tick's own check — a released lease is exactly what
        // the take above grabs. Give it back rather than lead for one beat on the way out.
        if self.retiring.load(std::sync::atomic::Ordering::Relaxed) {
            if let Some(os) = self.ownership.object_store() {
                let _ = lease::release(os.as_ref(), &h).await;
            }
            return Ok(());
        }
        // A lease we cannot use lapses on its own TTL and somebody else takes it; leading with a
        // reader would grant nothing anyway.
        //
        // The open replays the map's WAL and has been measured at 146s (see `OwnershipStore::
        // promote`), many times LEADER_TTL — and the election beat is sequential, so nothing else
        // renews meanwhile. Renew from inside the wait instead: same held version chain as the
        // beat, so the store still arbitrates. A refusal means somebody else holds the lease and
        // may already be opening the writer; abandon the promotion rather than finish an open that
        // would fence them.
        let mut cur = h;
        let open = self.ownership.promote();
        tokio::pin!(open);
        let outcome = loop {
            tokio::select! {
                r = &mut open => break r.map(|()| true),
                _ = tokio::time::sleep(lease::LEADER_RENEW) => {
                    let Some(os) = self.ownership.object_store() else { continue };
                    match lease::renew(os.as_ref(), &cur, self.now_ms()).await {
                        Ok(Some(h2)) => cur = h2,
                        Ok(None) => break Ok(false),
                        Err(e) => break Err(e),
                    }
                }
            }
        };
        match outcome {
            Ok(true) => {}
            // Dropping `open` cancels the build; `demote_locked` closes any writer that did land
            // and clears an epoch we held before this promotion.
            Ok(false) => {
                self.demote_locked("lease lost while opening the writer").await;
                return Ok(());
            }
            Err(e) => {
                self.demote_locked("renewal failed while opening the writer").await;
                return Err(e);
            }
        }
        let fresh = self.leader_epoch() != cur.lease.epoch;
        self.leader_epoch.store(cur.lease.epoch, std::sync::atomic::Ordering::Relaxed);
        // The version and expiry carried out of the renewals above, not the one we came in with.
        self.held_expires_ms.store(cur.lease.expires_ms, std::sync::atomic::Ordering::Relaxed);
        self.note_live(&cur.lease);
        if fresh {
            tracing::info!(epoch = cur.lease.epoch, "lease.acquired");
        }
        Ok(())
    }

    /// Stop leading: epoch to zero under `leader_lock` — so no grant is mid-write when the writer
    /// goes — then close the writer and follow the map again. Called for a refused renewal, a
    /// lease read that names somebody else, and a fenced map write.
    pub async fn demote(&self, why: &str) {
        let _g = self.leader_lock.lock().await;
        self.demote_locked(why).await;
    }

    /// Shutdown: give the leader lease back, then demote. Without the release the fleet is
    /// writerless for up to `LEADER_TTL` on every leader roll; with it the next tick anywhere
    /// takes over. Best-effort — a release the store refuses or cannot reach falls back to the
    /// TTL, which is slower but never wrong — and idempotent, so both exit paths may call it.
    pub async fn resign(&self) {
        self.retiring.store(true, std::sync::atomic::Ordering::Relaxed);
        if let (true, Some(os)) = (self.is_leader(), self.ownership.object_store()) {
            let r = match lease::read(os.as_ref()).await {
                Ok(Some(c)) if c.lease.node == self.self_name => lease::release(os.as_ref(), &c).await.map(|_| ()),
                Ok(_) => Ok(()),
                Err(e) => Err(e),
            };
            if let Err(e) = r {
                tracing::warn!(error = %e, "lease.release.failed");
            }
        }
        self.demote("shutdown").await;
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Claim the drain: `true` if one was already running or done. A `swap`, not a read then a
    /// write — two preStop hooks (or a hand-run curl beside one) would otherwise both pass the
    /// check and run two handovers over each other.
    pub fn begin_draining(&self) -> bool {
        self.draining.swap(true, std::sync::atomic::Ordering::SeqCst)
    }

    /// Hand every repo this pod owns to a live peer, before the process stops answering.
    ///
    /// This is what a roll used to pay for in 502s: SIGTERM released the leases and the repos sat
    /// unowned until some node's next claim — up to a lease cycle, and longer if the dying pod was
    /// the map's writer. Handing them over instead means the map names the new owner while this
    /// pod is still answering, so a follower's next read routes there rather than into a socket
    /// that has gone.
    ///
    /// Order, and each step is load-bearing:
    /// 1. `draining` first, so nothing claims a new repo behind the handover's back.
    /// 2. resign the lease. A draining writer cannot hand anything over — every reassignment is a
    ///    map write, and the writer is about to close. `resign` is idempotent and also stops the
    ///    election beat retaking it; then wait for somebody else to take it, bounded.
    /// 3. per repo: release our entry, then CLOSE the local database (after the same drain a
    ///    retire takes, so a request in flight here finishes), and only then name the peer.
    ///
    /// That last order is the one that was wrong first time round. Naming the peer while this
    /// pod still holds the handle open lets the peer open it and FENCE us, and a fenced request
    /// here answers 503 — which git does not retry, i.e. exactly the sample this exists to
    /// remove. Closing first costs nothing the goal cares about: `release` has already deleted
    /// the entry, so for the length of the window a follower sees an unowned repo either way,
    /// which is the pre-change behaviour and is handled (421, then the recovery path).
    ///
    /// Returns `(moved, kept)`. A repo is `kept` when the leader refuses to grant it away — the
    /// entry is put back to us and the ordinary SIGTERM release deals with it — because the one
    /// thing that must never happen is reporting a repo handed over while the map still names a
    /// pod that has closed it.
    ///
    /// Bounded twice: `PER_REPO` on each handover, so one unreachable-leader repo cannot eat the
    /// budget for the other fifteen, and `DRAIN_BUDGET` overall, after which the rest are left as
    /// they are — still ours, still released by the SIGTERM path.
    pub async fn drain(&self) -> (usize, usize) {
        self.begin_draining();
        let deadline = std::time::Instant::now() + DRAIN_BUDGET;
        let was_leader = self.is_leader();
        self.resign().await;
        if was_leader {
            self.await_new_leader().await;
        }
        let (mut moved, mut kept) = (0, 0);
        for repo in self.store.pool.warm_repos() {
            if std::time::Instant::now() >= deadline {
                tracing::warn!(repo = %repo, "ownership.handover.deadline");
                break;
            }
            match tokio::time::timeout(PER_REPO, self.hand_over(&repo)).await {
                Ok(true) => moved += 1,
                Ok(false) => kept += 1,
                // Out of time mid-handover: the entry may be anything, so make it nothing. An
                // unowned repo is claimed by whoever is asked next; one naming a pod that is
                // leaving is a dead end for a whole LEASE_TTL.
                Err(_) => {
                    tracing::warn!(repo = %repo, timeout_s = PER_REPO.as_secs(), "ownership.handover.timeout");
                    let _ = tokio::time::timeout(PER_REPO, self.release(&repo)).await;
                }
            }
        }
        (moved, kept)
    }

    /// One repo's handover. `true` when a live peer now owns it in the map.
    pub(super) async fn hand_over(&self, repo: &str) -> bool {
        let target = self.handover_target(repo).await;
        if let Err(e) = self.release(repo).await {
            // The entry still names us and is live, so the peer would be refused anyway. Close and
            // keep it: the SIGTERM release is the fallback, exactly as before this existed.
            tracing::warn!(repo = %repo, reason = "drain", error = %e, "ownership.release.failed");
            self.close_local(repo).await;
            return false;
        }
        self.close_local(repo).await;
        let Some(target) = target else {
            // Nobody to hand it to. The entry is already gone, so whichever node is asked next
            // claims it — the pre-change behaviour, and never this pod.
            tracing::warn!(repo = %repo, "ownership.handover.nopeer");
            return false;
        };
        match self.claim_for(repo, &target).await {
            Ok(Grant::Granted(_)) => true,
            // Refused, or the leader could not be reached: the move did NOT happen. Take the repo
            // back so the map names a node that is at least still answering until SIGTERM releases
            // it; a `HeldBy` naming a third node is already owned, and the re-claim is refused in
            // its turn, which is the right answer too.
            other => {
                match other {
                    Ok(g) => tracing::warn!(repo = %repo, peer = %target, grant = ?g, "ownership.handover.refused"),
                    Err(e) => tracing::warn!(repo = %repo, peer = %target, error = %e, "ownership.handover.failed"),
                }
                if let Err(e) = self.claim_for(repo, &self.self_name.clone()).await {
                    tracing::warn!(repo = %repo, error = %e, "ownership.handover.reclaim.failed");
                }
                false
            }
        }
    }

    /// Close this node's handle. The entry is already gone, so this must NOT go through the
    /// release hook — and it drains first, because unlike every other `evict` caller this node
    /// may still be serving the repo.
    pub(super) async fn close_local(&self, repo: &str) {
        if let Some((o, n)) = repo.split_once('/') {
            self.store.pool.evict_after_drain(o, n).await;
        }
    }

    /// Wait for a leader that is not us, so the reassignments below have a writer to reach.
    /// Bounded by `LEADER_TTL + LEADER_RENEW`: the released lease is takeable at once, so this
    /// normally returns within a tick, and waiting longer than a full cycle would eat the drain's
    /// own budget for a leader that is evidently not coming.
    pub(super) async fn await_new_leader(&self) {
        let Some(os) = self.ownership.object_store() else { return };
        let deadline = std::time::Instant::now() + LEADER_TTL + lease::LEADER_RENEW;
        while std::time::Instant::now() < deadline {
            if let Ok(Some(c)) = lease::read(os.as_ref()).await {
                if c.lease.node != self.self_name && !lease::is_expired(&c.lease, self.now_ms()) {
                    self.note_live(&c.lease);
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        tracing::warn!("ownership.drain.noleader");
    }

    /// Which peer takes `repo`. Rendezvous over the nodes we can see, so a drain spreads its repos
    /// rather than piling them all on one pod, and two drains agree without coordinating.
    ///
    /// The candidate set is the one this process actually has: the nodes named by live entries in
    /// the map, plus the leader. There is no membership service here — `addr_of` is a name
    /// template, not a directory — and a node holding nothing is one nothing routes to yet, so
    /// missing it costs balance, never correctness.
    // ponytail: a node that owns nothing and is not the leader is invisible to this, so a
    // freshly-started peer gets no repos; a real membership list (or the StatefulSet's replica
    // count) is the upgrade if the spread ever matters more than the handover.
    pub(super) async fn handover_target(&self, repo: &str) -> Option<String> {
        let now = self.now_ms();
        let mut nodes: Vec<String> = self
            .ownership
            .all()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, e)| !ownership::is_expired(e, now))
            .map(|(_, e)| e.node)
            .chain(self.leader())
            .filter(|n| *n != self.self_name)
            .collect();
        nodes.sort_unstable();
        nodes.dedup();
        nodes.into_iter().max_by_key(|n| {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            repo.hash(&mut h);
            n.hash(&mut h);
            h.finish()
        })
    }

    /// `demote` for a caller already holding `leader_lock` (the grants).
    pub(super) async fn demote_locked(&self, why: &str) {
        if !self.is_leader() {
            return;
        }
        tracing::warn!(epoch = self.leader_epoch(), reason = why, "lease.demoted");
        self.leader_epoch.store(0, std::sync::atomic::Ordering::Relaxed);
        self.held_expires_ms.store(0, std::sync::atomic::Ordering::Relaxed);
        self.set_leader(None);
        self.ownership.demote().await;
        metrics::counter!("ownership_demotions_total").increment(1);
    }

    /// The epoch a map write is made under. Zero — not leading, or demoted since the handler
    /// checked — refuses: the in-process half of the fence, ahead of SlateDB's.
    pub(super) fn writing_epoch(&self) -> Result<u64> {
        match self.leader_epoch() {
            0 => Err(err("not the leader")),
            e => Ok(e),
        }
    }

    /// A map operation's result, with a fence turned into a demotion. Caller holds `leader_lock`.
    pub(super) async fn fenced_check<T>(&self, r: Result<T>) -> Result<T> {
        if let Err(e) = &r {
            if pool::is_fenced(e) {
                self.demote_locked("map write fenced").await;
            }
        }
        r
    }

    pub(super) async fn ask_leader(&self, what: &str, body: String) -> Result<String> {
        self.ask_leader_with(what, body, Self::default_patience(what)).await
    }

    pub(super) fn default_patience(what: &str) -> Patience {
        match what {
            "claim" => Patience::Claim,
            "release" => Patience::Release,
            _ => Patience::None,
        }
    }

    pub(super) async fn ask_leader_with(&self, what: &str, body: String, patience: Patience) -> Result<String> {
        // A claim waits out a leader restart instead of failing the client's request. Measured on a
        // rolling restart, the leader is unreachable for about 35s — its preStop delay, its
        // shutdown, its start, and the DNS cache behind it — and every request needing a claim in
        // that window failed. Waiting turns those into slow requests, which for a git client is the
        // difference between a retry and an error.
        //
        // Only claims wait. Renewals and releases run on their own clocks and would pile up on top
        // of each other; they are advisory, and a lease that misses a beat lapses on its TTL.
        // A release that does not land is expensive in a way a missed renewal is not: the entry
        // stays live for the whole LEASE_TTL, and every other node forwards into a node that has
        // already gone — which is exactly the 502 burst a roll produces. Retry it, bounded so the
        // whole thing still fits inside the shutdown's release budget. A renewal that misses a beat
        // simply waits for the next one.
        // A recovery ask — a forward to the owner just failed — must NOT inherit the claim budget.
        // Owner and leader both unreachable is exactly a rolling restart, and thirty seconds of
        // waiting there is worse than the immediate 502 this path replaced: the client had a
        // working owner a moment ago and can simply retry. Two quick tries cover a leader that is
        // merely between requests; anything longer, give up fast.
        let attempts = match patience {
            Patience::Claim => proxy::CLAIM_ATTEMPTS,
            Patience::Recover => proxy::RECOVER_ATTEMPTS,
            Patience::Release => proxy::RELEASE_ATTEMPTS,
            Patience::None => 1,
        };
        // Only the patient path is gated: it is the one that can hold a task for the length of a
        // leader failover. The permit lives for the whole retry loop.
        let _permit = match patience {
            Patience::Claim => Some(
                self.claim_gate
                    .try_acquire()
                    .map_err(|_| err("too many claims already waiting on the leader; retry"))?,
            ),
            _ => None,
        };
        let mut leader = self.leader();
        let mut last = err("the leader was unreachable");
        for attempt in 0..attempts {
            if attempt > 0 {
                let backoff = match patience {
                    Patience::Claim => proxy::CLAIM_BACKOFF,
                    Patience::Recover => proxy::RECOVER_BACKOFF,
                    _ => proxy::RELEASE_BACKOFF,
                };
                tokio::time::sleep(backoff).await;
            }
            // The cached name is only trusted once: an attempt is retried only after it failed,
            // and then the lease is the authority — the loop's last read may be a tick old. Re-read
            // it here so a failover completes inside THIS request's patience rather than waiting
            // for the next beat. After the gate: with the gate full, "too many claims" is the
            // answer whether or not a leader is known.
            let name = match leader.take() {
                Some(n) => n,
                None => match self.refresh_leader().await {
                    Some(n) => n,
                    None => {
                        last = err("no live leader");
                        continue;
                    }
                },
            };
            let addr = (self.addr_of)(&name);
            let res = self
                .forwarder
                .client
                .post(format!("http://{addr}/own/{what}"))
                .header(proxy::PEER_HEADER, &self.forwarder.secret)
                .timeout(proxy::LEADER_TIMEOUT)
                .body(body.clone())
                .send()
                .await;
            match res {
                Ok(r) if r.status().is_success() => return Ok(r.text().await?),
                // The node we asked is not the leader — it never was, or it was just fenced and
                // demoted. Our name is stale; the next attempt re-reads.
                Ok(r) if r.status() == reqwest::StatusCode::MISDIRECTED_REQUEST => {
                    last = err(format!("own/{what}: {name} is not the leader"));
                }
                // Any other answer is about the request, not about who leads: retrying cannot change it.
                Ok(r) => return Err(err(format!("own/{what}: leader answered {}", r.status()))),
                Err(e) => last = e.into(),
            }
        }
        Err(last)
    }

    /// Re-read who leads. `None` means the lease is absent or expired — nobody can grant right
    /// now — and forgets the name we had. A store error says nothing about who leads, so it keeps
    /// what we had rather than forgetting a leader that is probably fine.
    pub(super) async fn refresh_leader(&self) -> Option<String> {
        let os = self.ownership.object_store()?;
        match lease::read(os.as_ref()).await {
            Ok(Some(h)) if !lease::is_expired(&h.lease, self.now_ms()) => {
                self.note_live(&h.lease);
                Some(h.lease.node)
            }
            Ok(_) => {
                self.set_leader(None);
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, "lease.read.failed");
                self.leader()
            }
        }
    }

    // ---- The leader's side of the three messages. Only ever reached on the lease holder. ----
}
