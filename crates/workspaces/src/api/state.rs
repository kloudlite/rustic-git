//! What every `/v1` handler is handed: the verified `Caller`, the `ApiState` (kube client, JWT,
//! settings, the directory when this process has one), and the team-role and grant vocabulary
//! the directory answers in.

use super::*;


/// Who is calling, and whether they hold the platform-administrator claim.
///
/// A struct rather than the bare handle because two facts travel together everywhere: the owner
/// name every path is scoped by, and the claim `may_act_on` reads as its third arm. `Deref` and
/// `Display` are so the sites that only want the handle read unchanged.
#[derive(Debug, Clone)]
pub struct Caller {
    pub name: String,
    /// A CLAIM from the session token, minted at sign-in from the directory's list. Never an
    /// ownership: it decides who may act, never who owns anything, and it never widens a quota.
    pub superadmin: bool,
}


impl std::ops::Deref for Caller {
    type Target = str;
    fn deref(&self) -> &str {
        &self.name
    }
}


impl std::fmt::Display for Caller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}


/// What every workspace of an owner carries about them, from the directory the api tier owns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OwnerMaterial {
    /// What git commits as. Empty when the handle is nobody's, and git will ask.
    pub git_name: String,
    pub git_email: String,
}


/// A person's standing in a team, as the platform directory records it.
///
/// A local enum rather than `kloudlite_pulls::directory::Role` for the same reason the whole
/// `Directory` trait is local: this crate must not depend on the mongo-backed one just for a
/// lookup. `Ord` is declared by the variant ORDER — `Member < Admin < Owner` — so `>= Admin` is
/// the rank rule, and there is no second rank table to fall out of step with the first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TeamRole {
    Member,
    Admin,
    Owner,
}

/// The four lookups this api makes against the platform directory, kept behind a trait rather
/// than a direct dependency on `kloudlite_pulls::directory::Directory` (mongo-backed, heavy to
/// construct) so unit tests can supply a stub instead. Production wires `Directory` in via an
/// adapter in `bins/api`.
///
/// Every method is REQUIRED. A defaulted one made a partial test stub read as a live-but-empty
/// directory — `teams_for` returning an empty Vec is "asked and answered" to `resolve_new_owner`,
/// which is a 403 "not a member", where no directory at all is a 503. A stub must say which it
/// means.
#[async_trait::async_trait]
pub trait Directory: Send + Sync {
    /// Every team slug `user` belongs to. Called once per request, no cache —
    /// ponytail: an in-process cache would cut the N+1 here, add one if this ever shows up hot.
    async fn teams_for(&self, user: &str) -> Vec<String>;

    /// Is this CLI login still valid? A `cli` JWT carries a `jti` whose row in the directory IS
    /// the revocation list — the same rule `crates/api`'s `user_identity` enforces. `false`
    /// refuses the token, which is what an unwired directory must do: a 30-day token nobody can
    /// cancel is the worse failure.
    async fn is_live(&self, jti: &str) -> bool;

    /// The owner's git identity. `None` when the lookup FAILED — the Secret is then left alone
    /// rather than rewritten with empty strings.
    async fn for_owner(&self, owner: &str) -> Option<OwnerMaterial>;

    /// The `authorized_keys` file for an owner namespace — the union of its members' keys. `None`
    /// when the lookup FAILED; an owner with no keys is `Some("")`.
    async fn authorized_keys_for_owner(&self, owner: &str) -> Option<String>;

    /// Every namespace `email`'s keys project into: their own handle and each team they belong to.
    async fn owners_of(&self, email: &str) -> Vec<String>;

    /// The caller's role in `team`, or `None` when they are not a member — or when the lookup
    /// could not be made. Both answer "no" here, which is the safe direction for the one decision
    /// it feeds: who may raise a team's ceiling.
    ///
    /// `user` is whatever identity `teams_for` matches on, so the two can never disagree about who
    /// is in the team. Required (no default): unlike the other lookups, a stub that silently
    /// answered "not a member" would make the admin-only request check a no-op nobody tests.
    async fn team_role(&self, user: &str, team: &str) -> Option<TeamRole>;

