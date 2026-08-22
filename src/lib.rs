pub mod api;
pub mod auth;
pub mod browse;
pub mod cache;
pub mod config;
pub mod events;
pub mod gc;
pub mod gpg;
pub mod objects;
pub mod http;
pub mod index;
pub mod jwt;
pub mod ownership;
pub mod pktline;
pub mod pool;
pub mod protocol;
pub mod proxy;
pub mod refs;
pub mod registry;
pub mod ssh;
pub mod store;
pub mod directory;
pub mod pulls;

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
    // ponytail: unbounded map; entries are one Instant per repo ever recovered, and a repo count
    // that makes this matter is a bigger problem elsewhere first.
    pub recovery_asked: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
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
    /// repo -> when `route` first found it missing from the object store. Checked before the
    /// `pool.exists` LIST in `route`, pre-auth, so a spray of nonexistent repo names costs one
    /// LIST per name per TTL instead of one per request.
    // ponytail: 5s negative cache; a repo created within the window still 404s briefly —
    // acceptable, it's just-created. Expired entries are swept on insert (see `neg_cache_miss`),
    // so a spray of distinct names cannot grow this without bound.
    neg_cache: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    /// Mongo, for the ONE thing an owning node still needs it for: copying a repo's pre-existing
    /// pull requests into its own database on first touch (`pulls::ensure_migrated`). Resolved
    /// state, not an `Option`: "not configured" is safe to migrate as empty, "configured but
    /// unreachable" must not be, and a pair of fields could hold the nonsensical combination.
    pub dir: pulls::Source,
}

/// How long after asking the leader about a repo this node will not ask again for the same repo.
pub const RECOVERY_ASK_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// Pacing between repos in the visibility repair lane, mirroring the gc sweep's per-owner gap:
/// the lane is a backstop, not a deadline, so it yields object-store bandwidth to real requests.
const RECONCILE_GAP: std::time::Duration = std::time::Duration::from_millis(200);

