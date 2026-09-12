//! The directory: people, and the teams they belong to.
//!
//! Only the api server talks to this. Nothing else — least of all the web app —
//! holds a database connection: a second writer would mean two places that decide
//! what a valid handle is, and the browser-facing process would hold credentials
//! it has no reason to have.
//!
//! Stored in Cosmos DB (Mongo API) rather than the object store that holds packs
//! and credentials. Those are content-addressed or single-key lookups; a team is
//! queried by membership ("which teams does this person belong to?"), which an
//! object store cannot answer without listing everything.
//!
//! The slug is the document `_id`. That is not a shortcut: it makes uniqueness a
//! property of the database rather than of a check-then-insert, so two people
//! creating the same team at the same moment cannot both succeed. It also means
//! members live inside the team document, so creating a team and making its
//! creator an owner is one atomic write — no transaction, and no window where a
//! team exists with nobody able to administer it.

mod teams;
pub use teams::{
    check_pins, AcceptInvite, AddMember, DeleteTeam, Invite, Membership, Team, TeamProfile,
    MAX_PINS,
};

use mongodb::bson::{doc, DateTime};
use mongodb::options::ClientOptions;
use mongodb::{Client, Collection, IndexModel};
use kloudlite_core::{err, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    /// The person's stable identifier — their email, as the identity provider gives it.
    pub user: String,
    pub role: Role,
    pub joined_at: DateTime,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Owner,
    Admin,
    Member,
}

/// A person. The identity provider owns who they are; this records that they
/// exist here, so a team can name its members and a session can be tied to a row
/// rather than to a claim in a token.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct User {
    /// Email, as the identity provider gives it — lowercased, because providers
    /// are inconsistent about case and two rows for one human is a real bug.
    #[serde(rename = "_id")]
    pub email: String,
    pub name: String,
    /// The handle they picked, once they have. `None` until then, which is what
    /// the web app branches on to ask for one — a person exists the moment they
    /// sign in, but they do not have a namespace until they choose it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub created_at: DateTime,
    pub last_seen_at: DateTime,
}

/// A platform administrator. One row per person, keyed by email — the same identity the session
/// token's `sub` carries, so the mint is a single lookup.
///
/// A collection rather than an env var because the env var could only ever be a bootstrap: it is
/// read by one process at boot, cannot be changed without a roll, and says nothing about who
/// granted it. `addedBy` is the audit trail the env var never had.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SuperAdmin {
    #[serde(rename = "_id")]
    pub user: String,
    pub added_at: DateTime,
    pub added_by: String,
}

/// A claimed handle. Usernames and team slugs are the SAME namespace — both become
/// `/{handle}` in every URL and clone address — so they are reserved in one
/// collection. Uniqueness across both kinds is then a property of the database,
/// not of a check in application code that two requests could interleave.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Handle {
    #[serde(rename = "_id")]
    pub handle: String,
    pub kind: HandleKind,
    /// Who holds it: the user's email, or the team's creator.
    pub held_by: String,
    pub created_at: DateTime,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HandleKind {
    User,
    Team,
}

/// A repo, as the directory knows it.
///
/// A repo row as it was written before repos carried their own truth. Nothing writes these any
/// more: a repo's name, description, visibility and creation instant live in its own database,
/// and the listing markers in the object store answer "which repos does this owner have" without
/// opening one. The rows survive as `delete_team`'s repo count and as the rollback path — so the
/// field comments below describe what a row MEANT, not what any of it decides today.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Repo {
    /// `owner/name` — the clone path. This used to be what made a name unique; the owning
    /// node's check-then-create is, now that both creates of one name route to it.
    #[serde(rename = "_id")]
    pub id: String,
    pub owner: String,
    pub name: String,
    /// Public repos are readable by strangers. Always a mirror of the flag the owning git node
    /// enforces, and the seed the marker backfill copies into a listing marker.
    pub public: bool,
    #[serde(default)]
    pub description: String,
    pub created_by: String,
    pub created_at: DateTime,
}