    /// Does a team named `slug` exist? The one thing a slug's spelling cannot say by itself, and
    /// the quota routes need it to pick `default_quota(team)`'s right side rather than guess from
    /// who happens to be asking. Required (no default): a stub answering `false` here would make
    /// every team-owned quota request silently seed from the person defaults, and nothing would
    /// fail loudly enough to notice.
    async fn is_team(&self, slug: &str) -> bool;

    /// Put `user` into `team` at `role`, creating the membership if they are not in it yet. Only
    /// the admin process implements this — the user role has no route that could call it.
    ///
    /// `user` is the HANDLE the request was opened under, the same identity `team_role` takes; an
    /// implementation whose store keys memberships on something else (the directory keys on email)
    /// resolves it itself, and answers `NoSuchUser` when it cannot.
    /// Create-or-find a person by email and give them `username` (a no-op when they already hold
    /// it). Only the SLO probe's bootstrap calls this, through `/admin/slo/bootstrap`: sign-in is the
    /// one other way a person comes to exist, and a synthetic user never signs in. `Err` carries
    /// the directory's own sentence.
    async fn ensure_user(&self, email: &str, name: &str, username: &str) -> Result<(), String>;
    /// Seat `email` on the superadmin roster, idempotently. Same single caller as `ensure_user`:
    /// the roster routes read the caller's ROW, so a probe that grants and revokes needs one.
    async fn add_superadmin(&self, email: &str, by: &str) -> Result<(), String>;

    async fn grant_access(&self, _team: &str, _user: &str, _role: TeamRole) -> GrantAccess {
        GrantAccess::Unsupported
    }
}


/// What a membership write did. Not a `Result`: "no such user" and "no such team" are answers a
/// decider needs to read back verbatim, not errors to log.
#[derive(Debug, PartialEq, Eq)]
pub enum GrantAccess {
    Done,
    NoSuchUser,
    NoSuchTeam,
    /// The directory's own refusal, already in words fit to show — a last-owner demotion, say.
    Refused(String),
    /// No directory is wired, or this one cannot write. A DEFAULT so every test stub in this crate
    /// keeps compiling; the approve arm turns it into a 503 rather than a false success.
    Unsupported,
}


pub struct ApiState {
    pub jwt: Arc<Jwt>,
    /// Team membership, CLI-token revocation and the owner's ssh keys. `None` means no directory
    /// is wired (dev, or the directory tier is down): team envs answer 503 rather than silently
    /// behaving as if the caller has no teams, CLI tokens are refused outright, and a workspace
    /// comes up with the private key alone exactly as before ssh existed.
    pub directory: Option<Arc<dyn Directory>>,
    /// `None` when no kubeconfig/in-cluster config is available: every workspace, environment and
    /// volume route answers 503 rather than not existing.
    pub kube: Option<kube::Client>,
    /// The AKS in-cluster client — `kloudlite-admin`'s OWN cluster, distinct from `kube` above
    /// (a region's k3s, reached over a mounted kubeconfig). Only `admin::workloads`' central-scope
    /// calls use this; every CRD (workspaces, environments, regions, quotas) still lives in a
    /// region cluster and keeps reading `kube`. `None` off-AKS (dev, tests): central rolls answer
    /// 503 rather than not existing, same convention as `kube`.
    pub aks: Option<kube::Client>,
    /// The auth store: workspace creation copies the owner's platform-issued git key into their
    /// namespace through it, and `GET /admin/settings/central` reads `cluster/settings` off its
    /// `.os` object-store handle directly (this tier can read the object store anywhere, matching
    /// `_catalog`/`/api/{owner}/images` — only the write is peer-only). `None` in dev and in
    /// tests: workspaces still create without a key, and the central settings route answers 503.
    pub keys: Option<Arc<kloudlite_storage::store::Store>>,
    /// This tier's own region's `default_replicas`/`quota_gb_ceiling` — Task 3 gives the agent
    /// its own handle from a `ClusterSettings` reflector; this one seeds from env only, since
    /// `/v1` has no per-region watch of its own yet. Read-mostly today (no refresh beat wired
    /// here in this task), but the field exists so `clamp_quota` has a live ceiling to read
    /// instead of a compiled-in number.
    pub settings: LiveSettings<AgentSettings>,
    /// The server tier's peer listener + peer secret — the ONE call this admin process makes
    /// outbound to the git tier, forwarding a validated central-settings write (`PUT
    /// /api/admin/settings`, Task 4). `None` in dev/tests: `GET /admin/settings/central` still
    /// answers from `keys`' object store directly, only the `PUT` needs this.
    pub peer: Option<admin::PeerClient>,
    /// ClickHouse (ClickStack's), holding the collector's `default` telemetry and our own `kloudlite`
    /// database. `None` when `KLOUDLITE_CLICKHOUSE_URL` is unset — a supported configuration, not
    /// a degraded one: history routes answer `503 history unavailable` and the console renders a
    /// flat placeholder. Only the ADMIN process ever sets this; the user role never constructs one,
    /// which is what makes "the admin process is the only writer of `kloudlite`" a fact about the
    /// binary rather than a convention.
    pub history: Option<Arc<crate::history::History>>,
    /// Redis, for the `history` consumer group only — no request path reads it. `None` in dev and
    /// wherever the cache is disabled; the consumer then never spawns, which costs the activity
    /// feed its PR half and nothing else (CLAUDE.md: the stream is a nudge, never the record).
    pub cache: Option<Arc<kloudlite_storage::cache::Cache>>,
    /// `KLOUDLITE_SLO_WEBHOOK`, read once at boot. `None` — the default — means a failed probe
    /// run and a firing `SloBurn` are recorded and shown on the console like every other fact, and
    /// nothing is posted anywhere: the webhook is a nudge, never the record.
    pub slo_webhook: Option<String>,
    /// `KLOUDLITE_BUILDER_SECRET` — the shared secret the build gate presents on
    /// `/v1/internal/builders/*`, the one surface with no person behind it. `None` (dev, tests
    /// that do not exercise it, and the admin role, which mounts none of these routes) means
    /// every internal route answers 401: fail closed, never open.
    pub builder_secret: Option<String>,
    /// Turns a `name@version` entry into a `crd::Lock` before the CR is written. The api always wires one
    /// (`Resolver::from_env`); `None` is the test harness, where a list with a `@` entry is
    /// refused 503 rather than written unlocked, and a list without one never asks.
    pub resolver: Option<Arc<crate::packages::resolve::Resolver>>,
}


