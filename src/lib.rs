pub mod auth;
pub mod gc;
pub mod http;
pub mod peers;
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

pub struct App {
    pub store: std::sync::Arc<store::Store>,
    pub peers: std::sync::Arc<peers::Membership>,
    pub forwarder: std::sync::Arc<proxy::Forwarder>,
}

impl App {
    pub fn new(
        store: std::sync::Arc<store::Store>,
        peers: std::sync::Arc<peers::Membership>,
        peer_secret: String,
    ) -> Self {
        App {
            store,
            peers,
            forwarder: std::sync::Arc::new(proxy::Forwarder::new(peer_secret)),
        }
    }

    /// The routing decision for a repo, with the real probes wired in. The one place `decide` is
    /// called, so every route — HTTP public, HTTP peer, SSH, peer stream — applies the same rule.
    ///
    /// An unhealthy node never routes `Local`. Its peers see its /healthz fail and — with a second
    /// vantage — will serve its repos; if it kept serving them too, that is two writers. So it
    /// answers Unavailable and lets the fleet take over. Health has hysteresis (see
    /// `spawn_health_probe`) so one slow round trip does not flip the whole fleet's view.
    pub async fn route(&self, repo: &str) -> peers::Route {
        let f = self.forwarder.clone();
        let unhealthy = !self.store.healthy();
        let f2 = self.forwarder.clone();
        let route = self
            .peers
            .decide(
                repo,
                move |p: &peers::Peer| {
                    let f = f.clone();
                    let a = p.addr.clone();
                    async move { f.reachable(&a).await }
                },
                move |via: &peers::Peer, t: &peers::Peer| {
                    let f = f2.clone();
                    let a = via.addr.clone();
                    let n = t.name.clone();
                    async move { f.probe_via(&a, &n).await }
                },
            )
            .await;
        // An unhealthy node may still FORWARD what it does not own — that is safe and keeps its
        // 1/N of load-balancer traffic flowing — but never serves: its peers see its /healthz fail
        // and will take its repos, and serving alongside them is two writers.
        match (unhealthy, route) {
            (true, peers::Route::Local) => peers::Route::Unavailable,
            (_, r) => r,
        }
    }

    /// What to do when a request for `repo` hit a fence: re-run routing. `true` means this node
    /// still owns the repo (a stray admin process fenced us, or a peer has since released it) and
    /// the caller should reopen and retry the operation ONCE, in-handler — the HTTP handlers hold
    /// the body as `Bytes`, so a retry costs nothing. `false` means the fence was correct: answer
    /// 503. git does NOT retry a 503 by itself; the user re-runs.
    pub async fn on_fenced(&self, owner: &str, name: &str) -> bool {
        if !matches!(
            self.route(&format!("{owner}/{name}")).await,
            peers::Route::Local
        ) {
            return false;
        }
        // Pool::get never reopens a fenced handle by itself (that is the amplifier this exists to
        // remove). Routing says we still own it, so evict here — the retry's Pool::get then opens
        // fresh and takes the writer epoch back. Without this the retry gets a second FencedError.
        self.store.pool.evict(owner, name).await;
        true
    }
}
