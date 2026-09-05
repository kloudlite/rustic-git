//! Teams: creation, lookup, and membership listing. Split out of `directory::mod` at the
//! impl-block boundary — everything else about the directory (people, repos, credentials,
//! passkeys) lives there.

use super::{check_handle, is_duplicate_key, Backend, Directory, HandleKind, Member, Role, User};
use mongodb::bson::{doc, to_bson, DateTime};
use kloudlite_core::{err, Result};
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
    /// Whether a stranger may see this team at all. Off by default: a team is private until an
    /// owner or admin says otherwise, and the anonymous profile route answers 404 while it is off.
    #[serde(default)]
    pub public: bool,
    #[serde(default)]
    pub tagline: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub website: String,
    #[serde(default)]
    pub email: String,
    /// Bare repo names, at most `MAX_PINS`, validated against the team's listing on write only —
    /// a pin whose repo was since deleted is dropped at read time by the profile route.
    #[serde(default)]
    pub pins: Vec<String>,
    pub created_by: String,
    pub created_at: DateTime,
    pub members: Vec<Member>,
}

/// Every field empty and private, `created_at` at the epoch — a caller filling in the rest via
/// `..Default::default()` MUST set `created_at`, which `create_team` does. `bson::DateTime` has no
/// `Default` of its own, which is the only reason this is written out rather than derived.
impl Default for Team {
    fn default() -> Team {
        Team {
            slug: String::new(),
            name: String::new(),
            description: String::new(),
            public: false,
            tagline: String::new(),
            location: String::new(),
            website: String::new(),
            email: String::new(),
            pins: vec![],
            created_by: String::new(),
            created_at: DateTime::from_millis(0),
            members: vec![],
        }
    }
}

pub const MAX_PINS: usize = 6;

/// Everything on the public profile that an admin sets. Name and description stay on
/// `update_team`, which any member may call.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TeamProfile {
    pub public: bool,
    pub tagline: String,
    pub location: String,
    pub website: String,
    pub email: String,
    pub pins: Vec<String>,
}

/// Pins, deduplicated in order, capped, and each one a repo the team has. `repos` is the team's
/// full listing (private ones included — a member may pin a private repo; the profile route hides
/// it for strangers).
pub fn check_pins(pins: &[String], repos: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for p in pins {
        let p = p.trim();
        if p.is_empty() || out.iter().any(|o| o == p) {
            continue;
        }
        if !repos.iter().any(|r| r == p) {
            return Err(err(format!("no such repo to pin: {p}")));
        }
        out.push(p.to_string());
    }
    if out.len() > MAX_PINS {
        return Err(err(format!("at most {MAX_PINS} pins")));
    }
    Ok(out)
}

impl Directory {
    // ── teams ───────────────────────────────────────────────────────────────

    /// Create a team with `creator` as its owner. `Ok(None)` means the slug is taken —
    /// enforced by the database, not by a prior read.
    pub async fn create(&self, slug: &str, name: &str, creator: &str) -> Result<Option<Team>> {
        check_handle(slug)?;
        let name = name.trim();
        if name.is_empty() {
            return Err(super::invalid("team name required"));
        }
        // The same gate a username goes through, so a team can never take a handle
        // a person already holds, or the reverse.
        if !self.reserve(slug, HandleKind::Team, creator).await? {
            return Ok(None);
        }
        let now = DateTime::now();
        // Everything unset is empty and private — the profile fields are filled in later, by an
        // admin, through `set_profile`.
        let team = Team {
            slug: slug.to_string(),
            name: name.to_string(),
            created_by: creator.to_string(),
            created_at: now,
            members: vec![Member { user: creator.to_string(), role: Role::Owner, joined_at: now }],
            ..Default::default()
        };
        match &self.backend {
            Backend::Mongo(m) => match m.teams.insert_one(&team).await {
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
            },
            Backend::Memory(s) => {
                s.lock().unwrap().teams.insert(team.slug.clone(), team.clone());
                Ok(Some(team))
            }
        }
    }