/// A credential's metadata — never the credential.
///
/// The secret itself stays where the git fleet can read it without a database: an
/// object key named after its digest. That layout answers "who does this token
/// belong to?" in one GET, which is what every request needs, and answers nothing
/// else — it cannot list a person's tokens, name them, or say when one was made.
/// So the human-facing half lives here, keyed to the same digest.
///
/// Scope is one owner, chosen at creation, because that is exactly what the fleet
/// enforces: `auth::authorize` compares the credential's owner to the repo's. A
/// credential for a team is a separate credential, deliberately — it means a
/// leaked laptop key cannot reach a team's repos unless it was made for them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Credential {
    /// The token's sha256 digest, or the key's fingerprint — prefixed with its
    /// kind for a signing key, so registering one key for both purposes stores
    /// two rows rather than one overwriting the other.
    #[serde(rename = "_id")]
    pub id: String,
    pub kind: CredentialKind,
    /// The namespace this credential acts in.
    pub owner: String,
    /// The person who created it — so a team's members can see whose it is.
    pub created_by: String,
    /// What they called it. For an ssh key this is the comment or a given title.
    pub name: String,
    /// The armoured public key, for a GPG signing key only.
    ///
    /// An ssh signature carries its own key, so the fingerprint is enough. An
    /// OpenPGP signature does not — verifying it needs the key material, and
    /// answering "whose is this" needs the subkeys and user ids inside it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub material: String,
    /// Every fingerprint this credential answers to: for GPG, the primary key and
    /// each subkey, so a signature made by a subkey finds its owner in one query.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fingerprints: Vec<String>,
    pub created_at: DateTime,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CredentialKind {
    Token,
    SshKey,
    /// A key used to SIGN commits, never to authenticate.
    ///
    /// Separate from `SshKey` because they answer different questions — one is
    /// "may this connection push", the other "did this person write this commit"
    /// — and because git itself keeps them apart. The same key may be registered
    /// as both, which is why the id carries the kind.
    SigningKey,
    /// A CLI login. The token is a JWT, so nothing secret is stored — the row IS the
    /// revocation list: its `_id` is the token's `jti`, and a `cli` token whose row is
    /// gone authenticates nothing. Deleting the row is therefore the whole of revoking,
    /// and a token issued without one is inert rather than unrevocable.
    CliToken,
}

/// Pull requests live in the repo's own database now; the types are defined there so there is
/// one shape, not two that can drift, and migration reads the Mongo rows into them.
pub use crate::pulls::{Comment, MergeJob, Mergeability, PullRequest, PullState};

mod signin;
mod users;
mod credentials;
mod passkeys;
mod superadmins;

/// GitHub's vocabulary, because a client that already branches on theirs should
/// not have to learn a second one.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MergeableState {
    /// Nothing in the way.
    Clean,
    /// The base has moved on; this needs a real merge rather than a fast-forward.
    Behind,
    /// The two cannot be combined without someone deciding what wins.
    Dirty,
    /// Not worked out yet, or the branches could not be read.
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MergeState {
    /// Waiting for a worker.
    Queued,
    /// A worker has it.
    Running,
    /// The branches conflict; a person has to resolve it.
    Conflicts,
    /// It did not work, and not because of conflicts.
    Failed,
}

/// A passkey — a WebAuthn credential someone signs in with.
///
/// Belongs to a PERSON, not a namespace, unlike a token or an ssh key: those are
/// presented to the git fleet, which authorizes per repo owner, while this is only
/// ever used to establish who is sitting at the browser. Nothing here reaches a
/// git node.
///
/// The public key is public by construction — it verifies a signature and cannot
/// produce one — so storing it is not storing a secret. The private half never
/// leaves the authenticator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Passkey {
    /// The credential id the authenticator returns, base64url. Unique by
    /// construction, which is what makes it the document id.
    #[serde(rename = "_id")]
    pub id: String,
    /// The person's email.
    pub user: String,
    /// COSE public key, base64url.
    pub public_key: String,
    /// The authenticator's signature counter, as of the last successful sign-in.
    /// A counter that fails to advance is the documented signal of a cloned
    /// authenticator, which is why it is stored rather than merely accepted.
    pub counter: i64,
    #[serde(default)]
    pub transports: Vec<String>,
    pub name: String,
    pub created_at: DateTime,
}

