pub mod auth;
pub mod gc;
pub mod http;
pub mod ownership;
pub mod pktline;
pub mod pool;
pub mod protocol;
pub mod proxy;
pub mod refs;
pub mod ssh;
pub mod store;

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
}

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
                        // hold the database open, keep serving it. A grant only ever comes from the
                        // leader, so an unreachable leader means nobody else can have been granted
                        // this repo either — and we are still holding it, so continuing cannot
                        // produce a second writer. During a roll pod zero updates last, which ages
                        // out every entry; refusing here would 503 warm repos fleet-wide for the
                        // length of the restart, and buy nothing. A cold repo, or one named to
                        // someone else, is still Unavailable.
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
            // load-balancer traffic flowing) but never serves what it does.
            if self.store.healthy() {
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
    pub async fn claim(&self, repo: &str) -> Result<Grant> {
        if self.is_leader() {
            return self.grant_claim(repo, &self.self_name.clone()).await;
        }
        let body = format!("{repo}\n{}", self.self_name);
        let reply = self.ask_leader("claim", body).await?;
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
        Ok(reply.lines().filter(|l| !l.is_empty()).map(String::from).collect())
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

    async fn ask_leader(&self, what: &str, body: String) -> Result<String> {
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
        let attempts = if what == "claim" { proxy::CLAIM_ATTEMPTS } else { 1 };
        let mut last = err("the leader was unreachable");
        for attempt in 0..attempts {
            if attempt > 0 {
                tokio::time::sleep(proxy::CLAIM_BACKOFF).await;
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

    pub async fn grant_claim(&self, repo: &str, asker: &str) -> Result<Grant> {
        let now = ownership::now_ms();
        // Pod zero stores the lease; it does not hold repositories. When it is the one asking, it
        // hands the repo to the least loaded server instead of taking it, so a leader restart never
        // orphans a repo. Any other asker is granted what it asked for.
        let asker = if asker == self.leader()? {
            let servers = ownership::servers(asker, self.replicas);
            match ownership::least_loaded(&servers, &self.ownership.all().await?, now) {
                Some(n) => n,
                None => return Err(err("no server available to hold this repo".to_string())),
            }
        } else {
            asker.to_string()
        };
        let asker = asker.as_str();
        let g = ownership::decide_claim(self.ownership.get(repo).await?.as_ref(), asker, now);
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
