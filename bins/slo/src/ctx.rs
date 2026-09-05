//! The one value every stage is handed: who the probe is, what it has created so far, and the
//! steps it has recorded.
//!
//! State is a plain struct of ids rather than a map because a stage that needs the workspace id
//! and finds none must SKIP, and a `None` the compiler makes it handle is how that stays true;
//! a missing map key is the same bug with no reminder.

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use kloudlite_core::jwt::Jwt;
use kloudlite_workspaces::history::slo::StepReport;
use kloudlite_workspaces::slo::catalogue::Suite;

use crate::config::Config;

/// What the run has created, so teardown and the later stages can find it. Every id is
/// `run-{run_id}-…`, which is what makes teardown's prefix sweep both complete and safe.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct State {
    pub repo: Option<String>,
    pub workspace: Option<String>,
    /// The clone of `workspace`, which is a second object teardown must find.
    pub clone: Option<String>,
    pub environment: Option<String>,
    /// The `Volume` CR behind `workspace`, and the push that stage 7 restores from. Both are
    /// stage 5's outputs and stage 7's inputs, which is the whole reason they live here.
    pub volume: Option<String>,
    pub snapshot: Option<String>,
    /// The environment's own volume and the push on it. Teardown deletes these BY NAME: an
    /// environment's `Volume` outlives the environment for as long as a snapshot references it,
    /// so without these two the probe would leak one subvolume per run forever.
    pub env_volume: Option<String>,
    pub env_snapshot: Option<String>,
    pub token: Option<String>,
    /// The token's VALUE, not its id — stage 4 logs in to the registry with it. `skip`, so it
    /// never reaches `state.json`: this struct is written to disk after every step for the
    /// parent's teardown, and a live git credential does not belong in a file.
    #[serde(skip)]
    pub token_value: Option<String>,
    /// `sec.agent.spec` already ran (from the workspace stage) — the security stage must not repeat it.
    pub agent_spec_done: bool,
    /// The CLI token `id.cli.flow` minted, by id. `skip` for the same reason as `token_value`:
    /// it addresses a live credential, and teardown reaches it anyway through the `cli-token`
    /// entry in `KINDS`, whose name carries the run prefix.
    #[serde(skip)]
    pub cli_token: Option<String>,
    /// The name the probe's SSH key is registered under. `skip`: it names the one private key in
    /// the pod, and nothing the parent does needs it.
    #[serde(skip)]
    pub key: Option<String>,
    /// The Experience stage's own workspace (`run-{id}-x`): `ws.packages.*` create it and
    /// `home.persists` writes the file it later reads from a fresh one. Every workspace that
    /// stage creates is named `run-{run_id}-…`, so teardown's prefix sweep finds them by name
    /// whether or not the stage got as far as deleting them itself.
    pub ux_workspace: Option<String>,
    /// Stage 14's own environment (two services), its clone, and the workspace whose two pushes
    /// `vol.history` reads. Named `run-…`, so the prefix sweep finds them; held here because the
    /// four environment ids are one journey on one object.
    pub env_multi: Option<String>,
    pub env_clone: Option<String>,
    pub history_workspace: Option<String>,
    /// Volumes to delete BY NAME after the prefix sweep — see `stages::drop_extra_volumes`.
    pub extra_volumes: Vec<String>,
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
    /// `None` when no kubeconfig was reachable — every step that needs it skips rather than
    /// failing, because a missing kubeconfig is a deployment gap, not an SLO breach.
    pub kube: Option<kube::Client>,
    /// The UNIT the report's backoff schedule is measured in — one second in a deployment. A field
    /// only so a test can shrink the whole schedule without reimplementing it.
    pub retry_delay: Duration,
    /// The admin process's clock minus this pod's, from the first report that landed. `None` until
    /// then. Every step's `ts` is the probe's and every window the console reads is the admin's, so
    /// a drifted pod files samples into minutes they did not happen in and nothing downstream can
    /// tell — this is the one number that can say so.
    pub clock_skew_ms: Option<i64>,
    /// Which binary `git`, `ssh-keygen` and `ssh-keyscan` are. A field so a test can point one at
    /// a program that succeeds — see `tools::Programs`.
    pub programs: crate::tools::Programs,
    /// Set by the parent when the child died without leaving a failing step behind — a panic, an
    /// abort, a non-zero exit. The step list alone cannot express "the journey stopped", so
    /// without this a run that crashed on its first stage would be reported as passed.
    pub run_failed: bool,
    /// Set when a mid-run report could not be filed. The run does NOT stop for it — teardown and
    /// the final report are what make a broken run visible — but the process must still exit 3.
    pub report_failed: bool,
}