    pub async fn get(&self, slug: &str) -> Result<Option<Team>> {
        match &self.backend {
            Backend::Mongo(m) => m
                .teams
                .find_one(doc! { "_id": slug })
                .await
                .map_err(|e| err(format!("mongo: {e}"))),
            Backend::Memory(s) => Ok(s.lock().unwrap().teams.get(slug).cloned()),
        }
    }

    /// Every team `user` belongs to, newest first.
    pub async fn for_user(&self, user: &str) -> Result<Vec<Team>> {
        use futures::TryStreamExt;
        match &self.backend {
            Backend::Mongo(m) => {
                let cursor = m
                    .teams
                    .find(doc! { "members.user": user })
                    .sort(doc! { "createdAt": -1 })
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?;
                cursor.try_collect().await.map_err(|e| err(format!("mongo: {e}")))
            }
            Backend::Memory(s) => {
                let found = s
                    .lock()
                    .unwrap()
                    .teams
                    .values()
                    .filter(|t| t.members.iter().any(|m| m.user == user))
                    .cloned()
                    .collect();
                Ok(super::newest_first(found, |t| t.created_at))
            }
        }
    }

    /// Only the slugs, for the caller that asks on every request and wants nothing else —
    /// `for_user` carried every member array across the wire to answer it.
    pub async fn slugs_for(&self, user: &str) -> Result<Vec<String>> {
        use futures::TryStreamExt;
        #[derive(Deserialize)]
        struct Id {
            #[serde(rename = "_id")]
            slug: String,
        }
        match &self.backend {
            Backend::Mongo(m) => m
                .teams
                .clone_with_type::<Id>()
                .find(doc! { "members.user": user })
                .projection(doc! { "_id": 1 })
                .await
                .map_err(|e| err(format!("mongo: {e}")))?
                .map_ok(|i| i.slug)
                .try_collect()
                .await
                .map_err(|e| err(format!("mongo: {e}"))),
            Backend::Memory(s) => Ok(s
                .lock()
                .unwrap()
                .teams
                .values()
                .filter(|t| t.members.iter().any(|m| m.user == user))
                .map(|t| t.slug.clone())
                .collect()),
        }
    }

