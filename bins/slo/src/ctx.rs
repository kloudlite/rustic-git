//! The one value every stage is handed: who the probe is, what it has created so far, and the
//! steps it has recorded.
//!
//! State is a plain struct of ids rather than a map because a stage that needs the workspace id
//! and finds none must SKIP, and a `None` the compiler makes it handle is how that stays true;
//! a missing map key is the same bug with no reminder.

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use kloudlite_git_core::jwt::Jwt;
use kloudlite_git_workspaces::history::slo::StepReport;
use kloudlite_git_workspaces::slo::catalogue::Suite;

use crate::config::Config;

/// What the run has created, so teardown and the later stages can find it. Every id is
/// `run-{run_id}-…`, which is what makes teardown's prefix sweep both complete and safe.
#[derive(Debug, Default, Clone)]
pub struct State {
    pub repo: Option<String>,
    pub image: Option<String>,
    /// The sibling image that shares a layer with `image` (`reg.shared.layer`).
    pub sibling_image: Option<String>,
    pub workspace: Option<String>,
    /// The clone of `workspace`, which is a second object teardown must find.
    pub clone: Option<String>,
    pub environment: Option<String>,
    pub token: Option<String>,
    pub key: Option<String>,
    pub request: Option<String>,
}

pub struct Ctx {
    pub cfg: Config,
    /// `{suite}-{unix seconds}` — the shape `history::slo::validate` enforces, and the prefix
    /// every object this run creates is named with.
    pub run_id: String,
    pub suite: Suite,
    pub http: reqwest::Client,
    pub probe_jwt: String,
    /// The second tenant, for `sec.cross.owner`. Never used to create anything teardown sweeps.
    pub other_jwt: String,
    pub admin_jwt: String,
    pub started: DateTime<Utc>,
    pub steps: Vec<StepReport>,
    pub state: State,
    /// `/tmp` emptyDir: the git and crane working trees. The root filesystem is read-only.
    pub tmp: PathBuf,
    /// The stage now running, stamped onto every step it records.
    pub stage: String,
    /// `None` when no kubeconfig was mounted — every step that needs it skips rather than
    /// failing, because a missing kubeconfig is a deployment gap, not an SLO breach.
    pub kube: Option<kube::Client>,
    /// Between report attempts. A field only so the retry test does not sleep six seconds.
    pub retry_delay: Duration,
}

pub const PROBE_EMAIL: &str = "slo-probe@kloudlite.io";
pub const OTHER_EMAIL: &str = "slo-other@kloudlite.io";
pub const PROBE_USER: &str = "slo-probe";
pub const OTHER_USER: &str = "slo-other";

impl Ctx {
    pub async fn new(cfg: Config, suite: Suite) -> anyhow::Result<Ctx> {
        let jwt = Jwt::new(&cfg.jwt_secret).map_err(|e| anyhow::anyhow!("jwt secret: {e}"))?;
        let mint = |email: &str, user: &str| {
            jwt.mint(email, user, Some(user)).map_err(|e| anyhow::anyhow!("mint {user}: {e}"))
        };
        let started = Utc::now();
        Ok(Ctx {
            run_id: format!("{}-{}", suite.as_str(), started.timestamp()),
            probe_jwt: mint(PROBE_EMAIL, PROBE_USER)?,
            other_jwt: mint(OTHER_EMAIL, OTHER_USER)?,
            admin_jwt: jwt
                .mint_admin(PROBE_EMAIL, PROBE_USER, Some(PROBE_USER), true)
                .map_err(|e| anyhow::anyhow!("mint admin: {e}"))?,
            // One client for the whole run: connection reuse is the difference between a p95 that
            // measures the fleet and one that measures TLS handshakes.
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .map_err(|e| anyhow::anyhow!("http client: {e}"))?,
            // The probe is never the reason a run fails to start: no cluster reachable means the
            // Kubernetes-only steps skip, and everything over HTTP still runs.
            kube: match kube::Client::try_default().await {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!(error = %e, "slo.kube.unavailable");
                    None
                }
            },
            started,
            suite,
            steps: vec![],
            state: State::default(),
            tmp: std::env::temp_dir().join(format!("slo-{}", started.timestamp())),
            stage: String::new(),
            retry_delay: Duration::from_secs(2),
            cfg,
        })
    }

    /// The prefix every object this run creates carries, and the one teardown sweeps by.
    pub fn prefix(&self) -> String {
        format!("run-{}", self.run_id)
    }

    pub fn bearer(&self, token: &str) -> String {
        format!("Bearer {token}")
    }
}
