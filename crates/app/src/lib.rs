use rustic_git_core::jwt;
use rustic_git_core::peer as proxy;
use rustic_git_storage::{ownership, pool, store};
use rustic_git_pulls::pulls;

use ownership::{Entry, Grant, OwnershipStore, Route};
use rustic_git_core::{err, Result};
use std::sync::Arc;

/// Resolves a node name (`rustic-git-1`) to the address of its peer HTTP listener. In production
/// that is `{name}.{svc}:{port}` — the StatefulSet's own identity, no lookup. It is a function
/// rather than a template so tests can put a fleet on loopback ports.
pub type AddrOf = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// How patiently to wait for the leader. Chosen by the caller, because the same wire request can
/// deserve very different patience: a cold claim waits out a leader restart, a recovery ask after
/// a failed forward must not.
#[derive(Clone, Copy)]
pub enum Patience {
    /// A cold claim: wait out a leader restart rather than fail the client's request.
    Claim,
    /// A forward to the owner just failed: two quick tries, then a fast 502 the client retries.
    Recover,
    Release,
    None,
}

pub struct App {
    pub store: Arc<store::Store>,
    pub ownership: Arc<OwnershipStore>,
    /// This pod's own name, e.g. `rustic-git-2`.
    pub self_name: String,
    /// Who writes the ownership map. Derived from `self_name` by default (ordinal zero of this
    /// StatefulSet), overridden by `with_topology` when the leader runs in its own StatefulSet
    /// and no amount of string surgery on a server's name can name it.
    pub leader_name: String,
    /// The StatefulSet prefix that serving pods share, e.g. `rustic-git`. Equal to the leader's
    /// prefix unless the leader has been split out.
    pub server_prefix: String,
    pub addr_of: AddrOf,
    pub forwarder: Arc<proxy::Forwarder>,
    /// How many pods the StatefulSet runs. The leader needs it to know who it may hand a repo to;
    /// nothing else reads it.
    pub replicas: u32,
    /// When this node last asked the leader about a repo because a forward to its owner failed.
    /// A forward that fails is answered by asking the leader, and during a blip that touches many
    /// forwards at once every one of them would otherwise ask — a burst on pod zero at the moment
    /// it is least able to take one. One ask per repo per second is plenty: the answer does not
    /// change faster than that, and a request that arrives inside the window gets a plain 502 to
    /// retry, by which time the first ask has moved the map.
    // ponytail: unbounded map; entries are one u64 per repo ever recovered, and a repo count
    // that makes this matter is a bigger problem elsewhere first.
    pub recovery_asked: std::sync::Mutex<std::collections::HashMap<String, u64>>,
    /// Milliseconds added to this node's wall clock. Zero in production; a test advances it to
    /// age a lease entry or a recovery window without sleeping through it. Per node, not
    /// process-wide: the routing tests run many nodes in one process, and skewing them all
    /// would expire another test's drain lease under it.
    skew_ms: std::sync::atomic::AtomicU64,
    /// `now_ms()` of the last reply the leader gave this node, on any `/own/*` message. Zero until
    /// the first one. `/healthz` reads it: a node that has not heard from the leader inside one
    /// `LEASE_TTL` cannot claim, and whatever it holds may already be granted elsewhere — that is
    /// not a node to route traffic to, and the object-store ping alone could not tell.
    leader_seen_ms: std::sync::atomic::AtomicU64,
    /// Mints and verifies registry bearer tokens (`/v2/token`). Keyed from
    /// `RUSTIC_GIT_JWT_SECRET` when set; otherwise a random per-process secret, which means
    /// tokens die with the process — fine for a dev run, and in a fleet it shows up as
    /// "log in again", never as a forged token being accepted.
    pub jwt: Arc<jwt::Jwt>,
    /// Serializes the leader's four read-modify-write paths on the ownership map (grant_claim,
    /// grant_renew, grant_release, prune_once). Without it, two concurrent claims can both read
    /// `None` for the same repo and both write — granting one repo to two nodes, which fences the
    /// loser's live database. One process, one lock: cheap and total.
    pub leader_lock: tokio::sync::Mutex<()>,
    /// How many cold claims may wait on the leader at once. A claim waits out a leader roll
    /// (`CLAIM_ATTEMPTS × CLAIM_BACKOFF`, ~30 s), and each one pins an axum task for that long;
    /// a burst of cold repos during a roll would otherwise pin them all. Past the ceiling a claim
    /// fails at once and the client gets a fast 503 to retry instead of a slow one.
    pub claim_gate: tokio::sync::Semaphore,
    /// Mongo, for the ONE thing an owning node still needs it for: copying a repo's pre-existing
    /// pull requests into its own database on first touch (`pulls::ensure_migrated`). Resolved
    /// state, not an `Option`: "not configured" is safe to migrate as empty, "configured but
    /// unreachable" must not be, and a pair of fields could hold the nonsensical combination.
    pub dir: pulls::Source,
}