/// Handles a person may not take, beyond what `valid_owner` already refuses.
/// These are routes, or words a stranger would read as official.
const RESERVED: &[&str] = &[
    "admin", "api", "app", "assets", "auth", "billing", "blog", "dashboard", "docs", "help",
    // `/invite/{token}` and `/verify/{token}` are routes in the web app; a person with either
    // name would shadow one.
    "invite", "invites", "verify",
    // `/cli/authorize` is a top-level web route (the device-auth handoff); a person named `cli`
    // would shadow it.
    "cli",
    "kloudlite", "login", "logout", "new", "root", "settings", "signup", "static", "status",
    "support", "system", "team", "teams", "user", "users", "www",
];

/// `Ok(())` if this could be someone's handle. The rules are the namespace's, so
/// a username and a team slug are held to exactly the same ones.
/// A request the caller can fix — a bad handle, an empty name, a malformed email — as opposed to
/// a database failure. The api tier answers 400 with the text when the error IS this type, and a
/// fixed 502 otherwise; a substring match on the message used to echo Mongo's words as a 400.
#[derive(Debug)]
pub struct Invalid(pub String);

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Invalid {}

pub(crate) fn invalid(msg: &str) -> kloudlite_core::Error {
    Box::new(Invalid(msg.to_string()))
}

pub fn check_handle(h: &str) -> Result<()> {
    if h.len() < 3 {
        return Err(invalid("handle must be at least 3 characters"));
    }
    if h.len() > 39 {
        return Err(invalid("handle must be 39 characters or fewer"));
    }
    if h != h.to_lowercase() {
        return Err(invalid("handle must be lowercase"));
    }
    // Stricter than `valid_owner`, which also permits underscores: a handle is
    // read aloud, typed from memory and rendered under a link underline, where an
    // underscore is easy to miss. Repo names keep the looser rule; this namespace
    // does not.
    if !h.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        return Err(invalid("handle may use letters, digits and dashes only"));
    }
    if h.starts_with('-') || h.ends_with('-') {
        return Err(invalid("handle may not start or end with a dash"));
    }
    // Before `valid_owner`: several reserved words are also refused there, and
    // "that handle is reserved" tells the person something the generic message
    // does not.
    if RESERVED.contains(&h) {
        return Err(invalid("that handle is reserved"));
    }
    if !kloudlite_storage::store::valid_owner(h) {
        return Err(invalid("that handle cannot be used"));
    }
    Ok(())
}

#[derive(Clone)]
pub struct Directory {
    pub(crate) backend: Backend,
}

/// Where the rows live. Mongo in every deployment; the in-memory arm exists so the `/v1`
/// handlers can be tested without a database — a Mongo the CI runner has to provide is a
/// dependency the six directory-backed tests used to skip themselves over.
#[derive(Clone)]
pub(crate) enum Backend {
    Mongo(Box<MongoCollections>),
    Memory(std::sync::Arc<std::sync::Mutex<MemoryState>>),
}

#[derive(Clone)]
pub(crate) struct MongoCollections {
    teams: Collection<Team>,
    /// Migration only, both of these: repos and pull requests are truth in the owning repo's own
    /// database now. `repos` survives as `delete_team`'s "still owns repositories" count,
    /// `pulls_for` seeds a repo's pull history once. Read, never written.
    repos: Collection<Repo>,
    pulls: Collection<PullRequest>,
    credentials: Collection<Credential>,
    passkeys: Collection<Passkey>,
    users: Collection<User>,
    handles: Collection<Handle>,
    invites: Collection<Invite>,
    signins: Collection<SignInLink>,
    cli_logins: Collection<CliLogin>,
    superadmins: Collection<SuperAdmin>,
    /// One row per one-shot migration that has run, keyed by its name. Not a schema version:
    /// the migrations here are independent of each other and land in any order.
    meta: Collection<mongodb::bson::Document>,
}

