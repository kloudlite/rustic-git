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
    pub created_at: DateTime,
    pub last_seen_at: DateTime,
}

pub struct Directory {
    teams: Collection<Team>,
    users: Collection<User>,
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
        let dir = Directory { teams: db.collection("teams"), users: db.collection("users") };
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
        Ok(())
    }

    /// Create a team with `creator` as its owner. `Ok(None)` means the slug is taken —
    /// enforced by the database, not by a prior read.
    pub async fn create(&self, slug: &str, name: &str, creator: &str) -> Result<Option<Team>> {
        if !crate::store::valid_owner(slug) {
            return Err(err("invalid team handle"));
        }
        let name = name.trim();
        if name.is_empty() {
            return Err(err("team name required"));
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
            // 11000 is the duplicate-key code: the slug is taken. Every other write
            // error is a real failure and must not read as "already exists".
            Err(e) if is_duplicate_key(&e) => Ok(None),
            Err(e) => Err(err(format!("mongo: {e}"))),
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