/// How long after asking the leader about a repo this node will not ask again for the same repo.
pub const RECOVERY_ASK_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// See `App::claim_gate`.
pub const MAX_WAITING_CLAIMS: usize = 64;

/// Pacing between repos in the visibility repair lane, mirroring the gc sweep's per-owner gap:
/// the lane is a backstop, not a deadline, so it yields object-store bandwidth to real requests.
pub const RECONCILE_GAP: std::time::Duration = std::time::Duration::from_millis(200);


/// Eviction gives the lease back before the database closes. `Pool` calls this; it holds a `Weak`
/// so this reference back into `App` is not a cycle.
impl pool::ReleaseHook for App {
    fn release(&self, repo: String) -> futures::future::BoxFuture<'_, ()> {
        // The pool has already marked the entry releasing, so a failure here is not fatal: the
        // lease simply lapses on its own TTL instead of the drain. Log and close anyway.
        Box::pin(async move {
            if let Err(e) = App::release(self, &repo).await {
                tracing::warn!(repo = %repo, error = %e, "releasing the lease failed; it will lapse on its own TTL");
            }
        })
    }
}

/// How long a follower stays ready after the leader last answered it. Longer than one
/// `LEASE_TTL` on purpose: a leader pod roll takes ~35 s, and every srv pod dropping out of
/// the public Service for that whole window would turn a routine deploy into an outage. Six
/// TTLs covers a roll; a leader that is really gone still takes every follower un-ready within
/// a minute, which is what the probe is for.
pub const LEADER_SILENCE: std::time::Duration = std::time::Duration::from_secs(60);

impl App {
    pub fn new(
        store: Arc<store::Store>,
        ownership: Arc<OwnershipStore>,
        self_name: String,
        addr_of: AddrOf,
        peer_secret: String,
        replicas: u32,
    ) -> Self {
        let jwt_secret = std::env::var("RUSTIC_GIT_JWT_SECRET").unwrap_or_else(|_| {
            use rand::Rng;
            rand::thread_rng()
                .sample_iter(rand::distributions::Alphanumeric)
                .take(48)
                .map(char::from)
                .collect()
        });
        // Defaults reproduce the single-StatefulSet layout exactly: leader at ordinal zero of this
        // pod's own prefix. `with_topology` replaces them when the leader lives elsewhere.
        let leader_name = ownership::leader_of(&self_name).unwrap_or_else(|_| self_name.clone());
        let server_prefix = self_name
            .rsplit_once('-')
            .map(|(p, _)| p.to_string())
            .unwrap_or_else(|| self_name.clone());
        App {
            store,
            ownership,
            self_name,
            leader_name,
            server_prefix,
            addr_of,
            forwarder: Arc::new(proxy::Forwarder::new(peer_secret)),
            replicas,
            recovery_asked: Default::default(),
            skew_ms: std::sync::atomic::AtomicU64::new(0),
            leader_seen_ms: std::sync::atomic::AtomicU64::new(0),
            jwt: Arc::new(jwt::Jwt::new(&jwt_secret).expect("jwt secret")),
            leader_lock: tokio::sync::Mutex::new(()),
            claim_gate: tokio::sync::Semaphore::new(MAX_WAITING_CLAIMS),
            dir: pulls::Source::Absent,
        }
    }