/// The same rows, keyed the way Mongo keys them (`_id`), under one lock.
/// ponytail: one mutex over every collection, and every list is a full scan — a test fixture
/// holds tens of rows. If this ever backs anything but tests, it needs indexes and a real store,
/// which is what the Mongo arm already is.
#[derive(Default)]
pub(crate) struct MemoryState {
    teams: std::collections::BTreeMap<String, Team>,
    credentials: std::collections::BTreeMap<String, Credential>,
    passkeys: std::collections::BTreeMap<String, Passkey>,
    users: std::collections::BTreeMap<String, User>,
    handles: std::collections::BTreeMap<String, Handle>,
    invites: std::collections::BTreeMap<String, Invite>,
    signins: std::collections::BTreeMap<String, SignInLink>,
    cli_logins: std::collections::BTreeMap<String, CliLogin>,
    superadmins: std::collections::BTreeMap<String, SuperAdmin>,
    meta: std::collections::BTreeMap<String, String>,
    // `repos` and `pulls` have no in-memory arm on purpose: both Mongo collections are
    // read-only migration leftovers that nothing writes any more, so the answers are
    // "no repos" and "no pull rows" — which is exactly what an empty collection gives.
}

/// Newest first, the way every `sort(createdAt: -1)` in this file asks for it.
fn newest_first<T>(mut v: Vec<T>, at: impl Fn(&T) -> DateTime) -> Vec<T> {
    v.sort_by_key(|x| std::cmp::Reverse(at(x).timestamp_millis()));
    v
}

/// A magic sign-in link, keyed by the HASH of its token — same shape as an invitation, for
/// the same reason: the collection must not be usable to sign in as anyone. Redeeming deletes
/// the row first, so a link works exactly once.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SignInLink {
    #[serde(rename = "_id")]
    pub id: String,
    pub email: String,
    pub created_at: DateTime,
    pub expires_at: DateTime,
}

/// A CLI login in flight: `kl login` asked for a code, and a browser has not yet approved it —
/// or has, and the CLI has not yet collected the token. A row rather than memory because the
/// api runs more than one replica and the code is created on one pod and approved on another.
/// The token exists only here between approval and collection, which is why approval — not
/// polling — is what mints it, and why collecting deletes the row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CliLogin {
    /// The code a human reads off one screen and types into another.
    #[serde(rename = "_id")]
    pub code: String,
    /// The opaque id the CLI polls with. Separate from the code because the code is SHOWN to a
    /// person and the poll id is not: knowing the code someone is reading aloud must not be
    /// enough to steal the token it becomes.
    pub poll: String,
    pub device: String,
    pub expires_at: DateTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// The token's `exp`, epoch seconds — carried so the poll can answer `expiresAt` without
    /// decoding the token it is handing over.
    #[serde(default)]
    pub token_exp: u64,
}

