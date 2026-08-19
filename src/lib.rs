pub mod api;
pub mod auth;
pub mod browse;
pub mod cache;
pub mod gc;
pub mod http;
pub mod jwt;
pub mod ownership;
pub mod pktline;
pub mod pool;
pub mod protocol;
pub mod proxy;
pub mod refs;
pub mod ssh;
pub mod store;
pub mod directory;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

pub fn err(msg: impl Into<String>) -> Error {
    msg.into().into()
}

use ownership::{Entry, Grant, OwnershipStore, Route};
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
    /// This pod's own name, e.g. `rustic-git-2`. The leader is derived from it, never configured.
    pub self_name: String,
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
    // ponytail: unbounded map; entries are one Instant per repo ever recovered, and a repo count
    // that makes this matter is a bigger problem elsewhere first.
    pub recovery_asked: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
}

/// How long after asking the leader about a repo this node will not ask again for the same repo.
pub const RECOVERY_ASK_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// Eviction gives the lease back before the database closes. `Pool` calls this; it holds a `Weak`
/// so this reference back into `App` is not a cycle.
impl pool::ReleaseHook for App {
    fn release(&self, repo: String) -> futures::future::BoxFuture<'_, ()> {
        // The pool has already marked the entry releasing, so a failure here is not fatal: the
        // lease simply lapses on its own TTL instead of the drain. Log and close anyway.
        Box::pin(async move {
            if let Err(e) = App::release(self, &repo).await {
                eprintln!("releasing {repo}: {e}"); // ponytail: eprintln
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
        replicas: u32,
    ) -> Self {
        App {
            store,
            ownership,
            self_name,
            addr_of,
            forwarder: Arc::new(proxy::Forwarder::new(peer_secret)),
            replicas,
            recovery_asked: Default::default(),
        }
    }

    /// Who owns this repo, from this node's own copy of the map. No network: a follower's
    /// read-only handle answers, however stale it is — a stale read costs a hop, never an owner.
    pub async fn owner(&self, repo: &str) -> Result<Option<Entry>> {
        self.ownership.get(repo).await
    }

    fn leader(&self) -> Result<String> {
        ownership::leader_of(&self.self_name)
    }

    /// Leadership is a name, not a decision — there is nothing here that two nodes could answer
    /// differently, which is the whole point of the design.
    pub fn is_leader(&self) -> bool {
        self.leader().map(|l| l == self.self_name).unwrap_or(false)
    }

    /// Where this request belongs.
    ///
    /// Read the map; if it names someone and the lease is live, that is the answer. Otherwise ask
    /// the leader — and if the leader cannot be reached, answer `Unavailable`. **Never serve on a
    /// failed claim**: falling back to "well, serve it here" is failover to whoever asked first,
    /// which is the two-writer bug this design exists to remove.
    pub async fn route(&self, repo: &str) -> Route {
        let now = ownership::now_ms();
        let entry = match self.owner(repo).await {
            Ok(c) => c,
            // The map is unreadable from here. We know nothing, so we may not serve.
            Err(e) => {
                eprintln!("ownership read for {repo}: {e}"); // ponytail: eprintln
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
                // A repo that does not exist is never claimed. This runs before authentication
                // (deliberately — the damage a wrong route does is opening a database on the wrong
                // node), so claiming here would let an unauthenticated caller drive a leader round
                // trip and a durable write into the map for any name it invents. The handler
                // produces its normal 404 locally, touching nothing. An error from `exists` falls
                // back to claiming: better a needless claim than a 404 on a real repo.
                if let Some((o, n)) = repo.split_once('/') {
                    if !self.store.pool.exists(o, n).await.unwrap_or(true) {
                        return Route::Local;
                    }
                }
                match self.claim(repo).await {
                    Ok(Grant::Granted(e)) | Ok(Grant::HeldBy(e)) => e.node,
                    Err(e) => {
                        eprintln!("claiming {repo}: {e}"); // ponytail: eprintln
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

    /// Ask for this repo. On the leader that is a local decision and a write; anywhere else it is
    /// one POST to the leader's peer port.
    /// Whether this node may ask the leader about `repo` on a failed forward right now, recording
    /// the ask if so. See `recovery_asked`.
    pub fn may_ask_to_recover(&self, repo: &str) -> bool {
        let now = std::time::Instant::now();
        let mut m = self.recovery_asked.lock().unwrap();
        match m.get(repo) {
            Some(t) if now.duration_since(*t) < RECOVERY_ASK_EVERY => false,
            _ => {
                m.insert(repo.to_string(), now);
                true
            }
        }
    }

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
    /// The same guards as the claim path in `route()`: an unhealthy or departing node must not take
    /// a repo it cannot serve, and a repo that does not exist is never claimed — routing runs
    /// before authentication, so without that check an unauthenticated caller could drive leader
    /// writes for any name it invents. `exists` erring falls back to asking, as it does there.
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
        if repos.is_empty() {
            return Ok(Vec::new());
        }
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
            eprintln!("lost the lease on {repo}: closing it"); // ponytail: eprintln
            if let Some((o, n)) = repo.split_once('/') {
                self.store.pool.evict(o, n).await;
            }
        }
        Ok(())
    }

    /// Leader only: drop entries whose lease lapsed without a release — the node holding them died
    /// or was partitioned away. Keeps the map bounded by what is actually open.
    pub async fn prune_once(&self) -> Result<()> {
        let now = ownership::now_ms();
        for (repo, e) in self.ownership.all().await? {
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
        let leader = self.leader()?;
        let addr = (self.addr_of)(&leader);
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
                Ok(r) if r.status().is_success() => return Ok(r.text().await?),
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
        let now = ownership::now_ms();
        // Pod zero stores the lease; it does not hold repositories. When it is the one asking, it
        // hands the repo to the least loaded server instead of taking it, so a leader restart never
        // orphans a repo. Any other asker is granted what it asked for.
        let asker = if asker == self.leader()? {
            let servers = ownership::servers(asker, self.replicas);
            let draining = self.ownership.draining().await.unwrap_or_default();
            match ownership::least_loaded(&servers, &self.ownership.all().await?, &draining, now) {
                Some(n) => n,
                None => return Err(err("no server available to hold this repo".to_string())),
            }
        } else {
            asker.to_string()
        };
        let asker = asker.as_str();
        // Either way this is a leader-mediated compare-and-set: only pod zero writes the map, so a
        // force-claim is still one node's decision made in one place, never a local override.
        let cur = self.ownership.get(repo).await?;
        let g = if force {
            ownership::decide_force_claim(cur.as_ref(), asker, now)
        } else {
            ownership::decide_claim(cur.as_ref(), asker, now)
        };
        if let Grant::Granted(e) = &g {
            self.ownership.put(repo, e).await?;
        }
        Ok(g)
    }

    pub async fn grant_renew(&self, asker: &str, repos: &[String]) -> Result<Vec<String>> {
        let now = ownership::now_ms();
        let mut lost = Vec::new();
        for repo in repos {
            match ownership::decide_renew(self.ownership.get(repo).await?.as_ref(), asker, now) {
                Some(e) => self.ownership.put(repo, &e).await?,
                None => lost.push(repo.clone()),
            }
        }
        Ok(lost)
    }

    pub async fn grant_release(&self, repo: &str, asker: &str) -> Result<()> {
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
        if !matches!(self.route(&format!("{owner}/{name}")).await, Route::Local) {
            return false;
        }
        // Pool::get never reopens a fenced handle by itself (that is the amplifier this exists to
        // remove). Routing says we still own it, so evict here — the retry's Pool::get then opens
        // fresh and takes the writer epoch back. Without this the retry gets a second FencedError.
        self.store.pool.evict(owner, name).await;
        true
    }
}
