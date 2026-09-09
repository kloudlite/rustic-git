//! `App` is one struct, its methods in three files: construction and `may_act` here, the leader
//! election and drain in `election`, repo routing and claims in `routing`.

use kloudlite_core::jwt;
use kloudlite_core::peer as proxy;
use kloudlite_storage::{ownership, pool, store};
use kloudlite_pulls::pulls;

use ownership::lease::{self, Held, Lease, LEADER_TTL};
use ownership::{Entry, Grant, OwnershipStore, Route};
use kloudlite_core::{err, Result};
use std::sync::Arc;

mod election;
mod routing;

/// Resolves a node name (`kloudlite-1`) to the address of its peer HTTP listener. In production
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
    /// This pod's own name, e.g. `kloudlite-2`.
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
    /// Set by `drain`: this pod is leaving and must take NO new ownership, while the repos it
    /// still holds keep serving until each is handed to a peer. Distinct from `retiring` (which
    /// only concerns the leader lease) and from `pool.is_closed()` (which is the end of the
    /// shutdown, after which nothing here serves at all). `/healthz` reports it as not-ready.
    draining: std::sync::atomic::AtomicBool,
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
    /// `KLOUDLITE_JWT_SECRET` when set; otherwise a random per-process secret, which means
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
    /// `kloudlite_core::settings::refresh_central_beat` every `SETTINGS_REFRESH_SECS`. Seeded
    /// from env alone at construction; `main.rs`'s boot sequence does one synchronous GET of
    /// `cluster/settings` and stores the merged result before serving anything, so the very
    /// first request already sees an admin-set value rather than waiting out the first beat.
    pub central: kloudlite_core::settings::LiveSettings<kloudlite_core::settings::CentralSettings>,
    /// `may_act` answers, so a fetch or a push is not a directory round trip per connection.
    /// ponytail: cleared wholesale at the cap, no LRU — same shape and the same numbers as
    /// `kloudlite_api::browse::Membership`, which explains why a minute of grace is tolerable.
    membership: std::sync::Mutex<std::collections::HashMap<(String, String), (bool, std::time::Instant)>>,
}

/// How long a membership answer is believed. Matches `kloudlite_api::browse`.
const MEMBERSHIP_TTL: std::time::Duration = std::time::Duration::from_secs(60);
const MEMBERSHIP_CAP: usize = 10_000;

/// How long after asking the leader about a repo this node will not ask again for the same repo.
pub const RECOVERY_ASK_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// How long "the leader says nobody owns this" is believed. Well under `LEASE_TTL`, so a key that
/// someone really does claim is seen on the next window rather than after a lease's worth of 404s.
pub const MISSING_ASK_EVERY: std::time::Duration = std::time::Duration::from_secs(3);
/// The most names the negative cache remembers at once.
const MISSING_CACHE_MAX: usize = 4096;

/// See `App::claim_gate`.
pub const MAX_WAITING_CLAIMS: usize = 64;