impl Directory {
    /// `uri` is the Cosmos connection string; `db` the database name.
    pub async fn connect(uri: &str, db: &str) -> Result<Directory> {
        let mut opts = ClientOptions::parse(uri).await.map_err(|e| err(format!("mongo: {e}")))?;
        // Cosmos closes idle connections aggressively; a small pool that is
        // re-established quickly beats a large one full of dead sockets.
        opts.app_name = Some("kloudlite-api".into());
        opts.max_pool_size = Some(16);
        // The driver's own command monitoring is the ONE choke point this client has: every
        // collection call in this file (and in `teams.rs`) ends as a command event carrying the
        // round trip the driver measured, so nothing has to be wrapped at the ~60 call sites.
        opts.command_event_handler = Some(mongodb::event::EventHandler::callback(on_command));
        let client = Client::with_options(opts).map_err(|e| err(format!("mongo: {e}")))?;
        let db = client.database(db);
        let m = MongoCollections {
            teams: db.collection("teams"),
            repos: db.collection("repos"),
            credentials: db.collection("credentials"),
            passkeys: db.collection("passkeys"),
            pulls: db.collection("pulls"),
            users: db.collection("users"),
            handles: db.collection("handles"),
            invites: db.collection("invites"),
            signins: db.collection("signins"),
            cli_logins: db.collection("cli_logins"),
            superadmins: db.collection("superadmins"),
            meta: db.collection("meta"),
        };
        m.ensure_indexes().await?;
        match m.lowercase_signing_fingerprints().await {
            Ok(0) => {}
            Ok(n) => tracing::info!(count = n, "directory.repair.completed"),
            Err(e) => tracing::warn!(error = %e, "directory.repair.failed"),
        }
        let dir = Directory { backend: Backend::Mongo(Box::new(m)) };
        // Cosmos's TTL is on `_ts`, not on a field of ours, so expiry is swept from here. Every
        // process that opens the directory sweeps hourly, first pass at boot; the delete is
        // idempotent and indexed, so replicas overlapping costs nothing but an empty round trip.
        let sweeper = dir.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60 * 60));
            loop {
                tick.tick().await;
                match sweeper.sweep_expired().await {
                    Ok(0) => {}
                    Ok(n) => tracing::debug!(count = n, "directory.sweep.completed"),
                    Err(e) => tracing::warn!(error = %e, "directory.sweep.failed"),
                }
            }
        });
        Ok(dir)
    }

    /// A directory that keeps its rows in this process. For tests only: nothing is persisted and
    /// nothing is shared between processes, so the `/v1` handlers can be exercised end to end
    /// without a Mongo to point them at.
    #[doc(hidden)]
    pub fn in_memory() -> Directory {
        Directory { backend: Backend::Memory(Default::default()) }
    }

}

/// Index creation and the one-shot repair, both of which are about the Mongo collections
/// themselves rather than about the directory's semantics — so they live here and
/// `connect` is their only caller.
impl MongoCollections {
    /// Cosmos will not sort or filter on a field it has no index for — it answers
    /// "the index path corresponding to the specified order-by item is excluded"
    /// rather than sorting in memory the way MongoDB does for small results. So
    /// the queries below only work once these exist. Creating an index is
    /// idempotent, so this runs on every start rather than needing a migration.
    async fn ensure_indexes(&self) -> Result<()> {
        self.teams
            .create_indexes(vec![
                // for_user filters on this
                IndexModel::builder().keys(doc! { "members.user": 1 }).build(),
                // ...and sorts on this
                IndexModel::builder().keys(doc! { "createdAt": -1 }).build(),
            ])
            .await
            .map_err(|e| err(format!("mongo: creating indexes: {e}")))?;
        self.cli_logins
            .create_indexes(vec![
                // the CLI polls by this, never by the code
                IndexModel::builder().keys(doc! { "poll": 1 }).build(),
                // `sweep_expired` deletes by this
                IndexModel::builder().keys(doc! { "expiresAt": 1 }).build(),
            ])
            .await
            .map_err(|e| err(format!("mongo: creating indexes: {e}")))?;
        self.signins
            .create_indexes(vec![IndexModel::builder().keys(doc! { "expiresAt": 1 }).build()])
            .await
            .map_err(|e| err(format!("mongo: creating indexes: {e}")))?;
        // `user_by_handle` runs on workspace create and every key add or remove; without this it
        // read every user row. Uniqueness is the `handles` collection's job, not this index's.
        self.users
            .create_indexes(vec![IndexModel::builder().keys(doc! { "username": 1 }).build()])
            .await
            .map_err(|e| err(format!("mongo: creating indexes: {e}")))?;
        self.credentials
            .create_indexes(vec![
                IndexModel::builder().keys(doc! { "owner": 1 }).build(),
                IndexModel::builder().keys(doc! { "createdAt": -1 }).build(),
                // Verifying a signature looks a key up by any fingerprint it
                // answers to; without this that is a scan of every credential.
                IndexModel::builder().keys(doc! { "fingerprints": 1 }).build(),
                // `migrate_keys_to_people` filters on kind alone; Cosmos will not scan for it.
                IndexModel::builder().keys(doc! { "kind": 1 }).build(),
            ])
            .await
            .map_err(|e| err(format!("mongo: creating indexes: {e}")))?;
        // The settings page lists a team's open invitations; the accept path reads by `_id`.
        self.invites
            .create_indexes(vec![
                IndexModel::builder().keys(doc! { "team": 1 }).build(),
                IndexModel::builder().keys(doc! { "createdAt": -1 }).build(),
                IndexModel::builder().keys(doc! { "expiresAt": 1 }).build(),
            ])
            .await
            .map_err(|e| err(format!("mongo: {e}")))?;
        self.passkeys
            .create_indexes(vec![
                IndexModel::builder().keys(doc! { "user": 1 }).build(),
                IndexModel::builder().keys(doc! { "createdAt": -1 }).build(),
            ])
            .await
            .map_err(|e| err(format!("mongo: creating indexes: {e}")))?;
        self.pulls
            .create_indexes(vec![
                IndexModel::builder().keys(doc! { "repo": 1 }).build(),
                IndexModel::builder().keys(doc! { "createdAt": -1 }).build(),
            ])
            .await
            .map_err(|e| err(format!("mongo: creating indexes: {e}")))?;
        Ok(())
    }

