use rustic_git_core::jwt;
use rustic_git_core::peer as proxy;
use rustic_git_storage::{ownership, pool, store};
use rustic_git_pulls::pulls;

use ownership::lease::{self, Held, Lease, LEADER_TTL};
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
    /// Who holds the leader lease, as this node last read it. `None` until the first read, and
    /// again after a read finds the lease absent or expired — an unknown leader is found by
    /// re-reading the lease, never guessed from a name.
    leader: std::sync::Mutex<Option<String>>,
    /// The epoch of the lease THIS node holds; zero when it holds none. `is_leader()` is exactly
    /// `!= 0`, and every map write checks it under `leader_lock` — a leader mid-demotion stops
    /// granting in-process, before SlateDB's fence has to say so.
    leader_epoch: std::sync::atomic::AtomicU64,
    /// `now_ms()` when a LIVE lease was last read (any holder), and when that lease expires.
    /// `/healthz` reads both: readiness means "a leader exists", not "I am one".
    lease_seen_ms: std::sync::atomic::AtomicU64,
    lease_expires_ms: std::sync::atomic::AtomicU64,
    /// When the lease THIS node holds expires; zero when it holds none. Read at the top of every
    /// beat, BEFORE the lease read, because a leader cut off from the object store gets `Err` from
    /// that read and would otherwise keep granting for as long as the outage lasts — past its own
    /// expiry, while a peer that can reach the store has already taken over.
    held_expires_ms: std::sync::atomic::AtomicU64,
    /// Set by `resign`: this process is on its way out and must not take the lease again — the
    /// election beat keeps running through the drain, and a released lease is exactly the kind
    /// it would take.
    retiring: std::sync::atomic::AtomicBool,
    pub addr_of: AddrOf,
    pub forwarder: Arc<proxy::Forwarder>,
    /// When this node last asked the leader about a repo because a forward to its owner failed.
    /// A forward that fails is answered by asking the leader, and during a blip that touches many
    /// forwards at once every one of them would otherwise ask — a burst on pod zero at the moment
    /// it is least able to take one. One ask per repo per second is plenty: the answer does not
    /// change faster than that, and a request that arrives inside the window gets a plain 502 to
    /// retry, by which time the first ask has moved the map.
    // ponytail: unbounded map; entries are one u64 per repo ever recovered, and a repo count
    // that makes this matter is a bigger problem elsewhere first.
    pub recovery_asked: std::sync::Mutex<std::collections::HashMap<String, u64>>,
    /// Names this node has already asked the leader about and been told nobody owns, with the
    /// `now_ms()` of the answer. Without it a repeated invented name is one leader READ per
    /// request — cheaper than the map write it replaced, but at request rate rather than once per
    /// LEASE_TTL, which is the wrong direction for a path an anonymous client reaches.
    // ponytail: 4096 entries, swept on insert; a spray wider than that just gets less caching, and
    // an LRU is the upgrade if the sweep ever shows up in a profile.
    missing_seen: std::sync::Mutex<std::collections::HashMap<String, u64>>,
    /// How many times this node has asked the leader who owns a repo. Read by the routing tests to
    /// prove the negative cache actually saves the ask.
    pub owner_asks: std::sync::atomic::AtomicU64,
    /// Milliseconds added to this node's wall clock. Zero in production; a test advances it to
    /// age a lease entry or a recovery window without sleeping through it. Per node, not
    /// process-wide: the routing tests run many nodes in one process, and skewing them all
    /// would expire another test's drain lease under it.
    skew_ms: std::sync::atomic::AtomicU64,
    /// Mints and verifies registry bearer tokens (`/v2/token`). Keyed from
    /// `RUSTIC_GIT_JWT_SECRET` when set; otherwise a random per-process secret, which means
    /// tokens die with the process — fine for a dev run, and in a fleet it shows up as
    /// "log in again", never as a forged token being accepted.
    pub jwt: Arc<jwt::Jwt>,
    /// Serializes the leader's read-modify-write paths on the ownership map (grant_claim,
    /// grant_renew, grant_release, prune_once — and `demote`, so no grant is mid-write when the
    /// writer goes). Without it, two concurrent claims can both read
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
    /// `stored ?? env ?? default` for every central-tier tunable, swapped in by
    /// `rustic_git_core::settings::refresh_central_beat` every `SETTINGS_REFRESH_SECS`. Seeded
    /// from env alone at construction; `main.rs`'s boot sequence does one synchronous GET of
    /// `cluster/settings` and stores the merged result before serving anything, so the very
    /// first request already sees an admin-set value rather than waiting out the first beat.
    pub central: rustic_git_core::settings::LiveSettings<rustic_git_core::settings::CentralSettings>,
}