/// The whole handover's budget, inside the endpoint's own 30s bound and well inside the pod's 90s
/// grace period. Whatever is not handed over by then is left owned and released by SIGTERM.
pub const DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(25);
/// One repo's share of it. `release` alone can retry for ~21s against an unreachable leader
/// (`Patience::Release`), which would spend the whole budget on the first of sixteen repos.
pub const PER_REPO: std::time::Duration = std::time::Duration::from_secs(5);

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
                tracing::warn!(repo = %repo, reason = "evict", error = %e, "ownership.release.failed");
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
        let jwt_secret = std::env::var("KLOUDLITE_JWT_SECRET").unwrap_or_else(|_| {
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
            draining: std::sync::atomic::AtomicBool::new(false),
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
            central: kloudlite_core::settings::LiveSettings::new(
                kloudlite_core::settings::CentralSettings::from_env(),
            ),
            membership: Default::default(),
        }
    }

    /// May `user` (an email OR a handle) act under `owner` (a handle)? Their own handle, or a
    /// team they belong to. `Err` when the directory is configured but cannot answer — the caller
    /// refuses, never falls open. With no directory at all (single-node, `Source::Absent`) only
    /// the user's own handle matches, and "own handle" is then the fingerprint row's value itself:
    /// a solo deploy registers keys by handle and has no memberships to check.
    ///
    /// A handle principal is judged by the PERSON it names when the directory knows one: the
    /// registry passes handles (a PAT's owner and a registry token's `sub` are both handles), and
    /// judging a handle by equality alone refused every team member a push.
    pub async fn may_act(&self, user: &str, owner: &str) -> Result<bool> {
        match &self.dir {
            pulls::Source::Absent => Ok(user == owner),
            pulls::Source::Unavailable => Err(err("directory unavailable")),
            pulls::Source::Directory(d) => {
                // A handle the directory cannot place is judged by exactly the rule that stood
                // before: its own namespace and nothing else. That is the solo deploy, and the
                // pre-migration `auth/sshkey/{fp}` row that still names a handle.
                let user = match user.contains('@') {
                    true => user.to_string(),
                    false => match d
                        .user_by_handle(user)
                        .await
                        .map_err(|e| err(format!("directory: {e}")))?
                    {
                        Some(u) => u.email,
                        None => return Ok(user == owner),
                    },
                };
                let user = user.as_str();
                let key = (user.to_string(), owner.to_string());
                if let Some(yes) = self
                    .membership
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&key)
                    .filter(|(_, at)| at.elapsed() < MEMBERSHIP_TTL)
                    .map(|(yes, _)| *yes)
                {
                    return Ok(yes);
                }
                let own = d
                    .user(user)
                    .await
                    .map_err(|e| err(format!("directory: {e}")))?
                    .is_some_and(|u| u.username.as_deref() == Some(owner));
                let yes = own
                    || d.is_member(user, owner)
                        .await
                        .map_err(|e| err(format!("directory: {e}")))?;
                let mut m = self.membership.lock().unwrap_or_else(|e| e.into_inner());
                if m.len() >= MEMBERSHIP_CAP {
                    m.retain(|_, (_, at)| at.elapsed() < MEMBERSHIP_TTL);
                }
                m.insert(key, (yes, std::time::Instant::now()));
                Ok(yes)
            }
        }
    }

    /// Who owns this repo, from this node's own copy of the map. No network: a follower's
    /// read-only handle answers, however stale it is — a stale read costs a hop, never an owner.
    pub async fn owner(&self, repo: &str) -> Result<Option<Entry>> {
        self.ownership.get(repo).await
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
        let a = fleet_app(&os, "kloudlite-srv-0").await;
        assert!(!a.is_leader() && a.leader().is_none() && !a.leader_live());
        a.election_tick().await.unwrap();
        assert!(a.is_leader());
        assert_eq!(a.leader_epoch(), 1);
        assert_eq!(a.leader().as_deref(), Some("kloudlite-srv-0"));
        assert!(a.leader_live());
        assert!(a.ownership.is_writer().await);
        // A second tick renews rather than re-takes: same epoch, still the writer.
        a.election_tick().await.unwrap();
        assert_eq!(a.leader_epoch(), 1);
    }

    #[tokio::test]
    async fn a_second_node_follows_the_holder() {
        let os = mem();
        let a = fleet_app(&os, "kloudlite-srv-0").await;
        a.election_tick().await.unwrap();
        let b = fleet_app(&os, "kloudlite-srv-1").await;
        b.election_tick().await.unwrap();
        assert!(!b.is_leader());
        assert_eq!(b.leader().as_deref(), Some("kloudlite-srv-0"));
        assert!(b.leader_live());
        assert!(!b.ownership.is_writer().await);
    }

    #[tokio::test]
    async fn a_lease_taken_by_another_node_demotes() {
        let os = mem();
        let a = fleet_app(&os, "kloudlite-srv-0").await;
        a.election_tick().await.unwrap();
        plant(&os, "kloudlite-srv-1", 2, a.now_ms() + 5_000).await;
        a.election_tick().await.unwrap();
        assert!(!a.is_leader(), "somebody else holds a live lease at a newer epoch");
        assert_eq!(a.leader().as_deref(), Some("kloudlite-srv-1"));
        assert!(!a.ownership.is_writer().await);
        let e = a.grant_claim("alice/web", "kloudlite-srv-2", false).await.expect_err("demoted: must not grant");
        assert!(e.to_string().contains("not the leader"), "{e}");
    }

    #[tokio::test]
    async fn an_expired_lease_is_taken_with_the_next_epoch() {
        let os = mem();
        let a = fleet_app(&os, "kloudlite-srv-0").await;
        plant(&os, "kloudlite-srv-9", 5, a.now_ms() - 1).await;
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
        let a = fleet_app(&os, "kloudlite-srv-0").await;
        plant(&os, "kloudlite-srv-0", 3, a.now_ms() + 5_000).await;
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
        let a = fleet_app(&os, "kloudlite-srv-0").await;
        a.election_tick().await.unwrap();
        let stray = OwnershipStore::open(os.clone());
        stray.promote().await.unwrap();
        assert!(a.grant_claim("alice/web", "kloudlite-srv-1", false).await.is_err());
        assert!(!a.is_leader(), "a fenced writer is not the leader");
        assert!(!a.ownership.is_writer().await);
    }

    #[tokio::test]
    async fn grants_refuse_without_the_lease() {
        let os = mem();
        let a = fleet_app(&os, "kloudlite-srv-0").await;
        for r in [
            a.grant_claim("alice/web", "kloudlite-srv-1", false).await.map(|_| ()),
            a.grant_renew("kloudlite-srv-1", &["alice/web".into()]).await.map(|_| ()),
            a.grant_release("alice/web", "kloudlite-srv-1").await,
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
        let a = fleet_app(&os, "kloudlite-srv-0").await;
        a.election_tick().await.unwrap();
        let b = fleet_app(&os, "kloudlite-srv-1").await;
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
        let a = fleet_app(&os, "kloudlite-srv-0").await;
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
        let a = fleet_app(&os, "kloudlite-srv-0").await;
        let h = lease::take(os.as_ref(), "kloudlite-srv-0", a.now_ms(), None).await.unwrap().unwrap();
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
        let b = fleet_app(&os, "kloudlite-srv-1").await;
        b.set_leader(Some("ghost"));
        assert!(b.claim_to_recover("alice/web").await.is_err()); // two quick tries, 250 ms apart
        assert_eq!(b.leader(), None, "the lease is absent: nobody leads, and 'ghost' is forgotten");

        plant(&os, "kloudlite-srv-0", 4, b.now_ms() + 5_000).await;
        assert!(b.claim_to_recover("alice/web").await.is_err());
        assert_eq!(b.leader().as_deref(), Some("kloudlite-srv-0"), "re-read on the failed connect");
        assert!(b.leader_live());
    }

    /// A fingerprint row the api's migration has not reached yet names a HANDLE, not an email.
    /// With a directory configured it is still admitted for its own namespace and refused for
    /// anyone else's — the pre-migration rule — without a membership lookup, which is why an empty
    /// directory answers both.
    #[tokio::test]
    async fn a_handle_principal_acts_only_under_its_own_namespace() {
        let os = mem();
        let tmp = tempfile::tempdir().unwrap();
        let store =
            Arc::new(store::Store::open(os.clone(), tmp.path().join("cache"), false).await.unwrap());
        std::mem::forget(tmp);
        let dir = Arc::new(kloudlite_pulls::directory::Directory::in_memory());
        let a = App::new(
            store,
            Arc::new(OwnershipStore::open(os.clone())),
            "kloudlite-srv-0".into(),
            Arc::new(|_: &str| "127.0.0.1:1".into()),
            "test-secret".into(),
            pulls::Source::Directory(dir),
        );
        assert!(a.may_act("alice", "alice").await.unwrap(), "its own namespace");
        assert!(!a.may_act("alice", "acme").await.unwrap(), "somebody else's");
    }

    /// The registry authenticates by HANDLE — a PAT's owner and a registry token's `sub` are both
    /// handles — so a handle the directory can place is judged by that person's memberships, the
    /// same as their email. A handle it cannot place keeps the equality rule.
    #[tokio::test]
    async fn a_handle_the_directory_knows_is_judged_as_that_person() {
        let os = mem();
        let tmp = tempfile::tempdir().unwrap();
        let store =
            Arc::new(store::Store::open(os.clone(), tmp.path().join("cache"), false).await.unwrap());
        std::mem::forget(tmp);
        let dir = Arc::new(kloudlite_pulls::directory::Directory::in_memory());
        dir.upsert_user("alice@x", "Alice").await.unwrap();
        dir.claim_username("alice@x", "alice").await.unwrap().expect("handle");
        dir.create("acme", "Acme", "alice@x").await.unwrap().expect("team");
        let a = App::new(
            store,
            Arc::new(OwnershipStore::open(os.clone())),
            "kloudlite-srv-0".into(),
            Arc::new(|_: &str| "127.0.0.1:1".into()),
            "test-secret".into(),
            pulls::Source::Directory(dir),
        );
        assert!(a.may_act("alice", "acme").await.unwrap(), "a member, named by handle");
        assert!(a.may_act("alice@x", "acme").await.unwrap(), "the same person, named by email");
        assert!(a.may_act("alice", "alice").await.unwrap(), "their own namespace");
        assert!(!a.may_act("bob", "acme").await.unwrap(), "a handle nobody holds: equality only");
    }

    /// Solo: one node, no lease, no store traffic. It leads by construction.
    #[tokio::test]
    async fn a_solo_node_leads_without_a_lease() {
        let os = mem();
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(store::Store::open(os.clone(), tmp.path().join("cache"), false).await.unwrap());
        std::mem::forget(tmp);
        let a = App::new(store, Arc::new(OwnershipStore::solo()), "kloudlite-0".into(), Arc::new(|_: &str| "127.0.0.1:1".into()), "s".into(), pulls::Source::Absent);
        assert!(a.is_leader() && a.leader_live());
        a.election_tick().await.unwrap();
        assert!(lease::read(os.as_ref()).await.unwrap().is_none(), "solo never writes a lease");
    }

    /// A cold claim waits out a leader roll (~30 s of retries). With the gate full, one more
    /// fails at once instead of pinning another task for that long — the fast 503.
    #[tokio::test]
    async fn a_claim_past_the_gate_fails_fast() {
        let os = mem();
        let follower = fleet_app(&os, "kloudlite-srv-1").await; // nobody leads; the addr is a refused port
        let _held = follower.claim_gate.acquire_many(MAX_WAITING_CLAIMS as u32).await.unwrap();
        let t = std::time::Instant::now();
        let err = follower.claim("alice/cold").await.expect_err("must not be granted");
        assert!(err.to_string().contains("too many claims"), "{err}");
        assert!(t.elapsed() < std::time::Duration::from_millis(500), "must not enter the retry loop");
    }
}
