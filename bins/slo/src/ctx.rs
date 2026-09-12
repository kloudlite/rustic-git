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
    /// The pinned workspace (`run-{id}-pin`) the four `ws.packages.pin*` ids walk. Held for the
    /// same reason as `ux_workspace`: the steps after the create need the id it answered.
    pub pin_workspace: Option<String>,
    /// Stage 14's own environment (two services), its clone, and the workspace whose two pushes
    /// `vol.history` reads. Named `run-…`, so the prefix sweep finds them; held here because the
    /// four environment ids are one journey on one object.
    pub env_multi: Option<String>,
    pub env_clone: Option<String>,
    pub history_workspace: Option<String>,
    /// Workspaces whose POD the run frees as soon as nothing needs it — the restore's, and any
    /// other working copy a step makes and does not delete. Names only: the objects stay, and
    /// teardown's prefix sweep takes them. The region's pool nodes are 8 vCPU and a workspace
    /// requests 2, so a suite holding four at once has a fifth that can never schedule.
    pub extra_workspaces: Vec<String>,
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
    /// Who this run IS. From the CronJob's env — see `SUITE_TENANTS`.
    pub probe_user: String,
    pub other_user: String,
    pub probe_email: String,
    pub other_email: String,
    /// The second tenant, for `sec.cross.owner`. Never used to create anything teardown sweeps.
    pub other_jwt: String,
    /// The superadmin session, minted ON FIRST USE and cached (2026-09-12). It used to be minted
    /// at boot for every run of every suite, so the fast probe — which touches one admin route —
    /// held a superadmin credential in memory for its whole journey, and a run that never reached
    /// an admin route held one for nothing. `OnceLock` rather than a `Mutex`: it is written once
    /// and read from `&Ctx` closures all over the stages.
    admin_jwt: std::sync::OnceLock<String>,
    /// The minter itself, kept so the token above can be made later.
    jwt: Jwt,
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
    /// The last `rollout_in_flight` answer and when it was taken. The guard is asked before every
    /// stage AND on every failed step, and each ask is four or five reads of the API server; a
    /// stage with twenty failing steps made a hundred (2026-09-12). Ten seconds is far shorter
    /// than a roll and far longer than a burst of failures.
    pub rollout_cache: Option<(std::time::Instant, bool)>,
    /// Whether a failed step may ask the cluster if a roll is in flight. Off under the test kit:
    /// the dev pod IS in-cluster, so a unit test that expected failures read the real fleet
    /// mid-roll and saw skips instead (ship gate, 2026-09-12).
    pub roll_check: bool,
    /// When this run's ONE downgrade window opened. A roll is a real event with a beginning and
    /// an end, so a run gets a single window in which a failure may be read as the roll's rather
    /// than the service's — without that, a fleet stuck mid-roll for an hour turned every failing
    /// sample of every run into a skip, and the console went quiet instead of red.
    pub roll_window: Option<std::time::Instant>,
    /// Set when a mid-run report could not be filed. The run does NOT stop for it — teardown and
    /// the final report are what make a broken run visible — but the process must still exit 3.
    pub report_failed: bool,
}

/// How long the run's single downgrade window stays open. One srv roll is minutes; this is
/// generous enough to cover one and short enough that a fleet that never comes back is measured.
pub const ROLL_WINDOW: Duration = Duration::from_secs(600);

pub const PROBE_USER: &str = "slo-probe";
pub const OTHER_USER: &str = "slo-other";

/// The tenant pair for one suite. One pair PER SUITE, not one for the fleet: the hourly suite runs
/// for ~50 minutes and grants itself superadmin and a raised quota on the way, and the fast suite
/// ticks every five minutes underneath it — sharing a pair meant the fast run's `id.key.usable`
/// hit "that key is already added" and its `sec.*`/`quota.refused` checks read the hourly's
/// grants. The names come from the CronJob's env, so a suite is isolated by its yaml, not by code.
/// Every `slo-*` user is created by `bootstrap` and capped by deploy/k3s/quotas-slo.yaml.
pub const SUITE_TENANTS: &[(&str, &str)] =
    &[(PROBE_USER, OTHER_USER), ("slo-hourly", "slo-hourly-other"), ("slo-drill", "slo-drill-other")];

pub fn email_of(user: &str) -> String {
    format!("{user}@kloudlite.io")
}

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
        // Minted here rather than in the struct literal so `jwt` itself can be MOVED into the
        // context afterwards: it is what mints the superadmin session on first use.
        let probe_jwt = mint(&email_of(&cfg.probe_user), &cfg.probe_user)?;
        let other_jwt = mint(&email_of(&cfg.other_user), &cfg.other_user)?;
        Ok(Ctx {
            jwt,
            run_id: run_id.unwrap_or_else(|| format!("{}-{}", suite.as_str(), started.timestamp())),
            probe_jwt,
            other_jwt,
            admin_jwt: std::sync::OnceLock::new(),
            probe_user: cfg.probe_user.clone(),
            other_user: cfg.other_user.clone(),
            probe_email: email_of(&cfg.probe_user),
            other_email: email_of(&cfg.other_user),
            // One client for the whole run: connection reuse is the difference between a p95 that
            // measures the fleet and one that measures TLS handshakes.
            // Bounded at every layer: a connect that does not complete in 5 s is an answer, and
            // a pooled connection idle past 30 s is dropped before it can be reused dead — one
            // `audit.row` sample on 2026-09-11 17:26 was a POST to an in-cluster ClusterIP that
            // sat 20 s on the wire with the admin process idle.
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .connect_timeout(Duration::from_secs(5))
                .pool_idle_timeout(Duration::from_secs(30))
                .tcp_keepalive(Duration::from_secs(30))
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
            rollout_cache: None,
            roll_check: true,
            roll_window: None,
            cfg,
        })
    }

    /// The prefix every object this run creates carries, and the one teardown sweeps by.
    pub fn prefix(&self) -> String {
        format!("run-{}", self.run_id)
    }

    /// May this run still read a failure as the roll's? Opens the window on the first ask.
    pub fn roll_window_open(&mut self) -> bool {
        match self.roll_window {
            None => {
                self.roll_window = Some(std::time::Instant::now());
                tracing::warn!(run_id = %self.run_id, "slo.roll.window.opened");
                true
            }
            Some(at) => at.elapsed() < ROLL_WINDOW,
        }
    }

    /// The superadmin session, minted the first time something asks for one.
    ///
    /// Infallible on purpose: the secret was already parsed at boot (`Jwt::new`), so the only way
    /// this can fail is a bug, and a `Result` here would put a `?` in every admin step for a
    /// branch that cannot be taken. A mint that somehow fails yields an empty token, which every
    /// admin route answers 401 to — a loud failure, never a silent superadmin.
    pub fn admin_jwt(&self) -> String {
        self.admin_jwt
            .get_or_init(|| {
                match self.jwt.mint_admin(&self.probe_email, &self.probe_user, Some(&self.probe_user), true) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!(error = %e, "slo.admin.mint.failed");
                        String::new()
                    }
                }
            })
            .clone()
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
