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

use crate::{err, Result};
use mongodb::bson::{doc, DateTime};
use mongodb::options::ClientOptions;
use mongodb::{Client, Collection, IndexModel};
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Team {
    /// The slug. Also the namespace in every URL and clone address, which is why
    /// it is validated as an owner and can never be changed.
    #[serde(rename = "_id")]
    pub slug: String,
    pub name: String,
    pub created_by: String,
    pub created_at: DateTime,
    pub members: Vec<Member>,
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
/// The git fleet owns the repo's CONTENTS; this owns the fact that it exists and
/// what it is called. The split is not duplication — they answer different
/// questions. Each repo has its own database under `repo/{owner}/{name}` in the
/// object store, so "which repos does this owner have, and what are they" cannot
/// be answered there without opening every one of them, which is the second-writer
/// problem the whole design exists to avoid. A LIST of that prefix yields names
/// and nothing else: no description, no visibility, no timestamps.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Repo {
    /// `owner/name` — the clone path, and the reason uniqueness is the database's
    /// rather than a check-then-insert two requests could interleave.
    #[serde(rename = "_id")]
    pub id: String,
    pub owner: String,
    pub name: String,
    /// Public repos are readable by strangers. Mirrors the flag the owning git
    /// node enforces; this copy exists so a listing does not have to ask the node
    /// about every row. The node's copy is the one that AUTHORIZES.
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
    /// The token's sha256 digest, or the key's fingerprint. Also what the object
    /// key is named after, so revoking needs nothing else.
    #[serde(rename = "_id")]
    pub id: String,
    pub kind: CredentialKind,
    /// The namespace this credential acts in.
    pub owner: String,
    /// The person who created it — so a team's members can see whose it is.
    pub created_by: String,
    /// What they called it. For an ssh key this is the comment or a given title.
    pub name: String,
    pub created_at: DateTime,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CredentialKind {
    Token,
    SshKey,
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
    "kloudlite", "login", "logout", "new", "root", "settings", "signup", "static", "status",
    "support", "system", "team", "teams", "user", "users", "www",
];

/// `Ok(())` if this could be someone's handle. The rules are the namespace's, so
/// a username and a team slug are held to exactly the same ones.
pub fn check_handle(h: &str) -> Result<()> {
    if h.len() < 3 {
        return Err(err("handle must be at least 3 characters"));
    }
    if h.len() > 39 {
        return Err(err("handle must be 39 characters or fewer"));
    }
    if h != h.to_lowercase() {
        return Err(err("handle must be lowercase"));
    }
    // Stricter than `valid_owner`, which also permits underscores: a handle is
    // read aloud, typed from memory and rendered under a link underline, where an
    // underscore is easy to miss. Repo names keep the looser rule; this namespace
    // does not.
    if !h.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        return Err(err("handle may use letters, digits and dashes only"));
    }
    if h.starts_with('-') || h.ends_with('-') {
        return Err(err("handle may not start or end with a dash"));
    }
    // Before `valid_owner`: several reserved words are also refused there, and
    // "that handle is reserved" tells the person something the generic message
    // does not.
    if RESERVED.contains(&h) {
        return Err(err("that handle is reserved"));
    }
    if !crate::store::valid_owner(h) {
        return Err(err("that handle cannot be used"));
    }
    Ok(())
}

pub struct Directory {
    teams: Collection<Team>,
    repos: Collection<Repo>,
    credentials: Collection<Credential>,
    passkeys: Collection<Passkey>,
    users: Collection<User>,
    handles: Collection<Handle>,
}

impl Directory {
    /// `uri` is the Cosmos connection string; `db` the database name.
    pub async fn connect(uri: &str, db: &str) -> Result<Directory> {
        let mut opts = ClientOptions::parse(uri).await.map_err(|e| err(format!("mongo: {e}")))?;
        // Cosmos closes idle connections aggressively; a small pool that is
        // re-established quickly beats a large one full of dead sockets.
        opts.app_name = Some("rustic-git-api".into());
        opts.max_pool_size = Some(16);
        let client = Client::with_options(opts).map_err(|e| err(format!("mongo: {e}")))?;
        let db = client.database(db);
        let dir = Directory {
            teams: db.collection("teams"),
            repos: db.collection("repos"),
            credentials: db.collection("credentials"),
            passkeys: db.collection("passkeys"),
            users: db.collection("users"),
            handles: db.collection("handles"),
        };
        dir.ensure_indexes().await?;
        Ok(dir)
    }