    /// One-shot repair for ssh signing keys registered before fingerprints were lowercased at
    /// registration (they were stored as `SHA256:<base64>`, mixed case, which `signer_by_any`
    /// can never match). Runs on every connect rather than as an admin command: it is idempotent,
    /// touches a handful of rows, and nobody has to remember to run it. Logged and swallowed by
    /// the caller — a failed repair leaves signatures unverified, which is today's behaviour, not
    /// a reason to refuse to boot.
    async fn lowercase_signing_fingerprints(&self) -> Result<usize> {
        use futures::TryStreamExt;
        let kind = mongodb::bson::to_bson(&CredentialKind::SigningKey)
            .map_err(|e| err(format!("bson: {e}")))?;
        let mut cursor = self
            .credentials
            // ponytail: a `$regex` scan of the signing-key rows on every connect — no index
            // backs it, so it is O(signing keys). Fine at this scale and a no-op once clean;
            // drop the call entirely (or gate it behind a one-time marker) if that stops holding.
            .find(doc! { "kind": kind, "fingerprints": { "$regex": "[A-Z]" } })
            .await
            .map_err(|e| err(format!("mongo: {e}")))?;
        let mut fixed = 0;
        while let Some(c) = cursor.try_next().await.map_err(|e| err(format!("mongo: {e}")))? {
            let Some(lower) = lowercased(&c.fingerprints) else { continue };
            self.credentials
                .update_one(doc! { "_id": &c.id }, doc! { "$set": { "fingerprints": lower } })
                .await
                .map_err(|e| err(format!("mongo: {e}")))?;
            fixed += 1;
        }
        Ok(fixed)
    }
}

/// `Some(lowercased)` when any fingerprint has upper-case letters, `None` when the row is already
/// in the one spelling `signer_by_any` can find. Pure so the rule has a test; `connect` applies it.
pub(crate) fn lowercased(fingerprints: &[String]) -> Option<Vec<String>> {
    let lower: Vec<String> = fingerprints.iter().map(|f| f.to_lowercase()).collect();
    (lower != fingerprints).then_some(lower)
}

/// The `meta` row saying keys have been re-filed onto people and indexed under them.
const KEYS_V2: &str = "keys_v2";

const DEP: &str = "mongo";

/// Every op `mongo_op` can answer, for `metrics::register_dependency` at boot.
pub const OPS: &[&str] = &[
    "find", "insert", "update", "delete", "find_and_modify", "count", "create_indexes", "other",
];

/// One command event, timed by the driver itself. Started events carry no duration and are
/// ignored: the pair we want is succeeded/failed, which is one record per round trip.
fn on_command(ev: mongodb::event::command::CommandEvent) {
    use kloudlite_core::metrics::dep_took;
    use mongodb::event::command::CommandEvent::*;
    match ev {
        Started(_) => {}
        Succeeded(e) => dep_took(DEP, mongo_op(&e.command_name), e.duration, None),
        Failed(e) => {
            dep_took(DEP, mongo_op(&e.command_name), e.duration, Some(kind_of(&e.failure)))
        }
        // The enum is `#[non_exhaustive]`; a future event kind is not a measurement we take.
        _ => {}
    }
}