    /// The directory this node migrates pull requests from. Set once at startup, before the `App`
    /// is shared; there is no path that changes it later.
    pub fn with_directory(mut self, dir: pulls::Source) -> Self {
        self.dir = dir;
        self
    }

    /// Who owns this repo, from this node's own copy of the map. No network: a follower's
    /// read-only handle answers, however stale it is — a stale read costs a hop, never an owner.
    pub async fn owner(&self, repo: &str) -> Result<Option<Entry>> {
        self.ownership.get(repo).await
    }

    fn leader(&self) -> &str {
        &self.leader_name
    }

    /// Point this node at a leader that is not ordinal zero of its own StatefulSet.
    ///
    /// Naming the leader explicitly is the whole cost of splitting it out: every other node has to
    /// agree on who the single writer is, and two nodes disagreeing is precisely the split brain
    /// that fences a live database. Derivation cannot cross a StatefulSet boundary, so it becomes
    /// configuration — and both values must be identical on every pod.
    pub fn with_topology(mut self, leader: String, server_prefix: String) -> Self {
        self.leader_name = leader;
        self.server_prefix = server_prefix;
        self
    }

    /// Leadership is a name, not a decision — there is nothing here that two nodes could answer
    /// differently, which is the whole point of the design.
    pub fn is_leader(&self) -> bool {
        self.leader() == self.self_name
    }