/// How long after asking the leader about a repo this node will not ask again for the same repo.
pub const RECOVERY_ASK_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// How long "the leader says nobody owns this" is believed. Well under `LEASE_TTL`, so a key that
/// someone really does claim is seen on the next window rather than after a lease's worth of 404s.
pub const MISSING_ASK_EVERY: std::time::Duration = std::time::Duration::from_secs(3);
/// The most names the negative cache remembers at once.
const MISSING_CACHE_MAX: usize = 4096;

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

impl App {
    pub fn new(
        store: Arc<store::Store>,
        ownership: Arc<OwnershipStore>,
        self_name: String,
        addr_of: AddrOf,
        peer_secret: String,
        // The directory this node migrates pull requests from. A parameter, not a builder:
        // it is fixed at startup and nothing changes it once the `App` is shared.
        dir: pulls::Source,
    ) -> Self {
        let jwt_secret = std::env::var("RUSTIC_GIT_JWT_SECRET").unwrap_or_else(|_| {
            use rand::Rng;
            rand::thread_rng()
                .sample_iter(rand::distributions::Alphanumeric)
                .take(48)
                .map(char::from)
                .collect()
        });
        // Solo: one node and no lease. It leads by construction — epoch 1, itself — so every
        // claim is local and nothing here ever reads the store.
        let (leader, epoch) = if ownership.is_solo() { (Some(self_name.clone()), 1) } else { (None, 0) };
        App {
            store,
            ownership,
            self_name,
            leader: std::sync::Mutex::new(leader),
            leader_epoch: std::sync::atomic::AtomicU64::new(epoch),
            lease_seen_ms: std::sync::atomic::AtomicU64::new(0),
            lease_expires_ms: std::sync::atomic::AtomicU64::new(0),
            held_expires_ms: std::sync::atomic::AtomicU64::new(0),
            retiring: std::sync::atomic::AtomicBool::new(false),
            addr_of,
            forwarder: Arc::new(proxy::Forwarder::new(peer_secret)),
            recovery_asked: Default::default(),
            missing_seen: Default::default(),
            owner_asks: Default::default(),
            skew_ms: std::sync::atomic::AtomicU64::new(0),
            jwt: Arc::new(jwt::Jwt::new(&jwt_secret).expect("jwt secret")),
            leader_lock: tokio::sync::Mutex::new(()),
            claim_gate: tokio::sync::Semaphore::new(MAX_WAITING_CLAIMS),
            dir,
            central: rustic_git_core::settings::LiveSettings::new(
                rustic_git_core::settings::CentralSettings::from_env(),
            ),
        }
    }

    /// Who owns this repo, from this node's own copy of the map. No network: a follower's
    /// read-only handle answers, however stale it is — a stale read costs a hop, never an owner.
    pub async fn owner(&self, repo: &str) -> Result<Option<Entry>> {
        self.ownership.get(repo).await
    }

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

    fn note_live(&self, l: &Lease) {
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
        if self.retiring.load(std::sync::atomic::Ordering::Relaxed) {
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
    async fn promote(&self, h: Held) -> Result<()> {
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
            tracing::info!(epoch = cur.lease.epoch, "lease: leading");
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
                tracing::warn!(error = %e, "releasing the leader lease; it lapses on its TTL");
            }
        }
        self.demote("shutdown").await;
    }

    /// `demote` for a caller already holding `leader_lock` (the grants).
    async fn demote_locked(&self, why: &str) {
        if !self.is_leader() {
            return;
        }
        tracing::warn!(epoch = self.leader_epoch(), why, "lease: demoting");
        self.leader_epoch.store(0, std::sync::atomic::Ordering::Relaxed);
        self.held_expires_ms.store(0, std::sync::atomic::Ordering::Relaxed);
        self.set_leader(None);
        self.ownership.demote().await;
        metrics::counter!("ownership_demotions_total").increment(1);
    }

