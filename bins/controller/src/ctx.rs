//! Everything one pass of this process needs, and the term it is allowed to write under.

use kloudlite_core::settings::LiveSettings;
use kloudlite_workspaces::settings::AgentSettings;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

/// Env-derived config. No secret, by design: this process holds none.
pub struct Config {
    /// `WS_REGION` — logged and stamped, never used to filter: one controller per cluster means
    /// every object it can see is its own.
    pub region: String,
    /// `POD_NAME`, from the downward API. The lease's `holderIdentity`.
    pub holder: String,
}

impl Config {
    pub fn from_env() -> Result<Config, String> {
        let region = std::env::var("WS_REGION").unwrap_or_default();
        let holder = std::env::var("POD_NAME").unwrap_or_default();
        Config::check(&region, &holder)?;
        Ok(Config { region, holder })
    }

    pub fn check(region: &str, holder: &str) -> Result<(), String> {
        if region.is_empty() {
            return Err("WS_REGION is required".into());
        }
        if holder.is_empty() {
            return Err("POD_NAME is required: it is the lease holder identity".into());
        }
        Ok(())
    }
}

pub struct Ctx {
    pub client: kube::Client,
    pub holder: String,
    pub region: String,
    /// The term this process was elected under, 0 when it is not the leader. Read immediately
    /// before every write (`lease::may_write`); a stale term demotes instead of finishing.
    pub(crate) epoch: AtomicU32,
    /// `ensure`'s memory, exactly as the agent's (`bins/agent/src/controller/status.rs`), except
    /// that with ONE writer per cluster it is now the truth for the whole cluster rather than one
    /// of N per-process guesses.
    pub applied: Mutex<HashMap<String, (u64, std::time::Instant)>>,
    pub settings: LiveSettings<AgentSettings>,
}

impl Ctx {
    pub fn epoch(&self) -> u32 {
        self.epoch.load(Ordering::SeqCst)
    }
    pub fn leading(&self) -> bool {
        self.epoch() != 0
    }
    pub fn promote(&self, epoch: u32) {
        if self.epoch.swap(epoch, Ordering::SeqCst) != epoch {
            tracing::info!(holder = %self.holder, epoch, "leader.acquired");
        }
    }
    pub fn demote(&self, reason: &str) {
        let was = self.epoch.swap(0, Ordering::SeqCst);
        if was != 0 {
            tracing::info!(holder = %self.holder, epoch = was, reason, "leader.lost");
        }
    }
}

#[cfg(test)]
impl Ctx {
    /// A Ctx over whatever client the test supplies — a canned one for the election beat, an
    /// unrouted one for the epoch guard, which touches no API server.
    pub(crate) fn for_test_with(client: kube::Client) -> Ctx {
        Ctx {
            client,
            holder: "ctl-test".into(),
            region: "test".into(),
            epoch: AtomicU32::new(0),
            applied: Default::default(),
            settings: LiveSettings::new(AgentSettings::from_env()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The holder identity is the POD NAME, which is what makes "who holds the lease" answerable
    /// from `kubectl get lease` alone. An empty `POD_NAME` is a boot failure, not a blank holder:
    /// two pods with the same (empty) identity would each read the other's lease as their own and
    /// both write.
    #[test]
    fn a_blank_holder_is_refused() {
        assert!(Config::check("centralindia-k3s", "kloudlite-controller-abc").is_ok());
        assert!(Config::check("centralindia-k3s", "").is_err());
        assert!(Config::check("", "kloudlite-controller-abc").is_err());
    }

    /// The epoch guard is a process-wide fact, not a parameter threaded through every call site:
    /// a write path asks the Ctx.
    ///
    /// `tokio::test` only because building a `kube::Client` — even the canned one — needs a
    /// reactor; nothing under test here touches it.
    #[tokio::test]
    async fn the_ctx_remembers_the_term_it_was_elected_under() {
        let ctx = Ctx::for_test_with(kloudlite_workspaces::kube_test::mock_client(vec![]).0);
        assert_eq!(ctx.epoch(), 0);
        assert!(!ctx.leading());
        ctx.promote(4);
        assert_eq!(ctx.epoch(), 4);
        assert!(ctx.leading());
        ctx.demote("fenced");
        assert!(!ctx.leading());
    }
}