    /// Where this request belongs.
    ///
    /// Read the map; if it names someone and the lease is live, that is the answer. Otherwise ask
    /// the leader — and if the leader cannot be reached, answer `Unavailable`. **Never serve on a
    /// failed claim**: falling back to "well, serve it here" is failover to whoever asked first,
    /// which is the two-writer bug this design exists to remove.
    pub async fn route(&self, repo: &str) -> Route {
        let now = self.now_ms();
        let entry = match self.owner(repo).await {
            Ok(c) => c,
            // The map is unreadable from here. We know nothing, so we may not serve.
            Err(e) => {
                tracing::error!(repo = %repo, error = %e, "ownership read failed; refusing to serve");
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
                if self.store.pool.is_closed() {
                    return Route::Unavailable;
                }
                // A repo the map does not name is CLAIMED before anyone opens it, whether or not
                // its object-store prefix has anything in it yet. Routing on "does the prefix
                // exist" was a two-writer window: the first write to a new repo, image or
                // volume opened it here unleased, and until its manifest landed every other node
                // saw the same empty prefix and opened it too. A request for a name that really
                // does not exist still 404s in the handler (`open_repo` checks the prefix before
                // it opens anything); the claim it left behind lapses on the lease TTL, unrenewed,
                // because a repo never opened is never warm.
                // ponytail: one leader write per invented name per LEASE_TTL, pre-auth. Ceiling is
                // the leader's claim rate under a spray of distinct bad names; a per-node token
                // bucket on claims for empty-prefix names is the upgrade if that ever shows.
                match self.claim(repo).await {
                    Ok(Grant::Granted(e)) | Ok(Grant::HeldBy(e)) => e.node,
                    Err(e) => {
                        tracing::warn!(repo = %repo, error = %e, "claiming from the leader failed");
                        // The leader is unreachable. If the (expired) entry names US and we still
                        // hold the database open, keep serving it. A grant only ever comes from
                        // the leader, so an unreachable leader means nobody else can have been
                        // granted this repo either — and we are still holding it, so continuing
                        // cannot produce a second writer. During a roll pod zero updates last,
                        // which ages out every entry; refusing here would 503 warm repos
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

    /// The leader answered just now. Called on every successful `/own/*` round trip, so the renew
    /// beat (`RENEW_EVERY`, well inside `LEASE_TTL`) keeps this fresh on an idle node too.
    pub fn mark_leader_seen(&self) {
        self.leader_seen_ms.store(self.now_ms(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether the leader has answered this node within the last `LEADER_SILENCE`. The leader is
    /// always reachable to itself. A cached read: `/healthz` calls this on every probe.
    pub fn leader_reachable(&self) -> bool {
        self.is_leader()
            || self.now_ms().saturating_sub(self.leader_seen_ms.load(std::sync::atomic::Ordering::Relaxed))
                < LEADER_SILENCE.as_millis() as u64
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
        if !self.store.healthy() || self.store.pool.is_closed() {
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
        if !self.store.healthy() || self.store.pool.is_closed() {
            return Err(err("this node may not take a repo over right now"));
        }
        if let Some((o, n)) = repo.split_once('/') {
            if !self.store.pool.exists(o, n).await.unwrap_or(true) {
                return Err(err(format!("{repo}: no such repository")));
            }
        }
        self.claim_inner(repo, true, Patience::Recover).await
    }

    async fn claim_inner(&self, repo: &str, force: bool, patience: Patience) -> Result<Grant> {
        if self.is_leader() {
            return self.grant_claim(repo, &self.self_name.clone(), force).await;
        }
        let body = if force {
            format!("{repo}\n{}\nforce", self.self_name)
        } else {
            format!("{repo}\n{}", self.self_name)
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
        // No short-circuit on an empty list: the beat is also how an idle node proves it can
        // reach the leader (`leader_reachable`), and a node holding nothing is exactly the freshly
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
            tracing::info!(repo = %repo, "lost the lease: closing it");
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
        let now = self.now_ms();
        let all = self.ownership.all().await?;
        // The leader is the only writer, so its sweep is the one honest count of the map.
        metrics::gauge!("ownership_map_size").set(all.len() as f64);
        for (repo, e) in all {
            if ownership::is_expired(&e, now) {
                self.ownership.delete(&repo).await?;
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

    /// Tell the leader this node is on its way out — or, at startup, that it is not.
    ///
    /// Announced by the node itself: it is the only one that knows it has been asked to stop. The
    /// leader uses it to avoid handing repos to a pod that is leaving, which it would otherwise do
    /// preferentially, since a node that has released everything looks like the least loaded one.
    pub async fn announce_draining(&self, draining: bool) -> Result<()> {
        let flag = if draining { "1" } else { "0" };
        if self.is_leader() {
            return self.ownership.set_draining(&self.self_name, draining).await;
        }
        self.ask_leader("draining", format!("{}\n{flag}", self.self_name))
            .await
            .map(|_| ())
    }

    async fn ask_leader(&self, what: &str, body: String) -> Result<String> {
        self.ask_leader_with(what, body, Self::default_patience(what)).await
    }

    fn default_patience(what: &str) -> Patience {
        match what {
            "claim" => Patience::Claim,
            "release" | "draining" => Patience::Release,
            _ => Patience::None,
        }
    }

    async fn ask_leader_with(&self, what: &str, body: String, patience: Patience) -> Result<String> {
        let leader = self.leader();
        let addr = (self.addr_of)(leader);
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
        // leader roll. The permit lives for the whole retry loop.
        let _permit = match patience {
            Patience::Claim => Some(
                self.claim_gate
                    .try_acquire()
                    .map_err(|_| err("too many claims already waiting on the leader; retry"))?,
            ),
            _ => None,
        };
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
                Ok(r) if r.status().is_success() => {
                    self.mark_leader_seen();
                    return Ok(r.text().await?);
                }
                // An answer, not a transport failure: retrying cannot change it, and 421 in
                // particular means this node's idea of who leads has gone stale.
                Ok(r) => return Err(err(format!("own/{what}: leader answered {}", r.status()))),
                Err(e) => last = e.into(),
            }
        }
        Err(last)
    }

    // ---- The leader's side of the three messages. Only ever reached on pod zero. ----

    pub async fn grant_claim(&self, repo: &str, asker: &str, force: bool) -> Result<Grant> {
        // Serialize every leader read-modify-write: concurrent claims/renews/prunes on the same
        // repo could otherwise both read a stale map and both write, granting one repo to two
        // nodes — which fences the loser's live database. One process, one lock: cheap and total.
        // This makes the compare-and-set below genuinely atomic, not just advertised as one.
        let _g = self.leader_lock.lock().await;
        let now = self.now_ms();
        // Pod zero stores the lease; it does not hold repositories. When it is the one asking, it
        // hands the repo to the least loaded server instead of taking it, so a leader restart never
        // orphans a repo. Any other asker is granted what it asked for.
        let asker = if asker == self.leader() {
            let servers = ownership::servers(asker, &self.server_prefix, self.replicas);
            let draining = self.ownership.draining().await.unwrap_or_default();
            match ownership::least_loaded(&servers, &self.ownership.all().await?, &draining, now) {
                Some(n) => n,
                None => return Err(err("no server available to hold this repo".to_string())),
            }
        } else {
            asker.to_string()
        };
        let asker = asker.as_str();
        // Either way this is a genuinely serialized leader-mediated compare-and-set: only pod zero
        // writes the map, and `leader_lock` above means a force-claim is one node's decision made
        // atomically in one place, never a local override or a race with a concurrent asker.
        let cur = self.ownership.get(repo).await?;
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
            self.ownership.put(repo, e).await?;
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
        let now = self.now_ms();
        let mut lost = Vec::new();
        let mut renewed = Vec::new();
        for repo in repos {
            match ownership::decide_renew(self.ownership.get(repo).await?.as_ref(), asker, now) {
                Some(e) => renewed.push((repo.clone(), e)),
                None => lost.push(repo.clone()),
            }
        }
        self.ownership.put_many(&renewed).await?;
        Ok(lost)
    }

    pub async fn grant_release(&self, repo: &str, asker: &str) -> Result<()> {
        let _g = self.leader_lock.lock().await;
        if ownership::may_release(self.ownership.get(repo).await?.as_ref(), asker) {
            self.ownership.delete(repo).await?;
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
        if !matches!(self.route(&format!("{owner}/{name}")).await, Route::Local) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use slatedb::object_store::memory::InMemory;
    use slatedb::object_store::ObjectStore;

    async fn test_app(name: &str) -> App {
        let os: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let tmp = tempfile::tempdir().unwrap();
        let store =
            Arc::new(store::Store::open(os.clone(), tmp.path().join("cache"), false).await.unwrap());
        // Leaked so the App can outlive this helper's tempdir binding without the test wiring a
        // Node like tests/routing.rs does.
        std::mem::forget(tmp);
        let ownership = OwnershipStore::open(os, true).await.unwrap();
        App::new(store, Arc::new(ownership), name.into(), Arc::new(|_: &str| "127.0.0.1:1".into()), "test-secret".into(), 1)
    }

    /// What `/healthz` proves on a follower: a fresh node is NOT ready until the leader has
    /// answered it once, stays ready for `LEADER_SILENCE` after the last answer, and goes
    /// un-ready past it. The leader is always ready to itself.
    #[tokio::test]
    async fn leader_reachable_follows_the_last_beat() {
        let leader = test_app("rustic-git-0").await;
        assert!(leader.is_leader() && leader.leader_reachable());

        let follower = test_app("rustic-git-1").await;
        assert!(!follower.is_leader());
        assert!(!follower.leader_reachable(), "no beat yet: a rolled pod must not take traffic");
        follower.mark_leader_seen();
        assert!(follower.leader_reachable());
        follower.advance_clock(LEADER_SILENCE - std::time::Duration::from_millis(1));
        assert!(follower.leader_reachable());
        follower.advance_clock(std::time::Duration::from_millis(1));
        assert!(!follower.leader_reachable(), "a leader roll's worth of silence: un-ready");
    }

    /// A cold claim waits out a leader roll (~30 s of retries). With the gate full, one more
    /// fails at once instead of pinning another task for that long — the fast 503.
    #[tokio::test]
    async fn a_claim_past_the_gate_fails_fast() {
        let follower = test_app("rustic-git-1").await; // the leader is an unreachable port
        let _held = follower.claim_gate.acquire_many(MAX_WAITING_CLAIMS as u32).await.unwrap();
        let t = std::time::Instant::now();
        let err = follower.claim("alice/cold").await.expect_err("must not be granted");
        assert!(err.to_string().contains("too many claims"), "{err}");
        assert!(t.elapsed() < std::time::Duration::from_millis(500), "must not enter the retry loop");
    }
}