impl ApiState {
    pub fn new(jwt: Arc<Jwt>) -> Self {
        ApiState {
            jwt,
            directory: None,
            kube: None,
            aks: None,
            keys: None,
            settings: LiveSettings::new(AgentSettings::from_env()),
            peer: None,
            history: None,
            cache: None,
            slo_webhook: None,
            builder_secret: None,
            resolver: None,
        }
    }

    /// An empty value is no secret at all — an env var set to "" in a manifest must not become
    /// a gate anybody can pass by sending `Bearer `.
    pub fn with_builder_secret(mut self, secret: Option<String>) -> Self {
        self.builder_secret = secret.filter(|s| !s.trim().is_empty());
        self
    }

    pub fn with_resolver(mut self, r: Arc<crate::packages::resolve::Resolver>) -> Self {
        self.resolver = Some(r);
        self
    }

    pub fn with_peer(mut self, peer: admin::PeerClient) -> Self {
        self.peer = Some(peer);
        self
    }

    pub fn with_directory(mut self, directory: Arc<dyn Directory>) -> Self {
        self.directory = Some(directory);
        self
    }

    pub fn with_kube(mut self, client: kube::Client) -> Self {
        self.kube = Some(client);
        self
    }

    pub fn with_aks(mut self, client: kube::Client) -> Self {
        self.aks = Some(client);
        self
    }

    pub fn with_keys(mut self, keys: Arc<kloudlite_storage::store::Store>) -> Self {
        self.keys = Some(keys);
        self
    }

    pub fn with_history(mut self, history: Arc<crate::history::History>) -> Self {
        self.history = Some(history);
        self
    }

    pub fn with_cache(mut self, cache: Arc<kloudlite_storage::cache::Cache>) -> Self {
        self.cache = Some(cache);
        self
    }

    /// An empty value is no webhook: an env var set to "" in a manifest must not become a post to
    /// the empty url on every failed run.
    pub fn with_slo_webhook(mut self, url: Option<String>) -> Self {
        self.slo_webhook = url.filter(|u| !u.trim().is_empty());
        self
    }
}
