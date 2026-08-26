//! Teams: creation, lookup, and membership listing. Split out of `directory::mod` at the
//! impl-block boundary — everything else about the directory (people, repos, credentials,
//! passkeys) lives there.

use super::{check_handle, is_duplicate_key, Directory, HandleKind, Member, Role, User};
use mongodb::bson::{doc, to_bson, DateTime};
use rustic_git_core::{err, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Team {
    /// The slug. Also the namespace in every URL and clone address, which is why
    /// it is validated as an owner and can never be changed.
    #[serde(rename = "_id")]
    pub slug: String,
    pub name: String,
    /// Written after the field existed; `default` is what makes the older documents still parse.
    #[serde(default)]
    pub description: String,
    pub created_by: String,
    pub created_at: DateTime,
    pub members: Vec<Member>,
}

impl Directory {
    // ── teams ───────────────────────────────────────────────────────────────

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
            description: String::new(),
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

    /// The team and the people in it, names resolved — one query for the members, not one per
    /// member. A member whose user row is missing (deleted, or never signed in here) stays in the
    /// list with their email as the name: the page must show who holds a role, not hide them.
    pub async fn describe(&self, slug: &str) -> Result<Option<(Team, Vec<User>)>> {
        use futures::TryStreamExt;
        let Some(team) = self.get(slug).await? else { return Ok(None) };
        let emails: Vec<&str> = team.members.iter().map(|m| m.user.as_str()).collect();
        let cursor = self
            .users
            .find(doc! { "_id": { "$in": &emails } })
            .await
            .map_err(|e| err(format!("mongo: {e}")))?;
        let users: Vec<User> = cursor.try_collect().await.map_err(|e| err(format!("mongo: {e}")))?;
        Ok(Some((team, users)))
    }

    /// The caller's role in the team, if any. Every mutation below authorizes on THIS — the
    /// members array — never on who created the team or on anything in a URL.
    pub fn role_of(team: &Team, email: &str) -> Option<Role> {
        team.members.iter().find(|m| m.user.eq_ignore_ascii_case(email)).map(|m| m.role)
    }

    /// Rename and describe. The slug is deliberately not a parameter: it is in every URL and
    /// clone address, and the handle reservation is what makes it unique — changing it is a
    /// migration, not a setting.
    pub async fn update_team(&self, slug: &str, name: &str, description: &str) -> Result<bool> {
        let name = name.trim();
        if name.is_empty() {
            return Err(err("team name required"));
        }
        let r = self
            .teams
            .update_one(
                doc! { "_id": slug },
                doc! { "$set": { "name": name, "description": description.trim() } },
            )
            .await
            .map_err(|e| err(format!("mongo: {e}")))?;
        Ok(r.matched_count == 1)
    }

    /// Add an existing person. There is no invitation state: the person has to have signed in
    /// here already, so `NoSuchUser` is the answer for an email this deployment has never seen.
    /// ponytail: direct add, no pending invite; a pending collection plus a mailer replaces
    /// this the day there is something to send mail with.
    pub async fn add_member(&self, slug: &str, email: &str, role: Role) -> Result<AddMember> {
        let email = email.trim().to_lowercase();
        if self.user(&email).await?.is_none() {
            return Ok(AddMember::NoSuchUser);
        }
        // The filter carries the duplicate check, so two concurrent adds of the same person
        // cannot both push: the second finds no document whose members lack them.
        let member = Member { user: email.clone(), role, joined_at: DateTime::now() };
        let r = self
            .teams
            .update_one(
                doc! { "_id": slug, "members.user": { "$ne": &email } },
                doc! { "$push": { "members": to_bson(&member).map_err(|e| err(format!("bson: {e}")))? } },
            )
            .await
            .map_err(|e| err(format!("mongo: {e}")))?;
        if r.matched_count == 1 {
            return Ok(AddMember::Added);
        }
        // Matched nothing: either no such team, or they are already in. Tell them apart.
        Ok(match self.get(slug).await? {
            Some(_) => AddMember::AlreadyMember,
            None => AddMember::NoSuchTeam,
        })
    }

    /// Change a member's role. A team must always have an owner — one with none can never be
    /// administered again — so demoting the last owner is refused here, where every caller
    /// inherits the rule, rather than in a handler that a future route could forget.
    pub async fn set_role(&self, slug: &str, email: &str, role: Role) -> Result<Membership> {
        let email = email.trim().to_lowercase();
        let Some(team) = self.get(slug).await? else { return Ok(Membership::NoSuchTeam) };
        let Some(current) = Self::role_of(&team, &email) else { return Ok(Membership::NotAMember) };
        if current == Role::Owner && role != Role::Owner && Self::owner_count(&team) == 1 {
            return Ok(Membership::LastOwner);
        }
        // The owner check above read a snapshot; the filter here re-asserts it, so two
        // concurrent demotions cannot both pass the count and strand the team.
        let mut filter = doc! { "_id": slug, "members.user": &email };
        if current == Role::Owner && role != Role::Owner {
            filter.insert("members", doc! { "$elemMatch": { "role": "owner", "user": { "$ne": &email } } });
        }
        let r = self
            .teams
            .update_one(
                filter,
                doc! { "$set": { "members.$.role": to_bson(&role).map_err(|e| err(format!("bson: {e}")))? } },
            )
            .await
            .map_err(|e| err(format!("mongo: {e}")))?;
        Ok(if r.matched_count == 1 { Membership::Done } else { Membership::LastOwner })
    }

    /// Remove a member. Same last-owner rule as `set_role`, for the same reason.
    pub async fn remove_member(&self, slug: &str, email: &str) -> Result<Membership> {
        let email = email.trim().to_lowercase();
        let Some(team) = self.get(slug).await? else { return Ok(Membership::NoSuchTeam) };
        let Some(current) = Self::role_of(&team, &email) else { return Ok(Membership::NotAMember) };
        let mut filter = doc! { "_id": slug };
        if current == Role::Owner {
            if Self::owner_count(&team) == 1 {
                return Ok(Membership::LastOwner);
            }
            filter.insert("members", doc! { "$elemMatch": { "role": "owner", "user": { "$ne": &email } } });
        }
        let r = self
            .teams
            .update_one(filter, doc! { "$pull": { "members": { "user": &email } } })
            .await
            .map_err(|e| err(format!("mongo: {e}")))?;
        Ok(if r.matched_count == 1 { Membership::Done } else { Membership::LastOwner })
    }

    /// Hand the team from `from` to `to` in ONE update: `to` becomes owner and `from` becomes
    /// admin together, so there is never a document with two owners or none. `to` must already
    /// be a member — ownership is not how someone joins.
    pub async fn transfer(&self, slug: &str, from: &str, to: &str) -> Result<Membership> {
        let (from, to) = (from.trim().to_lowercase(), to.trim().to_lowercase());
        if from == to {
            return Ok(Membership::Done);
        }
        let Some(team) = self.get(slug).await? else { return Ok(Membership::NoSuchTeam) };
        if Self::role_of(&team, &to).is_none() {
            return Ok(Membership::NotAMember);
        }
        let r = self
            .teams
            .update_one(
                doc! { "_id": slug, "members": { "$elemMatch": { "user": &from, "role": "owner" } } },
                doc! { "$set": { "members.$[to].role": "owner", "members.$[from].role": "admin" } },
            )
            .array_filters(vec![doc! { "to.user": &to }, doc! { "from.user": &from }])
            .await
            .map_err(|e| err(format!("mongo: {e}")))?;
        Ok(if r.matched_count == 1 { Membership::Done } else { Membership::NotAMember })
    }

    /// Delete the team and give its handle back. Refused while the team still owns repositories:
    /// a repo's database, blobs and markers live on the git fleet and the object store, and
    /// nothing here can remove them transactionally. Deleting the team row first would leave
    /// them owned by a name that could then be re-registered by a stranger.
    /// ponytail: gates on repositories only, which is what the directory can see; images,
    /// workspaces and environments live in the object store and the cluster. Extend the gate
    /// when there is one place that can count all four.
    pub async fn delete_team(&self, slug: &str) -> Result<DeleteTeam> {
        let repos = self
            .repos
            .count_documents(doc! { "owner": slug })
            .await
            .map_err(|e| err(format!("mongo: {e}")))?;
        if repos > 0 {
            return Ok(DeleteTeam::StillOwns { repos });
        }
        let r = self
            .teams
            .delete_one(doc! { "_id": slug })
            .await
            .map_err(|e| err(format!("mongo: {e}")))?;
        if r.deleted_count == 0 {
            return Ok(DeleteTeam::NoSuchTeam);
        }
        self.release(slug).await?;
        Ok(DeleteTeam::Deleted)
    }

    fn owner_count(team: &Team) -> usize {
        team.members.iter().filter(|m| m.role == Role::Owner).count()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum AddMember {
    Added,
    AlreadyMember,
    NoSuchUser,
    NoSuchTeam,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Membership {
    Done,
    NotAMember,
    LastOwner,
    NoSuchTeam,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DeleteTeam {
    Deleted,
    StillOwns { repos: u64 },
    NoSuchTeam,
}