    // ── people ──────────────────────────────────────────────────────────────

    /// Record that this person exists and has just been seen. Called on every
    /// sign-in, so it must be an upsert: the first one creates the row, the rest
    /// only move `lastSeenAt` and refresh the display name.
    pub async fn upsert_user(&self, email: &str, name: &str) -> Result<User> {
        let email = email.trim().to_lowercase();
        if !email.contains('@') {
            return Err(err("a valid email is required"));
        }
        let name = if name.trim().is_empty() { email.split('@').next().unwrap_or(&email) } else { name.trim() };
        let now = DateTime::now();
        self.users
            .update_one(
                doc! { "_id": &email },
                doc! {
                    "$set": { "name": name, "lastSeenAt": now },
                    // Only on insert: a returning user keeps the date they joined.
                    "$setOnInsert": { "createdAt": now },
                },
            )
            .upsert(true)
            .await
            .map_err(|e| err(format!("mongo: {e}")))?;
        self.user(&email)
            .await?
            .ok_or_else(|| err("user vanished immediately after upsert"))
    }

    /// Reserve `handle` for `kind`, held by `held_by`. `Ok(false)` means it is
    /// already taken — by a user or a team, which is the point of one collection.
    async fn reserve(&self, handle: &str, kind: HandleKind, held_by: &str) -> Result<bool> {
        let doc = Handle {
            handle: handle.to_string(),
            kind,
            held_by: held_by.to_string(),
            created_at: DateTime::now(),
        };
        match self.handles.insert_one(&doc).await {
            Ok(_) => Ok(true),
            Err(e) if is_duplicate_key(&e) => Ok(false),
            Err(e) => Err(err(format!("mongo: {e}"))),
        }
    }

    async fn release(&self, handle: &str) -> Result<()> {
        self.handles
            .delete_one(doc! { "_id": handle })
            .await
            .map(|_| ())
            .map_err(|e| err(format!("mongo: {e}")))
    }

    /// Claim a username. `Ok(None)` means the handle is taken.
    ///
    /// Reserving comes first and is the gate: two people racing for one handle
    /// both reach the insert, and exactly one wins. Only then is it written to the
    /// user. If that second write fails the reservation is released, so a failure
    /// cannot leave a handle held by nobody.
    pub async fn claim_username(&self, email: &str, handle: &str) -> Result<Option<User>> {
        let email = email.trim().to_lowercase();
        let handle = handle.trim().to_lowercase();
        check_handle(&handle)?;

        let existing = self.user(&email).await?.ok_or_else(|| err("no such user"))?;
        if let Some(current) = &existing.username {
            // Not an error: asking again for the handle you already hold is a
            // retry, and should look like it worked.
            return if *current == handle { Ok(Some(existing)) } else { Err(err("username already set")) };
        }
        if !self.reserve(&handle, HandleKind::User, &email).await? {
            return Ok(None);
        }
        let set = self
            .users
            .update_one(doc! { "_id": &email }, doc! { "$set": { "username": &handle } })
            .await;
        match set {
            Ok(_) => self.user(&email).await,
            Err(e) => {
                // Compensate, or the handle is reserved for a user who does not
                // carry it — unclaimable by anyone, forever.
                let _ = self.release(&handle).await;
                Err(err(format!("mongo: {e}")))
            }
        }
    }

    pub async fn user(&self, email: &str) -> Result<Option<User>> {
        self.users
            .find_one(doc! { "_id": email.trim().to_lowercase() })
            .await
            .map_err(|e| err(format!("mongo: {e}")))
    }