    /// The team and the people in it, names resolved — one query for the members, not one per
    /// member. A member whose user row is missing (deleted, or never signed in here) stays in the
    /// list with their email as the name: the page must show who holds a role, not hide them.
    pub async fn describe(&self, slug: &str) -> Result<Option<(Team, Vec<User>)>> {
        use futures::TryStreamExt;
        let Some(team) = self.get(slug).await? else { return Ok(None) };
        let emails: Vec<&str> = team.members.iter().map(|m| m.user.as_str()).collect();
        let users: Vec<User> = match &self.backend {
            Backend::Mongo(m) => {
                let cursor = m
                    .users
                    .find(doc! { "_id": { "$in": &emails } })
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?;
                cursor.try_collect().await.map_err(|e| err(format!("mongo: {e}")))?
            }
            Backend::Memory(s) => {
                s.lock().unwrap().users.values().filter(|u| emails.contains(&u.email.as_str())).cloned().collect()
            }
        };
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
            return Err(super::invalid("team name required"));
        }
        match &self.backend {
            Backend::Mongo(m) => {
                let r = m
                    .teams
                    .update_one(
                        doc! { "_id": slug },
                        doc! { "$set": { "name": name, "description": description.trim() } },
                    )
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?;
                Ok(r.matched_count == 1)
            }
            Backend::Memory(s) => match s.lock().unwrap().teams.get_mut(slug) {
                Some(t) => {
                    t.name = name.to_string();
                    t.description = description.trim().to_string();
                    Ok(true)
                }
                None => Ok(false),
            },
        }
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
        let matched = match &self.backend {
            Backend::Mongo(m) => {
                let r = m
                    .teams
                    .update_one(
                        doc! { "_id": slug, "members.user": { "$ne": &email } },
                        doc! { "$push": { "members": to_bson(&member).map_err(|e| err(format!("bson: {e}")))? } },
                    )
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?;
                r.matched_count == 1
            }
            Backend::Memory(s) => match s.lock().unwrap().teams.get_mut(slug) {
                Some(t) if !t.members.iter().any(|m| m.user == email) => {
                    t.members.push(member);
                    true
                }
                _ => false,
            },
        };
        if matched {
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
        let demoting = current == Role::Owner && role != Role::Owner;
        if demoting && Self::owner_count(&team) == 1 {
            return Ok(Membership::LastOwner);
        }
        // The owner check above read a snapshot; the filter here re-asserts it, so two
        // concurrent demotions cannot both pass the count and strand the team.
        let matched = match &self.backend {
            Backend::Mongo(m) => {
                let mut filter = doc! { "_id": slug, "members.user": &email };
                if demoting {
                    filter.insert("members", doc! { "$elemMatch": { "role": "owner", "user": { "$ne": &email } } });
                }
                let r = m
                    .teams
                    .update_one(
                        filter,
                        doc! { "$set": { "members.$.role": to_bson(&role).map_err(|e| err(format!("bson: {e}")))? } },
                    )
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?;
                r.matched_count == 1
            }
            Backend::Memory(s) => {
                let mut s = s.lock().unwrap();
                match s.teams.get_mut(slug) {
                    Some(t)
                        if t.members.iter().any(|m| m.user == email)
                            && (!demoting
                                || t.members.iter().any(|m| m.role == Role::Owner && m.user != email)) =>
                    {
                        for m in t.members.iter_mut().filter(|m| m.user == email) {
                            m.role = role;
                        }
                        true
                    }
                    _ => false,
                }
            }
        };
        Ok(if matched { Membership::Done } else { Membership::LastOwner })
    }

    /// Remove a member. Same last-owner rule as `set_role`, for the same reason.
    pub async fn remove_member(&self, slug: &str, email: &str) -> Result<Membership> {
        let email = email.trim().to_lowercase();
        let Some(team) = self.get(slug).await? else { return Ok(Membership::NoSuchTeam) };
        let Some(current) = Self::role_of(&team, &email) else { return Ok(Membership::NotAMember) };
        let last_owner_risk = current == Role::Owner;
        if last_owner_risk && Self::owner_count(&team) == 1 {
            return Ok(Membership::LastOwner);
        }
        let matched = match &self.backend {
            Backend::Mongo(m) => {
                let mut filter = doc! { "_id": slug };
                if last_owner_risk {
                    filter.insert("members", doc! { "$elemMatch": { "role": "owner", "user": { "$ne": &email } } });
                }
                let r = m
                    .teams
                    .update_one(filter, doc! { "$pull": { "members": { "user": &email } } })
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?;
                r.matched_count == 1
            }
            Backend::Memory(s) => {
                let mut s = s.lock().unwrap();
                match s.teams.get_mut(slug) {
                    Some(t)
                        if !last_owner_risk
                            || t.members.iter().any(|m| m.role == Role::Owner && m.user != email) =>
                    {
                        t.members.retain(|m| m.user != email);
                        true
                    }
                    _ => false,
                }
            }
        };
        Ok(if matched { Membership::Done } else { Membership::LastOwner })
    }

    /// Delete the team and give its handle back. Refused while the team still owns repositories:
    /// a repo's database, blobs and markers live on the git fleet and the object store, and
    /// nothing here can remove them transactionally. Deleting the team row first would leave
    /// them owned by a name that could then be re-registered by a stranger.
    /// ponytail: gates on repositories only, which is what the directory can see; images,
    /// workspaces and environments live in the object store and the cluster. Extend the gate
    /// when there is one place that can count all four.
    pub async fn delete_team(&self, slug: &str) -> Result<DeleteTeam> {
        let deleted = match &self.backend {
            Backend::Mongo(m) => {
                let repos = m
                    .repos
                    .count_documents(doc! { "owner": slug })
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?;
                if repos > 0 {
                    return Ok(DeleteTeam::StillOwns { repos });
                }
                let r = m
                    .teams
                    .delete_one(doc! { "_id": slug })
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?;
                r.deleted_count > 0
            }
            // No `repos` rows to count: nothing has written one since repos became truth in
            // their own database, so the gate is vacuous rather than skipped.
            Backend::Memory(s) => s.lock().unwrap().teams.remove(slug).is_some(),
        };
        if !deleted {
            return Ok(DeleteTeam::NoSuchTeam);
        }
        self.release(slug).await?;
        Ok(DeleteTeam::Deleted)
    }