pub const PROBE_EMAIL: &str = "slo-probe@kloudlite.io";
pub const OTHER_EMAIL: &str = "slo-other@kloudlite.io";
pub const PROBE_USER: &str = "slo-probe";
pub const OTHER_USER: &str = "slo-other";

impl Ctx {
    /// `run_id` is `Some` only in the child process, which must report under the SAME id its
    /// parent will file the final report under — the run is one row in `slo_runs`, not two.
    pub async fn new(cfg: Config, suite: Suite, run_id: Option<String>) -> anyhow::Result<Ctx> {
        let jwt = Jwt::new(&cfg.jwt_secret).map_err(|e| anyhow::anyhow!("jwt secret: {e}"))?;
        let mint = |email: &str, user: &str| {
            jwt.mint(email, user, Some(user)).map_err(|e| anyhow::anyhow!("mint {user}: {e}"))
        };
        // The id carries the run's start, so a child handed one recovers the parent's clock
        // rather than stamping a second, later `started` on the same run.
        let started = run_id
            .as_deref()
            .and_then(|id| id.rsplit('-').next())
            .and_then(|ts| ts.parse::<i64>().ok())
            .and_then(|ts| DateTime::from_timestamp(ts, 0))
            .unwrap_or_else(Utc::now);
        Ok(Ctx {
            run_id: run_id.unwrap_or_else(|| format!("{}-{}", suite.as_str(), started.timestamp())),
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
            // Reads `KUBECONFIG` (or the in-cluster ServiceAccount) itself, which is why `Config`
            // carries no kubeconfig field. The probe is never the reason a run fails to start:
            // no cluster reachable means the Kubernetes-only steps skip, and HTTP still runs.
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
            retry_delay: Duration::from_secs(1),
            clock_skew_ms: None,
            programs: crate::tools::Programs::default(),
            run_failed: false,
            report_failed: false,
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

    /// Where the child leaves its steps for the parent. The parent cannot ask a dead process what
    /// it measured, so the child writes this after every stage — the same moment it PUTs.
    pub fn steps_path(&self) -> PathBuf {
        self.tmp.join("steps.json")
    }

    /// The child's `State`, handed to the parent the same way its steps are: the parent runs
    /// teardown and needs every name the child recorded (the environment's volume, the extra
    /// volumes), or it deletes only what the prefix sweep can see. This is what leaked
    /// one environment volume per run before it existed. The credential fields are `serde(skip)`
    /// — the file is a handover of NAMES.
    pub fn state_path(&self) -> std::path::PathBuf {
        self.tmp.join("state.json")
    }

    /// Write it out. Called after every STEP, not every stage: a child the parent kills at the
    /// budget hands over the names it had made a second earlier, and a stage is minutes long.
    pub fn save_state(&self) {
        // No directory means no run (a unit test): nothing to hand over, and nothing to warn about.
        if !self.tmp.is_dir() {
            return;
        }
        match serde_json::to_vec(&self.state) {
            Ok(b) => {
                if let Err(e) = std::fs::write(self.state_path(), b) {
                    tracing::warn!(error = %e, "slo.state.failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "slo.state.failed"),
        }
    }
}