/// The server's command name, mapped to a closed set — the wire name is whatever a driver version
/// decides to send (`hello`, `endSessions`, a future command), which is not a label we can bound.
fn mongo_op(name: &str) -> &'static str {
    match name {
        "find" | "getMore" | "aggregate" | "distinct" => "find",
        "insert" => "insert",
        "update" => "update",
        "delete" => "delete",
        "findAndModify" => "find_and_modify",
        "count" | "countDocuments" => "count",
        "createIndexes" => "create_indexes",
        _ => "other",
    }
}

/// The class of a directory failure. Cosmos answers a throttled request with `16500`/`TooManyRequests`
/// rather than an HTTP status, and that is the one class worth telling apart here: it means the
/// collection is under-provisioned, not that anything is down.
fn kind_of(e: &mongodb::error::Error) -> &'static str {
    use mongodb::error::ErrorKind;
    match &*e.kind {
        ErrorKind::Io(io) => match io.kind() {
            std::io::ErrorKind::TimedOut => "timeout",
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset => "refused",
            _ => "other",
        },
        ErrorKind::ServerSelection { .. } | ErrorKind::ConnectionPoolCleared { .. } => "refused",
        // 16500 is Cosmos's RU throttle; 50 is `MaxTimeMSExpired`, which is a timeout wearing a
        // command code.
        ErrorKind::Command(c) if c.code == 50 => "timeout",
        ErrorKind::Command(c) if c.code == 16500 || c.code_name == "TooManyRequests" => "status_429",
        ErrorKind::Command(_) | ErrorKind::Write(_) => "status_5xx",
        _ => "other",
    }
}