    pub async fn update_profile(&self, slug: &str, p: &TeamProfile) -> Result<bool> {
        match &self.backend {
            Backend::Mongo(m) => {
                let r = m
                    .teams
                    .update_one(
                        doc! { "_id": slug },
                        doc! { "$set": {
                            "public": p.public,
                            "tagline": p.tagline.trim(),
                            "location": p.location.trim(),
                            "website": p.website.trim(),
                            "email": p.email.trim(),
                            "pins": to_bson(&p.pins).map_err(|e| err(format!("bson: {e}")))?,
                        } },
                    )
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?;
                Ok(r.matched_count == 1)
            }
            Backend::Memory(s) => match s.lock().unwrap().teams.get_mut(slug) {
                Some(t) => {
                    t.public = p.public;
                    t.tagline = p.tagline.trim().to_string();
                    t.location = p.location.trim().to_string();
                    t.website = p.website.trim().to_string();
                    t.email = p.email.trim().to_string();
                    t.pins = p.pins.clone();
                    Ok(true)
                }
                None => Ok(false),
            },
        }
    }

    fn owner_count(team: &Team) -> usize {
        team.members.iter().filter(|m| m.role == Role::Owner).count()
    }

    // ── invitations ─────────────────────────────────────────────────────────
    //
    // An invitation is a row keyed by the HASH of a one-time token. The raw token exists in
    // exactly two places: the email, and the URL the recipient clicks. The directory never sees
    // it, so a dump of this collection cannot be used to join a team.

    /// Record an invitation. `id` is the caller's hash of the token; the directory does not
    /// choose the token so that it never holds anything a link could be rebuilt from.
    pub async fn create_invite(&self, invite: &Invite) -> Result<()> {
        match &self.backend {
            Backend::Mongo(m) => m
                .invites
                .insert_one(invite)
                .await
                .map(|_| ())
                .map_err(|e| err(format!("mongo: {e}"))),
            Backend::Memory(s) => {
                s.lock().unwrap().invites.insert(invite.id.clone(), invite.clone());
                Ok(())
            }
        }
    }

    /// Open invitations for a team, newest first. Expired ones are filtered here rather than
    /// by a TTL index: Cosmos's Mongo API only expires on `_ts`, and a stale row that is
    /// never shown and never accepted is harmless.
    /// ponytail: expired rows accumulate; sweep them if the collection ever matters.
    pub async fn invites_for(&self, team: &str) -> Result<Vec<Invite>> {
        use futures::TryStreamExt;
        let now = DateTime::now();
        match &self.backend {
            Backend::Mongo(m) => {
                let cursor = m
                    .invites
                    .find(doc! { "team": team, "expiresAt": { "$gt": now } })
                    .sort(doc! { "createdAt": -1 })
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?;
                cursor.try_collect().await.map_err(|e| err(format!("mongo: {e}")))
            }
            Backend::Memory(s) => {
                let found = s
                    .lock()
                    .unwrap()
                    .invites
                    .values()
                    .filter(|i| i.team == team && i.expires_at > now)
                    .cloned()
                    .collect();
                Ok(super::newest_first(found, |i| i.created_at))
            }
        }
    }

