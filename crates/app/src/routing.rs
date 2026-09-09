//! Who owns a repo and how a request gets there: `route_for` answers from the map (asking the
//! leader or claiming when nobody owns it), claims and renewals go to the leader as grants, and a
//! fenced database handle is recovered through `on_fenced`.

use super::*;

impl App {
    /// Where this request belongs.
    ///
    /// Read the map; if it names someone and the lease is live, that is the answer. Otherwise ask
    /// the leader — and if the leader cannot be reached, answer `Unavailable`. **Never serve on a
    /// failed claim**: falling back to "well, serve it here" is failover to whoever asked first,
    /// which is the two-writer bug this design exists to remove.
    ///
    /// `may_create` says whether the route being served can bring this database into being (see
    /// `router::route::may_create`); only such a route may claim a key with nothing under it.
    pub async fn route_for(&self, repo: &str, may_create: bool) -> Route {
        let now = self.now_ms();
        let entry = match self.owner(repo).await {
            Ok(c) => c,
            // The map is unreadable from here. We know nothing, so we may not serve.
            Err(e) => {
                tracing::warn!(repo = %repo, error = %e, "ownership.read.failed");
                return Route::Unavailable;
            }
        };
        let live = entry.clone().filter(|e| !ownership::is_expired(e, now));
        let node = match live {
            Some(e) => e.node,
            None => {
                // An unhealthy node must not claim: it would take a lease on a repo it cannot
                // serve, and hold it for the whole TTL.
                if !self.store.healthy() {
                    return Route::Unavailable;
                }
                // Nor may a node on its way out. SIGTERM releases every lease and closes the pool,
                // and a request arriving in the drain window that follows sees its own released
                // entry as absent — so it would claim the repo straight back. The leader has no way
                // to know the asker is seconds from exiting and may well grant it: then `pool.get`
                // fails with "pool is closed", and every other node forwards here for a full
                // LEASE_TTL. One dead end becomes a ten second one.
                // Same for a pod that is draining: it is handing back everything it owns, so
                // taking a NEW repo would only mean handing that one back too — or, if the drain
                // has already passed it, holding it open with nobody left to give it to. What it
                // already owns keeps serving (the entry below names us and is live); only the
                // claim path is closed.
                if self.store.pool.is_closed() || self.is_draining() {
                    // ...but it can still ROUTE. A draining pod keeps receiving requests for up to
                    // one readiness period after `begin_draining` (the Service drops it on the next
                    // /healthz), and a repo with no live entry in that window — its own, just
                    // released and not yet claimed by the peer `hand_over` named — used to answer
                    // 503 from here. Forward to the node the handover picks for this repo: the same
                    // deterministic choice, so the peer that takes the request is the one about to
                    // own it, and its own `route_for` claims on arrival. The weekly's
                    // `roll.zero.errors` counted exactly one such answer per roll.
                    return match self.handover_target(repo).await {
                        Some(peer) => self.route_to(peer),
                        None => Route::Unavailable,
                    };
                }
                // A repo the map does not name is CLAIMED before anyone opens it. Routing on
                // "does the prefix exist" was a two-writer window: the first write to a new repo,
                // image or volume opened it here unleased, and until its manifest landed every
                // other node saw the same empty prefix and opened it too. That is why the routes
                // which can CREATE claim unconditionally — the window is theirs and they keep the
                // lease. Every other route gates on the prefix, because `route()` runs before
                // authentication: without the gate a spray of invented names is one leader map
                // write per name per LEASE_TTL, from an anonymous client, against the one node the
                // whole fleet's routing depends on. An `exists` that errs falls back to claiming,
                // exactly as `force_claim` does — an unreadable store must not turn into a 404.
                let empty_prefix = !may_create
                    && match repo.split_once('/') {
                        Some((o, n)) => !self.store.pool.exists(o, n).await.unwrap_or(true),
                        None => false,
                    };
                if empty_prefix {
                    // Ask the LEADER who owns it before answering Missing. This node's own copy
                    // of the map is a follower's, up to a poll interval behind, so "the map names
                    // nobody" is only ever a guess here — which is why the claim path asks the
                    // leader too. Unlike a claim this WRITES NOTHING, so an invented name still
                    // costs the elected writer no map write.
                    //
                    // What it buys, exactly: the prefix probe and the map read are not one atomic
                    // look, and a creator elsewhere can claim the key and flush its first objects
                    // between them — after which falling through to a handler HERE opens the
                    // database unleased and fences the owner. The leader read narrows that window
                    // to the gap between its "nobody" and the handler's own `exists` probe; the
                    // creator's flush has to land inside THAT gap to hurt, which is far smaller
                    // than the whole request. It is not zero.
                    // ponytail: residual unleased-open window between the leader's "nobody" and
                    // the handler's probe; an atomic claim-or-read on the leader (answer the owner
                    // if there is one, claim only if the prefix is non-empty, all under the
                    // leader's lock) is the upgrade if it ever bites.
                    //
                    // A leader that cannot be reached is treated exactly like "nobody", on
                    // purpose: the alternative is 503 on a path an anonymous client reaches, and
                    // the cost of being wrong is bounded — a just-claimed, unflushed repo answers
                    // 404 locally after authentication, and nothing is opened, because the
                    // handler's own probe sees the same empty prefix.
                    if !self.may_ask_who_owns(repo) {
                        return Route::Missing;
                    }
                    return match self.ask_owner(repo).await.ok().flatten() {
                        Some(e) if !ownership::is_expired(&e, self.now_ms()) => {
                            self.route_to(e.node)
                        }
                        _ => {
                            self.note_no_owner(repo);
                            Route::Missing
                        }
                    };
                }
                match self.claim(repo).await {
                    Ok(Grant::Granted(e)) | Ok(Grant::HeldBy(e)) => e.node,
                    Err(e) => {
                        tracing::warn!(repo = %repo, error = %e, "ownership.claim.failed");
                        // The leader is unreachable. If the (expired) entry names US and we still
                        // hold the database open, keep serving it. A grant only ever comes from
                        // the leader, so an unreachable leader means nobody else can have been
                        // granted this repo either — and we are still holding it, so continuing
                        // cannot produce a second writer. During a leader failover every entry
                        // ages out unrenewed; refusing here would 503 warm repos
                        // fleet-wide for the length of the restart, and buy nothing. A cold repo,
                        // or one named to someone else, is still Unavailable.
                        if entry.is_some_and(|e| e.node == self.self_name)
                            && self.store.pool.warm_repos().iter().any(|r| r == repo)
                        {
                            self.self_name.clone()
                        } else {
                            return Route::Unavailable;
                        }
                    }
                }
            }
        };
        self.route_to(node)
    }