pub(crate) fn is_duplicate_key(e: &mongodb::error::Error) -> bool {
    use mongodb::error::ErrorKind;
    match *e.kind {
        ErrorKind::Write(mongodb::error::WriteFailure::WriteError(ref w)) => w.code == 11000,
        ErrorKind::InsertMany(ref f) => {
            f.write_errors.as_ref().is_some_and(|w| w.iter().any(|e| e.code == 11000))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    /// A 16-hex key id is not an identity: two registered keys can share one. A full
    /// fingerprint decides; a key id alone that matches two rows names nobody.
    #[tokio::test]
    async fn a_shared_key_id_attributes_a_signature_to_nobody() {
        let d = Directory::in_memory();
        let row = |id: &str, fps: &[&str]| Credential {
            id: id.into(),
            kind: CredentialKind::SigningKey,
            owner: "alice".into(),
            created_by: "alice@example.com".into(),
            name: "k".into(),
            material: String::new(),
            fingerprints: fps.iter().map(|f| f.to_string()).collect(),
            created_at: mongodb::bson::DateTime::now(),
        };
        let a = "a".repeat(24) + "0123456789abcdef";
        let b = "b".repeat(24) + "0123456789abcdef";
        d.add_credential(&row("sign:a", &[&a, "0123456789abcdef"])).await.unwrap();
        d.add_credential(&row("sign:b", &[&b, "0123456789abcdef"])).await.unwrap();
        assert!(d.signer_by_any(&["0123456789abcdef".into()]).await.unwrap().is_none(), "ambiguous");
        assert_eq!(d.signer_by_any(&[a.to_uppercase()]).await.unwrap().map(|c| c.id), Some("sign:a".into()));
        assert_eq!(d.signer_by_any(&[b.clone(), "0123456789abcdef".into()]).await.unwrap().map(|c| c.id), Some("sign:b".into()), "the fingerprint decides, the key id only narrows");
    }

    use super::{check_handle, Credential, CredentialKind, Directory};

    /// A key registered under a team moves to the person who added it; only ssh keys reach the
    /// fingerprint index; and the pass keeps returning its pairs until the caller says it has
    /// indexed them, so a boot that dies mid-index re-does the whole list rather than half of it.
    #[tokio::test]
    async fn team_keys_move_to_their_creator_once() {
        let row = |id: &str, kind, owner: &str| Credential {
            id: id.into(),
            kind,
            owner: owner.into(),
            created_by: "alice@example.com".into(),
            name: "k".into(),
            material: "ssh-ed25519 AAAA".into(),
            fingerprints: vec![],
            created_at: mongodb::bson::DateTime::now(),
        };
        let d = Directory::in_memory();
        d.add_credential(&row("SHA256:a", CredentialKind::SshKey, "acme")).await.unwrap();
        // A team's signing key moves too, but signatures are not authentication: its id must
        // never be handed to the caller to put in the ssh fingerprint index.
        d.add_credential(&row("gpg:b", CredentialKind::SigningKey, "acme")).await.unwrap();

        let moved = d.migrate_keys_to_people().await.unwrap();
        assert_eq!(moved, vec![("alice@example.com".to_string(), "SHA256:a".to_string())]);
        for id in ["SHA256:a", "gpg:b"] {
            assert_eq!(d.credential(id).await.unwrap().unwrap().owner, "alice@example.com");
        }

        // Unmarked, so the pairs come back: the caller has not said the index took them.
        assert_eq!(d.migrate_keys_to_people().await.unwrap(), moved);
        d.mark_keys_migrated().await.unwrap();
        assert!(d.migrate_keys_to_people().await.unwrap().is_empty());
    }

    /// The classifier is what a rule filters on: total, and never the error's text.
    #[test]
    fn mongo_failures_land_in_one_of_the_five_classes() {
        use mongodb::error::Error;
        let io = |k: std::io::ErrorKind| Error::from(std::io::Error::from(k));
        assert_eq!(super::kind_of(&io(std::io::ErrorKind::TimedOut)), "timeout");
        assert_eq!(super::kind_of(&io(std::io::ErrorKind::ConnectionRefused)), "refused");
        assert_eq!(super::kind_of(&io(std::io::ErrorKind::BrokenPipe)), "other");
        for e in [io(std::io::ErrorKind::TimedOut), io(std::io::ErrorKind::BrokenPipe)] {
            assert!(kloudlite_core::metrics::ERROR_KINDS.contains(&super::kind_of(&e)));
        }
    }

    /// A command name the driver invents (or a handshake) must not become a new series.
    #[test]
    fn ops_are_a_closed_set() {
        assert_eq!(super::mongo_op("findAndModify"), "find_and_modify");
        assert_eq!(super::mongo_op("hello"), "other");
        assert!(super::OPS.contains(&super::mongo_op("hello")));
    }

    #[test]
    fn lowercased_only_reports_rows_that_change() {
        use super::lowercased;
        assert_eq!(
            lowercased(&["SHA256:AbC/+=".into()]),
            Some(vec!["sha256:abc/+=".to_string()])
        );
        assert_eq!(lowercased(&["0123abcdef".into()]), None);
        assert_eq!(lowercased(&[]), None);
    }

    #[test]
    fn accepts_a_plain_handle() {
        for h in ["karthik", "alice-chen", "a-b-c", "abc", "x1y2z3"] {
            if h.len() >= 3 {
                assert!(check_handle(h).is_ok(), "{h} should be allowed");
            }
        }
    }

    #[test]
    fn refuses_what_it_says_it_refuses() {
        // Each message is shown under the field, so it has to name the actual rule.
        for (h, want) in [
            ("ab", "at least 3"),
            ("Karthik", "lowercase"),
            ("has_underscore", "letters, digits and dashes"),
            ("-lead", "start or end with a dash"),
            ("trail-", "start or end with a dash"),
            ("admin", "reserved"),
            ("api", "reserved"),
            ("cli", "reserved"),
        ] {
            let e = check_handle(h).unwrap_err().to_string();
            assert!(e.contains(want), "{h}: expected a message about {want:?}, got {e:?}");
        }
    }

    #[test]
    fn refuses_a_handle_longer_than_the_limit() {
        assert!(check_handle(&"a".repeat(40)).is_err());
        assert!(check_handle(&"a".repeat(39)).is_ok());
    }
}