    /// Withdraw an invitation. Scoped to the team in the filter so a caller who may act on
    /// team A cannot revoke team B's invitation by knowing its id.
    pub async fn revoke_invite(&self, team: &str, id: &str) -> Result<bool> {
        match &self.backend {
            Backend::Mongo(m) => {
                let r = m
                    .invites
                    .delete_one(doc! { "_id": id, "team": team })
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?;
                Ok(r.deleted_count == 1)
            }
            Backend::Memory(s) => {
                let mut s = s.lock().unwrap();
                match s.invites.get(id) {
                    Some(i) if i.team == team => Ok(s.invites.remove(id).is_some()),
                    _ => Ok(false),
                }
            }
        }
    }

    /// The invitation behind a token hash, if it is still open.
    pub async fn invite(&self, id: &str) -> Result<Option<Invite>> {
        let now = DateTime::now();
        match &self.backend {
            Backend::Mongo(m) => m
                .invites
                .find_one(doc! { "_id": id, "expiresAt": { "$gt": now } })
                .await
                .map_err(|e| err(format!("mongo: {e}"))),
            Backend::Memory(s) => Ok(s.lock().unwrap().invites.get(id).filter(|i| i.expires_at > now).cloned()),
        }
    }

    /// Accept: the signed-in person joins with the invited role, and the invitation is spent.
    ///
    /// The email must match. An invitation is addressed to a person, and a link forwarded to
    /// somebody else must not admit them — that would make every invite a bearer credential
    /// for the team. Deleting the row FIRST is what makes it one-shot: two accepts race, one
    /// delete wins, and only the winner adds the member.
    pub async fn accept_invite(&self, id: &str, email: &str) -> Result<AcceptInvite> {
        let email = email.trim().to_lowercase();
        let Some(inv) = self.invite(id).await? else { return Ok(AcceptInvite::Gone) };
        if !inv.email.eq_ignore_ascii_case(&email) {
            return Ok(AcceptInvite::WrongEmail);
        }
        let spent = match &self.backend {
            Backend::Mongo(m) => {
                let r = m
                    .invites
                    .delete_one(doc! { "_id": id })
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?;
                r.deleted_count > 0
            }
            Backend::Memory(s) => s.lock().unwrap().invites.remove(id).is_some(),
        };
        if !spent {
            return Ok(AcceptInvite::Gone);
        }
        Ok(match self.add_member(&inv.team, &email, inv.role).await? {
            AddMember::Added | AddMember::AlreadyMember => AcceptInvite::Joined(inv.team),
            AddMember::NoSuchUser => AcceptInvite::NoSuchUser,
            AddMember::NoSuchTeam => AcceptInvite::Gone,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Invite {
    /// Hex SHA-256 of the one-time token. See the module comment above `create_invite`.
    #[serde(rename = "_id")]
    pub id: String,
    pub team: String,
    /// Lowercased, like every email here; matched case-insensitively on accept regardless.
    pub email: String,
    pub role: Role,
    pub invited_by: String,
    pub created_at: DateTime,
    pub expires_at: DateTime,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AcceptInvite {
    Joined(String),
    WrongEmail,
    NoSuchUser,
    Gone,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_are_capped_deduped_and_must_exist() {
        let repos = vec!["web".to_string(), "api".to_string(), "cli".to_string()];
        let ok = check_pins(&["web".into(), "api".into(), "web".into()], &repos).unwrap();
        assert_eq!(ok, vec!["web".to_string(), "api".to_string()], "duplicates collapse, order kept");
        assert!(check_pins(&["ghost".into()], &repos).is_err(), "a pin must name a repo of the team");
        let seven: Vec<String> = (0..7).map(|i| format!("r{i}")).collect();
        assert!(check_pins(&seven, &seven).is_err(), "at most six pins");
    }

    #[test]
    fn an_older_team_document_still_parses() {
        let old = r#"{"_id":"acme","name":"Acme","createdBy":"a@x.io","createdAt":{"$date":{"$numberLong":"0"}},"members":[]}"#;
        let t: Team = serde_json::from_str(old).unwrap();
        assert!(!t.public);
        assert!(t.pins.is_empty());
        assert_eq!(t.tagline, "");
    }
}