    // ── teams ───────────────────────────────────────────────────────────────

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
        self.repos
            .create_indexes(vec![
                // for_owner filters on this...
                IndexModel::builder().keys(doc! { "owner": 1 }).build(),
                // ...and sorts on this
                IndexModel::builder().keys(doc! { "createdAt": -1 }).build(),
            ])
            .await
            .map_err(|e| err(format!("mongo: creating indexes: {e}")))?;
        self.credentials
            .create_indexes(vec![
                IndexModel::builder().keys(doc! { "owner": 1 }).build(),
                IndexModel::builder().keys(doc! { "createdAt": -1 }).build(),
            ])
            .await
            .map_err(|e| err(format!("mongo: creating indexes: {e}")))?;
        self.passkeys
            .create_indexes(vec![
                IndexModel::builder().keys(doc! { "user": 1 }).build(),
                IndexModel::builder().keys(doc! { "createdAt": -1 }).build(),
            ])
            .await
            .map_err(|e| err(format!("mongo: creating indexes: {e}")))?;
        Ok(())
    }

    /// Create a team with `creator` as its owner. `Ok(None)` means the slug is taken —
    /// enforced by the database, not by a prior read.
    pub async fn create(&self, slug: &str, name: &str, creator: &str) -> Result<Option<Team>> {
        check_handle(slug)?;
        let name = name.trim();
        if name.is_empty() {
            return Err(err("team name required"));
        }
        // The same gate a username goes through, so a team can never take a handle
        // a person already holds, or the reverse.
        if !self.reserve(slug, HandleKind::Team, creator).await? {
            return Ok(None);
        }
        let now = DateTime::now();
        let team = Team {
            slug: slug.to_string(),
            name: name.to_string(),
            created_by: creator.to_string(),
            created_at: now,
            members: vec![Member { user: creator.to_string(), role: Role::Owner, joined_at: now }],
        };
        match self.teams.insert_one(&team).await {
            Ok(_) => Ok(Some(team)),
            // The reservation already decided uniqueness; reaching here means the
            // team document itself failed, so give the handle back.
            Err(e) => {
                let _ = self.release(slug).await;
                if is_duplicate_key(&e) {
                    return Ok(None);
                }
                Err(err(format!("mongo: {e}")))
            }
        }
    }

    pub async fn get(&self, slug: &str) -> Result<Option<Team>> {
        self.teams
            .find_one(doc! { "_id": slug })
            .await
            .map_err(|e| err(format!("mongo: {e}")))
    }

    /// Every team `user` belongs to, newest first.
    pub async fn for_user(&self, user: &str) -> Result<Vec<Team>> {
        use futures::TryStreamExt;
        let cursor = self
            .teams
            .find(doc! { "members.user": user })
            .sort(doc! { "createdAt": -1 })
            .await
            .map_err(|e| err(format!("mongo: {e}")))?;
        cursor.try_collect().await.map_err(|e| err(format!("mongo: {e}")))
    }

    // ── repos ───────────────────────────────────────────────────────────────

    /// Claim `owner/name`. `Ok(None)` means it is taken — decided by the unique
    /// `_id`, so two simultaneous creates cannot both win.
    ///
    /// This runs BEFORE the repo is created on the git fleet, and that order is
    /// the point: the name is reserved atomically here, so the fleet is only ever
    /// asked to create a name nobody else holds. A fleet failure then unwinds with
    /// `forget`, which is a delete of a row created microseconds earlier.
    pub async fn claim_repo(
        &self,
        owner: &str,
        name: &str,
        public: bool,
        description: &str,
        creator: &str,
    ) -> Result<Option<Repo>> {
        if !crate::store::valid_owner(owner) || !crate::store::valid_segment(name) {
            return Err(err("invalid repository name"));
        }
        let repo = Repo {
            id: format!("{owner}/{name}"),
            owner: owner.to_string(),
            name: name.to_string(),
            public,
            description: description.trim().to_string(),
            created_by: creator.to_string(),
            created_at: DateTime::now(),
        };
        match self.repos.insert_one(&repo).await {
            Ok(_) => Ok(Some(repo)),
            Err(e) if is_duplicate_key(&e) => Ok(None),
            Err(e) => Err(err(format!("mongo: {e}"))),
        }
    }

    /// Change what a repo says about itself. Visibility is mirrored here for the
    /// listing badge; the git node's copy is the one that AUTHORIZES, so this is
    /// written after the node has accepted the change, never before.
    pub async fn update_repo(
        &self,
        owner: &str,
        name: &str,
        description: Option<&str>,
        public: Option<bool>,
    ) -> Result<()> {
        let mut set = doc! {};
        if let Some(d) = description {
            set.insert("description", d.trim());
        }
        if let Some(p) = public {
            set.insert("public", p);
        }
        if set.is_empty() {
            return Ok(());
        }
        self.repos
            .update_one(doc! { "_id": format!("{owner}/{name}") }, doc! { "$set": set })
            .await
            .map(|_| ())
            .map_err(|e| err(format!("mongo: {e}")))
    }

    /// Drop a claim. Only for unwinding a `claim_repo` whose fleet create failed —
    /// deleting a repo that exists is the fleet's business, not the index's.
    pub async fn forget_repo(&self, owner: &str, name: &str) -> Result<()> {
        self.repos
            .delete_one(doc! { "_id": format!("{owner}/{name}") })
            .await
            .map(|_| ())
            .map_err(|e| err(format!("mongo: {e}")))
    }

    /// Every repo under `owner`, newest first. Both public and private: who may
    /// see this list is the caller's question, decided before this is called.
    pub async fn repos_for(&self, owner: &str) -> Result<Vec<Repo>> {
        use futures::TryStreamExt;
        let cursor = self
            .repos
            .find(doc! { "owner": owner })
            .sort(doc! { "createdAt": -1 })
            .await
            .map_err(|e| err(format!("mongo: {e}")))?;
        cursor.try_collect().await.map_err(|e| err(format!("mongo: {e}")))
    }

    // ── credentials ─────────────────────────────────────────────────────────

    /// Record a credential. `Ok(None)` means this exact credential is already
    /// registered — which for an ssh key means the same key, and is worth saying
    /// rather than silently re-adding.
    pub async fn add_credential(&self, c: &Credential) -> Result<Option<()>> {
        match self.credentials.insert_one(c).await {
            Ok(_) => Ok(Some(())),
            Err(e) if is_duplicate_key(&e) => Ok(None),
            Err(e) => Err(err(format!("mongo: {e}"))),
        }
    }

    pub async fn credentials_for(&self, owner: &str, kind: CredentialKind) -> Result<Vec<Credential>> {
        use futures::TryStreamExt;
        let kind = mongodb::bson::to_bson(&kind).map_err(|e| err(format!("bson: {e}")))?;
        let cursor = self
            .credentials
            .find(doc! { "owner": owner, "kind": kind })
            .sort(doc! { "createdAt": -1 })
            .await
            .map_err(|e| err(format!("mongo: {e}")))?;
        cursor.try_collect().await.map_err(|e| err(format!("mongo: {e}")))
    }

    /// Look one up to check its owner before revoking it. Revocation is authorized
    /// against the credential's OWNER, not against whoever holds the id — an id is
    /// a digest, and a digest is guessable in principle if the secret is known.
    pub async fn credential(&self, id: &str) -> Result<Option<Credential>> {
        self.credentials
            .find_one(doc! { "_id": id })
            .await
            .map_err(|e| err(format!("mongo: {e}")))
    }

    pub async fn forget_credential(&self, id: &str) -> Result<()> {
        self.credentials
            .delete_one(doc! { "_id": id })
            .await
            .map(|_| ())
            .map_err(|e| err(format!("mongo: {e}")))
    }

    // ── passkeys ────────────────────────────────────────────────────────────

    /// `Ok(None)` means this credential id is already registered — which means the
    /// same authenticator was enrolled twice, not that anything is wrong.
    pub async fn add_passkey(&self, p: &Passkey) -> Result<Option<()>> {
        match self.passkeys.insert_one(p).await {
            Ok(_) => Ok(Some(())),
            Err(e) if is_duplicate_key(&e) => Ok(None),
            Err(e) => Err(err(format!("mongo: {e}"))),
        }
    }

    /// By credential id — the lookup a sign-in makes, before it knows who is
    /// signing in. That is the whole point of a discoverable credential: the
    /// authenticator names the account.
    pub async fn passkey(&self, id: &str) -> Result<Option<Passkey>> {
        self.passkeys
            .find_one(doc! { "_id": id })
            .await
            .map_err(|e| err(format!("mongo: {e}")))
    }

    pub async fn passkeys_for(&self, user: &str) -> Result<Vec<Passkey>> {
        use futures::TryStreamExt;
        let cursor = self
            .passkeys
            .find(doc! { "user": user.trim().to_lowercase() })
            .sort(doc! { "createdAt": -1 })
            .await
            .map_err(|e| err(format!("mongo: {e}")))?;
        cursor.try_collect().await.map_err(|e| err(format!("mongo: {e}")))
    }

    /// Record that a passkey was just used. The counter is what detects a cloned
    /// authenticator, so it is stored on every successful sign-in rather than only
    /// when convenient.
    pub async fn advance_passkey(&self, id: &str, counter: i64) -> Result<()> {
        self.passkeys
            .update_one(doc! { "_id": id }, doc! { "$set": { "counter": counter } })
            .await
            .map(|_| ())
            .map_err(|e| err(format!("mongo: {e}")))
    }

    pub async fn forget_passkey(&self, id: &str) -> Result<()> {
        self.passkeys
            .delete_one(doc! { "_id": id })
            .await
            .map(|_| ())
            .map_err(|e| err(format!("mongo: {e}")))
    }
}

fn is_duplicate_key(e: &mongodb::error::Error) -> bool {
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
    use super::check_handle;

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