/// How long `route` trusts a "repo does not exist" verdict before asking the object store again.
const NEG_TTL: std::time::Duration = std::time::Duration::from_secs(5);

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
            jwt: Arc::new(jwt::Jwt::new(&jwt_secret).expect("jwt secret")),
            leader_lock: tokio::sync::Mutex::new(()),
            neg_cache: Default::default(),
            dir: pulls::Source::Absent,
        }
    }

    /// The directory this node migrates pull requests from. Set once at startup, before the `App`
    /// is shared; there is no path that changes it later.
    pub fn with_directory(mut self, dir: pulls::Source) -> Self {
        self.dir = dir;
        self
    }

    /// `true` if `repo` was recorded missing within the last `NEG_TTL`. Evicts the entry lazily
    /// (like `cache::memory`'s `mem_get`) rather than sweeping, since one Instant per repo ever
    /// found missing is cheap and the map only grows as fast as distinct bad names arrive.
    fn neg_cache_hit(&self, repo: &str) -> bool {
        let mut m = self.neg_cache.lock().unwrap();
        match m.get(repo) {
            Some(t) if t.elapsed() < NEG_TTL => true,
            Some(_) => {
                m.remove(repo);
                false
            }
            None => false,
        }
    }

    /// Record that `repo` does not exist right now. Only ever called for a negative `exists()`
    /// result — a positive is never cached, so a repo that gets created is visible the moment
    /// ownership or the store says so.
    ///
    /// Lazy eviction alone only reclaims a name that is asked for twice, so a spray of DISTINCT
    /// bad names — which is exactly the unauthenticated traffic this cache exists to absorb —
    /// would grow the map instead. Expired entries are swept here, on insert, whenever the map has
    /// grown past a size no honest workload reaches: every entry older than the TTL is dead by
    /// definition, so this is a cheap scan that cannot drop a live one.
    fn neg_cache_miss(&self, repo: &str) {
        const SWEEP_AT: usize = 1024;
        let mut m = self.neg_cache.lock().unwrap();
        if m.len() >= SWEEP_AT {
            m.retain(|_, t| t.elapsed() < NEG_TTL);
        }
        m.insert(repo.to_string(), std::time::Instant::now());
    }

    /// Who owns this repo, from this node's own copy of the map. No network: a follower's
    /// read-only handle answers, however stale it is — a stale read costs a hop, never an owner.
    pub async fn owner(&self, repo: &str) -> Result<Option<Entry>> {
        self.ownership.get(repo).await
    }

    fn leader(&self) -> Result<String> {
        Ok(self.leader_name.clone())
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
                    if self.neg_cache_hit(repo) {
                        return Route::Local;
                    }
                    if !self.store.pool.exists(o, n).await.unwrap_or(true) {
                        self.neg_cache_miss(repo);
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

    /// One pass of the visibility repair lane: for every repo/image this node holds open, move
    /// its listing marker back onto what the repo's own database says. `open_repo`'s lazy repair
    /// only fires when someone touches a repo; a repo nobody clones or browses — and every
    /// pre-existing repo, which has no marker at all until the structural sweep writes a
    /// fail-closed PRIVATE one — would otherwise stay missing from listings forever.
    ///
    /// `warm_repos()` is the ownership set on purpose: it names only databases THIS node has
    /// open, so the lane can never open a repo owned elsewhere and fence its owner. Repairs are
    /// paced by `RECONCILE_GAP` for the same reason the gc sweep paces its owners — this is a
    /// backstop, and it must not compete with request traffic for object-store bandwidth.
    /// Log-and-continue per repo: a marker is a view, not authorization, so one unreadable repo
    /// is not a reason to leave the rest drifting.
    pub async fn reconcile_owned_markers(&self) {
        for key in self.store.pool.warm_repos() {
            let (kind, rest) = match key.strip_prefix("img/") {
                Some(rest) => (index::Kind::Img, rest),
                None => (index::Kind::Repo, key.as_str()),
            };
            let Some((owner, name)) = rest.split_once('/') else { continue };
            if let Err(e) = self.store.reconcile_marker(owner, name, kind).await {
                eprintln!("reconcile marker {owner}/{name}: {e}"); // ponytail: eprintln
            }
            tokio::time::sleep(RECONCILE_GAP).await;
        }
    }

    /// Recompute mergeability for the open changes in every repo this node has warm.
    ///
    /// THE SAFETY FLOOR for merge work, and it needs no Redis and no Mongo — which is the whole
    /// reason discovery moved here. A repo's changes live in its own database, and opening that on
    /// any other node fences this one, so the owner is the only party that may go looking. A lost
    /// stream event now costs latency, never a check.
    ///
    /// Warm repos only, exactly like `reconcile_owned_markers`: a repo nobody has opened has no
    /// reader waiting on the answer either. A repo whose Mongo changes have not been migrated yet
    /// has an empty `pull/` prefix and is silently a no-op — the first routed touch migrates it,
    /// and this lane picks it up on the next pass.
    ///
    /// Log-and-continue per repo, paced by `RECONCILE_GAP` for the same reason the marker lane is:
    /// a backstop must yield bandwidth to real requests rather than compete with them.
    pub async fn check_owned_pulls(&self) {
        for key in self.store.pool.warm_repos() {
            // Images have no pull requests; `repo/img/...` shares the pool with repos.
            if key.starts_with("img/") {
                continue;
            }
            let Some((owner, name)) = key.split_once('/') else { continue };
            if let Err(e) = pulls::check_repo(&self.store, owner, name).await {
                eprintln!("checking mergeability for {owner}/{name}: {e}"); // ponytail: eprintln
            }
            tokio::time::sleep(RECONCILE_GAP).await;
        }
    }

    /// How long a claimed merge may sit before this node assumes the claimant is gone and takes
    /// it again. Generous: a merge on a large tree is real work, and re-running one that is still
    /// in flight is worse than waiting.
    const MERGE_LEASE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

    /// Perform the merges the repos this node owns have waiting.
    ///
    /// Merge EXECUTION was already here — the worker's only job was to POST `/api/…/merge` at
    /// this node, because moving a ref has exactly one legitimate writer. What moved is the
    /// orchestration around it: the job hangs off a change, a change lives in its repo's own
    /// database, and no other process may read that. So the owner claims its own work and calls
    /// `merge::perform` directly rather than making an HTTP round trip to itself.
    ///
    /// Warm repos only and log-and-continue per repo, exactly like the two lanes above. One
    /// claim per repo per pass: a repo with several queued merges lands the rest on later passes,
    /// which keeps one busy repo from monopolising the lane.
    pub async fn merge_owned_pulls(&self) {
        for key in self.store.pool.warm_repos() {
            if key.starts_with("img/") {
                continue;
            }
            let Some((owner, name)) = key.split_once('/') else { continue };
            match pulls::claim_merge(&self.store, owner, name, Self::MERGE_LEASE, &self.self_name)
                .await
            {
                Ok(Some(pr)) => self.run_merge(owner, name, pr).await,
                Ok(None) => continue, // nothing waiting; no reason to pace
                Err(e) => eprintln!("claiming a merge in {owner}/{name}: {e}"), // ponytail: eprintln
            }
            tokio::time::sleep(RECONCILE_GAP).await;
        }
    }

    /// One claimed merge, landed or refused, with the outcome written back.
    async fn run_merge(&self, owner: &str, name: &str, pr: pulls::PullRequest) {
        let Some(job) = pr.merge.clone() else { return };
        let message = format!("{} (#{})\n", pr.title, pr.number);
        let out = crate::http::browse_api::merge::perform(
            self,
            owner,
            name,
            &pr.base,
            &pr.head,
            &job.strategy,
            Some(message),
        )
        .await;
        let merged = out.is_ok();
        let record = match out {
            Ok(_) => {
                // State first, then the job: a crash between them leaves a merged change with a
                // stale job on it, which someone can see and clear. The other order would show a
                // change that merged nothing.
                if let Err(e) = pulls::set_state(&self.store, owner, name, pr.number, pulls::PullState::Merged).await {
                    eprintln!("recording {owner}/{name}#{}: {e}", pr.number); // ponytail: eprintln
                }
                pulls::clear_merge(&self.store, owner, name, pr.number).await.err()
            }
            // The fleet's own words — "behind its base", or the protection rule that refused it —
            // written for the person waiting rather than replaced with a generic failure.
            Err((code, why)) => {
                let state = if code == axum::http::StatusCode::CONFLICT {
                    directory::MergeState::Conflicts
                } else {
                    directory::MergeState::Failed
                };
                pulls::finish_merge(&self.store, owner, name, pr.number, state, Some(why.trim()))
                    .await
                    .err()
            }
        };
        if let Some(e) = record {
            eprintln!("recording the merge of {owner}/{name}#{}: {e}", pr.number); // ponytail: eprintln
            return;
        }
        if merged {
            // Two nudges, not one: `PullMerged` is the change's own timeline event, while
            // `HeadMoved` says the base branch tip moved — which is what makes every OTHER open
            // change against the same base worth re-checking.
            let now = ownership::now_ms() as i64;
            let mut ev = events::Event {
                kind: events::Kind::PullMerged,
                repo: format!("{owner}/{name}"),
                number: pr.number,
                actor: job.requested_by.clone(),
                at_ms: now,
                title: pr.title.clone(),
                base: pr.base.clone(),
                head: pr.head.clone(),
            };
            events::publish(&self.store.cache, &ev).await;
            // Repo-wide, so it carries no change and no branches — see `events::Event::number`.
            ev.kind = events::Kind::HeadMoved;
            ev.number = 0;
            ev.title = String::new();
            ev.base = String::new();
            ev.head = String::new();
            events::publish(&self.store.cache, &ev).await;
        }
    }

    /// Leader only: drop entries whose lease lapsed without a release — the node holding them died
    /// or was partitioned away. Keeps the map bounded by what is actually open.
    pub async fn prune_once(&self) -> Result<()> {
        let _g = self.leader_lock.lock().await;
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
        // Serialize every leader read-modify-write: concurrent claims/renews/prunes on the same
        // repo could otherwise both read a stale map and both write, granting one repo to two
        // nodes — which fences the loser's live database. One process, one lock: cheap and total.
        // This makes the compare-and-set below genuinely atomic, not just advertised as one.
        let _g = self.leader_lock.lock().await;
        let now = ownership::now_ms();
        // Pod zero stores the lease; it does not hold repositories. When it is the one asking, it
        // hands the repo to the least loaded server instead of taking it, so a leader restart never
        // orphans a repo. Any other asker is granted what it asked for.
        let asker = if asker == self.leader()? {
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
            self.ownership.put(repo, e).await?;
        }
        Ok(g)
    }

    pub async fn grant_renew(&self, asker: &str, repos: &[String]) -> Result<Vec<String>> {
        let _g = self.leader_lock.lock().await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::BoxStream;
    use slatedb::object_store::memory::InMemory;
    use slatedb::object_store::path::Path as OsPath;
    use slatedb::object_store::{
        GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore, PutMultipartOptions,
        PutOptions, PutPayload, PutResult, Result as OsResult,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Wraps an in-memory store and counts `list` calls, so a test can observe how many LISTs
    /// `route`'s `pool.exists` probe actually issued — the thing the negative cache exists to cut.
    #[derive(Debug)]
    struct CountingStore {
        inner: InMemory,
        lists: AtomicUsize,
    }

    impl std::fmt::Display for CountingStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "CountingStore")
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for CountingStore {
        async fn put_opts(
            &self,
            location: &OsPath,
            payload: PutPayload,
            opts: PutOptions,
        ) -> OsResult<PutResult> {
            self.inner.put_opts(location, payload, opts).await
        }

        async fn put_multipart_opts(
            &self,
            location: &OsPath,
            opts: PutMultipartOptions,
        ) -> OsResult<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(location, opts).await
        }

        async fn get_opts(&self, location: &OsPath, options: GetOptions) -> OsResult<GetResult> {
            self.inner.get_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: BoxStream<'static, OsResult<OsPath>>,
        ) -> BoxStream<'static, OsResult<OsPath>> {
            self.inner.delete_stream(locations)
        }

        fn list(&self, prefix: Option<&OsPath>) -> BoxStream<'static, OsResult<ObjectMeta>> {
            self.lists.fetch_add(1, Ordering::SeqCst);
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(&self, prefix: Option<&OsPath>) -> OsResult<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &OsPath,
            to: &OsPath,
            options: slatedb::object_store::CopyOptions,
        ) -> OsResult<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    async fn counting_app() -> (Arc<CountingStore>, App) {
        let counting = Arc::new(CountingStore { inner: InMemory::new(), lists: AtomicUsize::new(0) });
        let os: Arc<dyn ObjectStore> = counting.clone();
        let tmp = tempfile::tempdir().unwrap();
        let store =
            Arc::new(store::Store::open(os.clone(), tmp.path().join("cache"), false).await.unwrap());
        // Leaked so the App (which needs a 'static-ish handle only via Arc clones) can outlive
        // this helper's tempdir binding without the test wiring a Node like tests/routing.rs does.
        std::mem::forget(tmp);
        let ownership = OwnershipStore::open(os, true).await.unwrap();
        let app = App::new(
            store,
            Arc::new(ownership),
            "rustic-git-0".into(),
            Arc::new(|_: &str| "127.0.0.1:1".into()),
            "test-secret".into(),
            1,
        );
        (counting, app)
    }

    /// The regression test for this task: five lookups of the same nonexistent repo inside the
    /// negative-cache TTL must cost the object store one LIST, not five.
    #[tokio::test]
    async fn repeated_missing_repo_lookups_hit_store_once() {
        let (counting, app) = counting_app().await;
        // Setup itself (opening the leader's ownership DB) issues its own LIST against the
        // object store; only count the LISTs `route` causes from here.
        let baseline = counting.lists.load(Ordering::SeqCst);
        for _ in 0..5 {
            let _ = app.route("ghost/repo").await;
        }
        assert_eq!(counting.lists.load(Ordering::SeqCst) - baseline, 1);
    }

    /// The cache-check helpers directly: a miss is remembered, a hit expires after `NEG_TTL`, and
    /// a repo never recorded is never a hit — the eviction/TTL logic the brief calls out.
    #[tokio::test]
    async fn neg_cache_expires_and_only_caches_recorded_misses() {
        let (_counting, app) = counting_app().await;
        assert!(!app.neg_cache_hit("nope/never-recorded"));

        app.neg_cache_miss("acme/gone");
        assert!(app.neg_cache_hit("acme/gone"));

        // Force expiry by back-dating the entry past NEG_TTL, then confirm it's evicted on lookup.
        app.neg_cache
            .lock()
            .unwrap()
            .insert("acme/gone".into(), std::time::Instant::now() - NEG_TTL - std::time::Duration::from_millis(1));
        assert!(!app.neg_cache_hit("acme/gone"));
        assert!(!app.neg_cache.lock().unwrap().contains_key("acme/gone"));
    }
}