    /// The epoch a map write is made under. Zero — not leading, or demoted since the handler
    /// checked — refuses: the in-process half of the fence, ahead of SlateDB's.
    fn writing_epoch(&self) -> Result<u64> {
        match self.leader_epoch() {
            0 => Err(err("not the leader")),
            e => Ok(e),
        }
    }

    /// A map operation's result, with a fence turned into a demotion. Caller holds `leader_lock`.
    async fn fenced_check<T>(&self, r: Result<T>) -> Result<T> {
        if let Err(e) = &r {
            if pool::is_fenced(e) {
                self.demote_locked("map write fenced").await;
            }
        }
        r
    }

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
                tracing::warn!(repo = %repo, error = %e, "ownership read failed; refusing to serve");
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
                        tracing::warn!(repo = %repo, error = %e, "claiming from the leader failed");
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
    fn route_to(&self, node: String) -> Route {
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
    fn may_ask_who_owns(&self, repo: &str) -> bool {
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
    fn note_no_owner(&self, repo: &str) {
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

    async fn ask_leader(&self, what: &str, body: String) -> Result<String> {
        self.ask_leader_with(what, body, Self::default_patience(what)).await
    }

    fn default_patience(what: &str) -> Patience {
        match what {
            "claim" => Patience::Claim,
            "release" => Patience::Release,
            _ => Patience::None,
        }
    }

    async fn ask_leader_with(&self, what: &str, body: String, patience: Patience) -> Result<String> {
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
    async fn refresh_leader(&self) -> Option<String> {
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
                tracing::warn!(error = %e, "reading the leader lease");
                self.leader()
            }
        }
    }

    // ---- The leader's side of the three messages. Only ever reached on the lease holder. ----

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
            tracing::debug!(repo = %repo, node = %e.node, epoch, "ownership: granted");
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

#[cfg(test)]
mod tests {
    use super::*;
    use ownership::lease::{self, LEADER_TTL};
    use slatedb::object_store::{memory::InMemory, path::Path, ObjectStore, ObjectStoreExt, PutPayload};

    fn mem() -> Arc<dyn ObjectStore> {
        Arc::new(InMemory::new())
    }

    /// A fleet node over a shared object store. Nothing is ticked here: each test decides when.
    async fn fleet_app(os: &Arc<dyn ObjectStore>, name: &str) -> App {
        let tmp = tempfile::tempdir().unwrap();
        let store =
            Arc::new(store::Store::open(os.clone(), tmp.path().join("cache"), false).await.unwrap());
        // Leaked so the App can outlive this helper's tempdir binding without the test wiring a
        // Node like tests/routing.rs does.
        std::mem::forget(tmp);
        App::new(
            store,
            Arc::new(OwnershipStore::open(os.clone())),
            name.into(),
            Arc::new(|_: &str| "127.0.0.1:1".into()),
            "test-secret".into(),
            pulls::Source::Absent,
        )
    }

    /// Write the lease object outright — what another node's put looks like from here.
    async fn plant(os: &Arc<dyn ObjectStore>, node: &str, epoch: u64, expires_ms: u64) {
        os.put(&Path::from(lease::PATH), PutPayload::from(format!("{node}\n{epoch}\n{expires_ms}").into_bytes()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_lone_node_takes_the_lease_and_leads() {
        let os = mem();
        let a = fleet_app(&os, "rustic-git-srv-0").await;
        assert!(!a.is_leader() && a.leader().is_none() && !a.leader_live());
        a.election_tick().await.unwrap();
        assert!(a.is_leader());
        assert_eq!(a.leader_epoch(), 1);
        assert_eq!(a.leader().as_deref(), Some("rustic-git-srv-0"));
        assert!(a.leader_live());
        assert!(a.ownership.is_writer().await);
        // A second tick renews rather than re-takes: same epoch, still the writer.
        a.election_tick().await.unwrap();
        assert_eq!(a.leader_epoch(), 1);
    }

    #[tokio::test]
    async fn a_second_node_follows_the_holder() {
        let os = mem();
        let a = fleet_app(&os, "rustic-git-srv-0").await;
        a.election_tick().await.unwrap();
        let b = fleet_app(&os, "rustic-git-srv-1").await;
        b.election_tick().await.unwrap();
        assert!(!b.is_leader());
        assert_eq!(b.leader().as_deref(), Some("rustic-git-srv-0"));
        assert!(b.leader_live());
        assert!(!b.ownership.is_writer().await);
    }

    #[tokio::test]
    async fn a_lease_taken_by_another_node_demotes() {
        let os = mem();
        let a = fleet_app(&os, "rustic-git-srv-0").await;
        a.election_tick().await.unwrap();
        plant(&os, "rustic-git-srv-1", 2, a.now_ms() + 5_000).await;
        a.election_tick().await.unwrap();
        assert!(!a.is_leader(), "somebody else holds a live lease at a newer epoch");
        assert_eq!(a.leader().as_deref(), Some("rustic-git-srv-1"));
        assert!(!a.ownership.is_writer().await);
        let e = a.grant_claim("alice/web", "rustic-git-srv-2", false).await.expect_err("demoted: must not grant");
        assert!(e.to_string().contains("not the leader"), "{e}");
    }

    #[tokio::test]
    async fn an_expired_lease_is_taken_with_the_next_epoch() {
        let os = mem();
        let a = fleet_app(&os, "rustic-git-srv-0").await;
        plant(&os, "rustic-git-srv-9", 5, a.now_ms() - 1).await;
        a.election_tick().await.unwrap();
        assert!(a.is_leader());
        assert_eq!(a.leader_epoch(), 6);
    }

    /// A pod that restarts keeps its name, and within one TTL the lease still names it. It resumes
    /// that lease rather than waiting for it to lapse — a restart must not cost ten seconds of
    /// "not the leader" answered to itself.
    #[tokio::test]
    async fn a_restarted_holder_resumes_its_own_live_lease() {
        let os = mem();
        let a = fleet_app(&os, "rustic-git-srv-0").await;
        plant(&os, "rustic-git-srv-0", 3, a.now_ms() + 5_000).await;
        a.election_tick().await.unwrap();
        assert!(a.is_leader());
        assert_eq!(a.leader_epoch(), 3);
        assert!(a.ownership.is_writer().await);
    }

    /// The storage-level fence, turned into a demotion: a stray writer on the map (another node
    /// that won the lease and opened it) makes this node's next map write fail, and that failure
    /// must strip its leadership rather than be reported as one bad grant.
    #[tokio::test]
    async fn a_fenced_map_write_demotes() {
        let os = mem();
        let a = fleet_app(&os, "rustic-git-srv-0").await;
        a.election_tick().await.unwrap();
        let stray = OwnershipStore::open(os.clone());
        stray.promote().await.unwrap();
        assert!(a.grant_claim("alice/web", "rustic-git-srv-1", false).await.is_err());
        assert!(!a.is_leader(), "a fenced writer is not the leader");
        assert!(!a.ownership.is_writer().await);
    }

    #[tokio::test]
    async fn grants_refuse_without_the_lease() {
        let os = mem();
        let a = fleet_app(&os, "rustic-git-srv-0").await;
        for r in [
            a.grant_claim("alice/web", "rustic-git-srv-1", false).await.map(|_| ()),
            a.grant_renew("rustic-git-srv-1", &["alice/web".into()]).await.map(|_| ()),
            a.grant_release("alice/web", "rustic-git-srv-1").await,
            a.prune_once().await,
        ] {
            let e = r.expect_err("no lease, no writes");
            assert!(e.to_string().contains("not the leader"), "{e}");
        }
    }

    /// What `/healthz` proves: a live leader exists — this node, or a lease read within
    /// `LEADER_TTL` that has not expired. The leader is always live to itself.
    #[tokio::test]
    async fn leader_live_follows_the_lease() {
        let os = mem();
        let a = fleet_app(&os, "rustic-git-srv-0").await;
        a.election_tick().await.unwrap();
        let b = fleet_app(&os, "rustic-git-srv-1").await;
        assert!(!b.leader_live(), "no lease read yet: a rolled pod must not take traffic");
        b.election_tick().await.unwrap();
        assert!(b.leader_live());
        b.advance_clock(LEADER_TTL + std::time::Duration::from_millis(1));
        assert!(!b.leader_live(), "the lease lapsed and nobody took it: un-ready");
        a.advance_clock(LEADER_TTL * 10);
        assert!(a.leader_live(), "the holder is live to itself until it is demoted");
    }

    /// A leader cut off from the object store cannot read the lease — and must still stop leading
    /// when the lease it holds runs out, because a peer that CAN reach the store has taken it.
    #[tokio::test]
    async fn a_leader_past_its_own_expiry_demotes_even_when_the_read_fails() {
        let os = mem();
        let a = fleet_app(&os, "rustic-git-srv-0").await;
        a.election_tick().await.unwrap();
        assert!(a.is_leader());
        // Unreadable lease: the beat's `lease::read` errors before it can tell us anything.
        os.put(&Path::from(lease::PATH), PutPayload::from("garbage".as_bytes().to_vec())).await.unwrap();
        a.advance_clock(LEADER_TTL);
        assert!(a.election_tick().await.is_err());
        assert!(!a.is_leader(), "expired and blind: must not keep granting");
        assert!(!a.ownership.is_writer().await);
    }

    /// `resign` sets `retiring`, but a beat already past that check can still be holding a take.
    /// It must not lead on the way out, and must not sit on the lease either.
    #[tokio::test]
    async fn a_retiring_node_does_not_promote_and_gives_the_lease_back() {
        let os = mem();
        let a = fleet_app(&os, "rustic-git-srv-0").await;
        let h = lease::take(os.as_ref(), "rustic-git-srv-0", a.now_ms(), None).await.unwrap().unwrap();
        a.retiring.store(true, std::sync::atomic::Ordering::Relaxed);
        a.promote(h).await.unwrap();
        assert!(!a.is_leader());
        assert!(!a.ownership.is_writer().await);
        let cur = lease::read(os.as_ref()).await.unwrap().unwrap();
        assert!(lease::is_expired(&cur.lease, a.now_ms()), "released, so the next node takes it at once");
    }

    /// A connect failure re-reads the lease before the next attempt: the name this node had was a
    /// tick old, and a failover has to finish inside the asker's patience, not the loop's cadence.
    /// Every address here is a refused port, so the ask never succeeds — what is asserted is what
    /// the node BELIEVES afterwards.
    #[tokio::test]
    async fn a_failed_ask_re_reads_the_lease() {
        let os = mem();
        let b = fleet_app(&os, "rustic-git-srv-1").await;
        b.set_leader(Some("ghost"));
        assert!(b.claim_to_recover("alice/web").await.is_err()); // two quick tries, 250 ms apart
        assert_eq!(b.leader(), None, "the lease is absent: nobody leads, and 'ghost' is forgotten");

        plant(&os, "rustic-git-srv-0", 4, b.now_ms() + 5_000).await;
        assert!(b.claim_to_recover("alice/web").await.is_err());
        assert_eq!(b.leader().as_deref(), Some("rustic-git-srv-0"), "re-read on the failed connect");
        assert!(b.leader_live());
    }

    /// Solo: one node, no lease, no store traffic. It leads by construction.
    #[tokio::test]
    async fn a_solo_node_leads_without_a_lease() {
        let os = mem();
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(store::Store::open(os.clone(), tmp.path().join("cache"), false).await.unwrap());
        std::mem::forget(tmp);
        let a = App::new(store, Arc::new(OwnershipStore::solo()), "rustic-git-0".into(), Arc::new(|_: &str| "127.0.0.1:1".into()), "s".into(), pulls::Source::Absent);
        assert!(a.is_leader() && a.leader_live());
        a.election_tick().await.unwrap();
        assert!(lease::read(os.as_ref()).await.unwrap().is_none(), "solo never writes a lease");
    }

    /// A cold claim waits out a leader roll (~30 s of retries). With the gate full, one more
    /// fails at once instead of pinning another task for that long — the fast 503.
    #[tokio::test]
    async fn a_claim_past_the_gate_fails_fast() {
        let os = mem();
        let follower = fleet_app(&os, "rustic-git-srv-1").await; // nobody leads; the addr is a refused port
        let _held = follower.claim_gate.acquire_many(MAX_WAITING_CLAIMS as u32).await.unwrap();
        let t = std::time::Instant::now();
        let err = follower.claim("alice/cold").await.expect_err("must not be granted");
        assert!(err.to_string().contains("too many claims"), "{err}");
        assert!(t.elapsed() < std::time::Duration::from_millis(500), "must not enter the retry loop");
    }
}