    /// The node the map named, as a `Route`.
    pub(super) fn route_to(&self, node: String) -> Route {
        if node == self.self_name {
            // An unhealthy node still forwards what it does not own (safe, and keeps its share of
            // load-balancer traffic flowing) but never serves what it does. The same holds for a
            // node on its way out: its pool is closed, so serving would fail at `pool.get` anyway —
            // and answering Unavailable here lets the client retry somewhere useful instead.
            if self.store.healthy() && !self.store.pool.is_closed() {
                Route::Local
            } else {
                Route::Unavailable
            }
        } else {
            Route::Peer(ownership::Peer {
                addr: (self.addr_of)(&node),
                name: node,
            })
        }
    }

    /// `route_for` for the paths that can never create a database — every git route, and the peer
    /// stream. The default is the safe one on purpose: a new caller that forgets to think about it
    /// gets the gated behaviour, not the amplifier.
    pub async fn route(&self, repo: &str) -> Route {
        self.route_for(repo, false).await
    }

    /// This node's view of wall-clock time, in ms since the epoch. Every lease decision this
    /// node makes reads the clock through here so a test can move it.
    pub fn now_ms(&self) -> u64 {
        ownership::now_ms() + self.skew_ms.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Test hook: move this node's clock forward. Never called in production.
    pub fn advance_clock(&self, d: std::time::Duration) {
        self.skew_ms
            .fetch_add(d.as_millis() as u64, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether this node may ask the leader about `repo` on a failed forward right now, recording
    /// the ask if so. See `recovery_asked`.
    pub fn may_ask_to_recover(&self, repo: &str) -> bool {
        let now = self.now_ms();
        let mut m = self.recovery_asked.lock().unwrap();
        match m.get(repo) {
            Some(t) if now.saturating_sub(*t) < RECOVERY_ASK_EVERY.as_millis() as u64 => false,
            _ => {
                m.insert(repo.to_string(), now);
                true
            }
        }
    }

    /// Whether the leader still has to be asked who owns `repo`, recording the ask if so. A "no
    /// owner" answer is remembered for `MISSING_ASK_EVERY`, which is well under `LEASE_TTL`, so a
    /// creator that claims the key is still seen within one window. See `missing_seen`.
    pub(super) fn may_ask_who_owns(&self, repo: &str) -> bool {
        let now = self.now_ms();
        let mut m = self.missing_seen.lock().unwrap();
        if m.get(repo)
            .is_some_and(|t| now.saturating_sub(*t) < MISSING_ASK_EVERY.as_millis() as u64)
        {
            return false;
        }
        if m.len() >= MISSING_CACHE_MAX {
            // Drop what has aged out; if it is still full the spray is wider than the cap, and
            // forgetting everything is the honest bound — the worst case is the uncached rate.
            m.retain(|_, t| now.saturating_sub(*t) < MISSING_ASK_EVERY.as_millis() as u64);
            if m.len() >= MISSING_CACHE_MAX {
                m.clear();
            }
        }
        true
    }

    /// Remember that the leader named nobody for `repo`.
    pub(super) fn note_no_owner(&self, repo: &str) {
        let now = self.now_ms();
        self.missing_seen.lock().unwrap().insert(repo.to_string(), now);
    }

    /// Ask for this repo. On the leader that is a local decision and a write; anywhere else it is
    /// one POST to the leader's peer port.
    pub async fn claim(&self, repo: &str) -> Result<Grant> {
        self.claim_inner(repo, false, Patience::Claim).await
    }

    /// The ordinary claim, on the short retry budget: for a forward to the owner that just failed.
    /// Same decision at the leader; only how long this node is willing to wait for it differs.
    pub async fn claim_to_recover(&self, repo: &str) -> Result<Grant> {
        // Same admission as a forced claim: a node that is unhealthy or on its way out must not be
        // granted a repo it will then fail to open. (It would self-heal through the release on a
        // failed open, but that costs the client a request for nothing.)
        if !self.store.healthy() || self.store.pool.is_closed() || self.is_draining() {
            return Err(err("this node may not take a repo over right now"));
        }
        self.claim_inner(repo, false, Patience::Recover).await
    }

    /// Ask the leader to take this repo off a holder we could not reach. Only `http.rs`'s recovery
    /// path calls this, and only after a re-route has already been tried and failed.
    ///
    /// The same health guards as the claim path in `route()`. Unlike it, a repo with an empty
    /// prefix is refused here: a FORCED claim evicts a named holder, and a holder whose repo has
    /// nothing in the store yet is a creator mid-write — the one moment a takeover is guaranteed
    /// to fence a live database for nothing. Its lease lapses on the TTL and the ordinary claim
    /// path takes it from there. `exists` erring falls back to asking, as it does in `route()`.
    pub async fn force_claim(&self, repo: &str) -> Result<Grant> {
        if !self.store.healthy() || self.store.pool.is_closed() || self.is_draining() {
            return Err(err("this node may not take a repo over right now"));
        }
        if let Some((o, n)) = repo.split_once('/') {
            if !self.store.pool.exists(o, n).await.unwrap_or(true) {
                return Err(err(format!("{repo}: no such repository")));
            }
        }
        self.claim_inner(repo, true, Patience::Recover).await
    }

    /// Who the LEADER says owns `repo` — the authoritative read, and the reason it exists: this
    /// node's copy of the map is eventually consistent, so "the map names nobody" is only ever a
    /// guess here. Unlike `claim` it takes no lease and writes nothing.
    pub async fn ask_owner(&self, repo: &str) -> Result<Option<Entry>> {
        self.owner_asks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if self.is_leader() {
            return self.ownership.get(repo).await;
        }
        // The recovery budget, not the claim budget: this runs ahead of authentication, so it must
        // never hold a task through a leader failover.
        let reply = self
            .ask_leader_with("owner", repo.to_string(), Patience::Recover)
            .await?;
        let mut lines = reply.lines();
        match (lines.next(), lines.next()) {
            (Some(node), Some(expires)) if !node.is_empty() => Ok(Some(Entry {
                node: node.to_string(),
                expires_ms: expires
                    .parse()
                    .map_err(|_| err(format!("owner reply: bad expiry {expires:?}")))?,
            })),
            _ => Ok(None),
        }
    }

    /// Claim `repo` ON BEHALF OF another node — the handover half of a drain. The wire protocol
    /// already carries the asker's name, and the leader grants to whoever the body names, so this
    /// is `claim` with a different name in it and nothing else.
    pub async fn claim_for(&self, repo: &str, node: &str) -> Result<Grant> {
        self.claim_as(repo, node, false, Patience::Recover).await
    }

    pub(super) async fn claim_inner(&self, repo: &str, force: bool, patience: Patience) -> Result<Grant> {
        self.claim_as(repo, &self.self_name.clone(), force, patience).await
    }

    pub(super) async fn claim_as(&self, repo: &str, asker: &str, force: bool, patience: Patience) -> Result<Grant> {
        if self.is_leader() {
            return self.grant_claim(repo, asker, force).await;
        }
        let body = if force {
            format!("{repo}\n{asker}\nforce")
        } else {
            format!("{repo}\n{asker}")
        };
        let reply = self.ask_leader_with("claim", body, patience).await?;
        let mut lines = reply.lines();
        let (verb, node, expires) = (
            lines.next().unwrap_or_default(),
            lines.next().unwrap_or_default().to_string(),
            lines.next().unwrap_or_default(),
        );
        let e = Entry {
            node,
            expires_ms: expires
                .parse()
                .map_err(|_| err(format!("claim reply: bad expiry {expires:?}")))?,
        };
        match verb {
            "granted" => Ok(Grant::Granted(e)),
            "heldby" => Ok(Grant::HeldBy(e)),
            other => Err(err(format!("claim reply: unknown verb {other:?}"))),
        }
    }

    /// Renew everything this node holds, in one message. Returns the repos whose lease was NOT
    /// renewed — the caller must close those databases at once (the lifecycle invariant).
    pub async fn renew_all(&self, repos: &[String]) -> Result<Vec<String>> {
        // No short-circuit on an empty list: the beat is also how an idle node re-reads the lease
        // on a failed ask (`refresh_leader`), and a node holding nothing is exactly the freshly
        // rolled one whose readiness the probe is trying to establish.
        if self.is_leader() {
            return self.grant_renew(&self.self_name.clone(), repos).await;
        }
        let mut body = self.self_name.clone();
        for r in repos {
            body.push('\n');
            body.push_str(r);
        }
        let reply = self.ask_leader("renew", body).await?;
        Ok(reply
            .lines()
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect())
    }

    /// One renewal beat: renew every repo this node holds open, and close at once any the leader
    /// declines. A declined renewal means the map no longer names us — the lease is gone, so the
    /// handle must go with it (the lifecycle invariant), before a fence makes the point for us.
    pub async fn renew_once(&self) -> Result<()> {
        let lost = self.renew_all(&self.store.pool.warm_repos()).await?;
        for repo in lost {
            tracing::info!(repo = %repo, "ownership.lost");
            if let Some((o, n)) = repo.split_once('/') {
                self.store.pool.evict(o, n).await;
            }
        }
        Ok(())
    }

    /// How long a claimed merge may sit before it is assumed abandoned and may be taken again.
    /// Generous: a merge on a large tree is real work in a worker, and re-running one that is
    /// still in flight is worse than waiting.
    pub const MERGE_LEASE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

    /// Leader only: drop entries whose lease lapsed without a release — the node holding them died
    /// or was partitioned away. Keeps the map bounded by what is actually open.
    pub async fn prune_once(&self) -> Result<()> {
        let _g = self.leader_lock.lock().await;
        self.writing_epoch()?;
        let now = self.now_ms();
        let all = self.fenced_check(self.ownership.all().await).await?;
        // The writer is the only one that can sweep, so its count is the one honest size of the map.
        metrics::gauge!("ownership_map_size").set(all.len() as f64);
        for (repo, e) in all {
            if ownership::is_expired(&e, now) {
                self.fenced_check(self.ownership.delete(&repo).await).await?;
            }
        }
        Ok(())
    }

    /// Give a repo up: the entry is deleted, and the repo is immediately claimable by anyone. The
    /// caller must already have CLOSED the database — see `Pool::retire`, which drains, closes,
    /// and only then calls this. Releasing while the handle is still open is what lets a successor
    /// fence a database this node is still writing through.
    pub async fn release(&self, repo: &str) -> Result<()> {
        if self.is_leader() {
            return self.grant_release(repo, &self.self_name.clone()).await;
        }
        self.ask_leader("release", format!("{repo}\n{}", self.self_name))
            .await
            .map(|_| ())
    }

    pub async fn grant_claim(&self, repo: &str, asker: &str, force: bool) -> Result<Grant> {
        // Serialize every read-modify-write on the map: concurrent claims/renews/prunes on the
        // same repo could otherwise both read a stale map and both write, granting one repo to
        // two nodes — which fences the loser's live database. One process, one lock: cheap and
        // total. `demote` takes the same lock, so an epoch seen here is still held at the write.
        let _g = self.leader_lock.lock().await;
        let epoch = self.writing_epoch()?;
        let now = self.now_ms();
        let cur = self.fenced_check(self.ownership.get(repo).await).await?;
        let g = if force {
            ownership::decide_force_claim(cur.as_ref(), asker, now)
        } else {
            ownership::decide_claim(cur.as_ref(), asker, now)
        };
        if let Grant::Granted(e) = &g {
            // A grant over a live entry naming another node is a MOVE (a roll, a drain, a
            // force-claim), which is the event worth graphing against 421s and fences.
            let result = match &cur {
                Some(c) if c.node != e.node => "moved",
                _ => "granted",
            };
            metrics::counter!("ownership_claims_total", "result" => result).increment(1);
            self.fenced_check(self.ownership.put(repo, e).await).await?;
            tracing::debug!(repo = %repo, node = %e.node, epoch, "ownership.granted");
        } else {
            metrics::counter!("ownership_claims_total", "result" => "heldby").increment(1);
        }
        Ok(g)
    }

    pub async fn grant_renew(&self, asker: &str, repos: &[String]) -> Result<Vec<String>> {
        // One lock, N local reads, ONE durable write. The lock used to be taken per repo so that
        // `grant_claim` — on a cold repo's request path — was not queued behind N serialised WAL
        // flushes; batching removes the flushes instead, so what the lock now covers is N memtable
        // reads and a single write, which is about what one put cost. Every entry's
        // compare-and-set stays atomic: nothing else writes the map between the read and the batch.
        let _g = self.leader_lock.lock().await;
        self.writing_epoch()?;
        let now = self.now_ms();
        let mut lost = Vec::new();
        let mut renewed = Vec::new();
        for repo in repos {
            let cur = self.fenced_check(self.ownership.get(repo).await).await?;
            match ownership::decide_renew(cur.as_ref(), asker, now) {
                Some(e) => renewed.push((repo.clone(), e)),
                None => lost.push(repo.clone()),
            }
        }
        self.fenced_check(self.ownership.put_many(&renewed).await).await?;
        Ok(lost)
    }

    pub async fn grant_release(&self, repo: &str, asker: &str) -> Result<()> {
        let _g = self.leader_lock.lock().await;
        self.writing_epoch()?;
        let cur = self.fenced_check(self.ownership.get(repo).await).await?;
        if ownership::may_release(cur.as_ref(), asker) {
            self.fenced_check(self.ownership.delete(repo).await).await?;
        }
        Ok(())
    }

    /// What to do when a request for `repo` hit a fence: re-run routing. `true` means this node
    /// still owns the repo (a stray admin process fenced us, or a peer has since released it) and
    /// the caller should reopen and retry the operation ONCE, in-handler — the HTTP handlers hold
    /// the body as `Bytes`, so a retry costs nothing. `false` means the fence was correct: answer
    /// 503. git does NOT retry a 503 by itself; the user re-runs.
    pub async fn on_fenced(&self, owner: &str, name: &str) -> bool {
        // THE invariant violation (CLAUDE.md): another node opened this database under us. Every
        // path (HTTP, SSH, peer) lands here, so this is the one count that means "it happened".
        metrics::counter!("db_fence_detected_total").increment(1);
        // The ungated form: the database demonstrably exists — it just fenced us.
        if !matches!(self.route_for(&format!("{owner}/{name}"), true).await, Route::Local) {
            return false;
        }
        // Pool::get never reopens a fenced handle by itself (that is the amplifier this exists to
        // remove). Routing says we still own it, so evict here — the retry's Pool::get then opens
        // fresh and takes the writer epoch back. Without this the retry gets a second FencedError.
        self.store.pool.evict(owner, name).await;
        true
    }

    /// `open_repo`, retried once when the first attempt hits a fence that routing says this node
    /// may still own (see `on_fenced`). The one place that rule lives, so HTTP, SSH and the peer
    /// stream cannot drift: SSH did not retry at all, and a stray fence made it fail until some
    /// HTTP request happened to evict the handle. A fence this node must honour comes back as the
    /// original error for the caller to report.
    pub async fn open_repo_after_fence(&self, owner: &str, name: &str) -> Result<Option<store::Repo>> {
        match self.store.open_repo(owner, name).await {
            Err(e) if pool::is_fenced(&e) && self.on_fenced(owner, name).await => {
                self.store.open_repo(owner, name).await
            }
            r => r,
        }
    }
}
