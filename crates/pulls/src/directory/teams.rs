//! Teams: creation, lookup, and membership listing. Split out of `directory::mod` at the
//! impl-block boundary — everything else about the directory (people, repos, credentials,
//! passkeys) lives there.

use super::{check_handle, is_duplicate_key, Backend, Directory, HandleKind, Member, MemberState, Role, User};
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
    /// The region this team's benches live in; empty = unbound. Set once, by `bind_region`.
    #[serde(default)]
    pub region: String,
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
            region: String::new(),
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

    /// Create a team with `creator` as its owner, bound to `region` in the same insert — a team
    /// is placed once, when it is made, and stays there (empty only for callers predating that
    /// rule, i.e. tests). `Ok(None)` means the slug is taken — enforced by the database, not by a
    /// prior read.
    pub async fn create(&self, slug: &str, name: &str, creator: &str, region: &str) -> Result<Option<Team>> {
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
            region: region.to_string(),
            created_by: creator.to_string(),
            created_at: now,
            members: vec![Member { user: creator.to_string(), role: Role::Owner, joined_at: now, state: MemberState::Active, paused_at: None, paused_by: None }],
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

    /// Slugs of every team with no region yet — the one-off backfill's work list. Unbounded on
    /// purpose: it runs once at an admin boot, and a capped list would silently skip teams.
    pub async fn unbound_teams(&self) -> Result<Vec<String>> {
        use futures::TryStreamExt;
        match &self.backend {
            Backend::Mongo(m) => {
                let filter = doc! { "$or": [{ "region": { "$exists": false } }, { "region": "" }] };
                let cursor = m.teams.find(filter).await.map_err(|e| err(format!("mongo: {e}")))?;
                let teams: Vec<Team> = cursor.try_collect().await.map_err(|e| err(format!("mongo: {e}")))?;
                Ok(teams.into_iter().map(|t| t.slug).collect())
            }
            Backend::Memory(s) => {
                Ok(s.lock().unwrap().teams.values().filter(|t| t.region.is_empty()).map(|t| t.slug.clone()).collect())
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

    /// Is `email` a member of team `slug`, at any role. One read, no cache: the callers that need
    /// one (the git tier's ssh path) keep their own, so this stays the truth.
    pub async fn is_member(&self, email: &str, slug: &str) -> Result<bool> {
        Ok(self.get(slug).await?.is_some_and(|t| Self::role_of(&t, email).is_some()))
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
                    .limit(super::LISTING_LIMIT)
                    .max_time(super::QUERY_MAX_TIME)
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
                .find(doc! { "members": { "$elemMatch": { "user": user, "state": { "$ne": "paused" } } } })
                .projection(doc! { "_id": 1 })
                .limit(super::LISTING_LIMIT)
                .max_time(super::QUERY_MAX_TIME)
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
                .filter(|t| t.members.iter().any(|m| m.user == user && m.state != MemberState::Paused))
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
                    .max_time(super::QUERY_MAX_TIME)
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

    /// Compare-and-set on the empty value: a team slug, else a person by handle. Returns the region
    /// the slug is bound to after the call — `region` itself, or what it already held — and `None`
    /// for no such owner. The filter carries the emptiness check, so two concurrent binds cannot
    /// both land.
    // ponytail: set once; moving a team to another region is a migration of its benches' folders, designed when needed.
    pub async fn bind_region(&self, slug: &str, region: &str) -> Result<Option<String>> {
        let handle = slug.trim().to_lowercase();
        match &self.backend {
            Backend::Mongo(m) => {
                let unbound = doc! { "$or": [{ "region": { "$exists": false } }, { "region": "" }] };
                let set = doc! { "$set": { "region": region } };
                let mut filter = doc! { "_id": slug };
                filter.extend(unbound.clone());
                m.teams.update_one(filter, set.clone()).await.map_err(|e| err(format!("mongo: {e}")))?;
                if let Some(t) = self.get(slug).await? {
                    return Ok(Some(t.region));
                }
                let mut filter = doc! { "username": &handle };
                filter.extend(unbound);
                m.users.update_one(filter, set).await.map_err(|e| err(format!("mongo: {e}")))?;
                Ok(self.user_by_handle(&handle).await?.map(|u| u.region))
            }
            Backend::Memory(s) => {
                let mut s = s.lock().unwrap();
                let slot = match s.teams.get_mut(slug) {
                    Some(t) => &mut t.region,
                    None => match s.users.values_mut().find(|u| u.username.as_deref() == Some(&handle)) {
                        Some(u) => &mut u.region,
                        None => return Ok(None),
                    },
                };
                if slot.is_empty() {
                    *slot = region.to_string();
                }
                Ok(Some(slot.clone()))
            }
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
        let member = Member { user: email.clone(), role, joined_at: DateTime::now(), state: MemberState::Active, paused_at: None, paused_by: None };
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

    /// Pause or resume a member. Pausing keeps the row and role; it only drops the team from
    /// `slugs_for`. A team must keep an ACTIVE owner — a paused one cannot administer it — so
    /// pausing the last one is refused, in the filter as well as the snapshot, like `set_role`.
    pub async fn set_member_state(&self, slug: &str, email: &str, state: MemberState, by: &str) -> Result<Membership> {
        let email = email.trim().to_lowercase();
        let Some(team) = self.get(slug).await? else { return Ok(Membership::NoSuchTeam) };
        let Some(current) = team.members.iter().find(|m| m.user == email) else { return Ok(Membership::NotAMember) };
        let active_owner = |m: &Member| m.role == Role::Owner && m.state == MemberState::Active;
        let guarded = state == MemberState::Paused && active_owner(current);
        if guarded && team.members.iter().filter(|m| active_owner(m)).count() == 1 {
            return Ok(Membership::LastOwner);
        }
        let now = DateTime::now();
        let matched = match &self.backend {
            Backend::Mongo(m) => {
                let mut filter = doc! { "_id": slug, "members.user": &email };
                if guarded {
                    filter.insert(
                        "members",
                        doc! { "$elemMatch": { "role": "owner", "user": { "$ne": &email }, "state": { "$ne": "paused" } } },
                    );
                }
                // arrayFilters, not the positional `$`: with an $elemMatch in the filter `$` would
                // point at the OTHER owner, not the member being paused.
                let update = match state {
                    MemberState::Paused => doc! { "$set": {
                        "members.$[m].state": "paused", "members.$[m].pausedAt": now, "members.$[m].pausedBy": by,
                    } },
                    MemberState::Active => doc! {
                        "$set": { "members.$[m].state": "active" },
                        "$unset": { "members.$[m].pausedAt": "", "members.$[m].pausedBy": "" },
                    },
                };
                let r = m
                    .teams
                    .update_one(filter, update)
                    .array_filters(vec![doc! { "m.user": &email }])
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?;
                r.matched_count == 1
            }
            Backend::Memory(s) => {
                let mut s = s.lock().unwrap();
                match s.teams.get_mut(slug) {
                    Some(t)
                        if t.members.iter().any(|m| m.user == email)
                            && (!guarded || t.members.iter().any(|m| active_owner(m) && m.user != email)) =>
                    {
                        for m in t.members.iter_mut().filter(|m| m.user == email) {
                            m.state = state;
                            (m.paused_at, m.paused_by) = match state {
                                MemberState::Paused => (Some(now), Some(by.to_string())),
                                MemberState::Active => (None, None),
                            };
                        }
                        true
                    }
                    _ => false,
                }
            }
        };
        Ok(if matched { Membership::Done } else { Membership::LastOwner })
    }

    /// Strict: `Ok(None)` means the team exists and the person is not in it; a failed read is an
    /// error, never "not a member", so a caller deciding to delete data cannot act on an outage.
    pub async fn membership(&self, slug: &str, email: &str) -> std::result::Result<Option<MemberState>, MembershipErr> {
        let team = self.get(slug).await.map_err(|e| MembershipErr::Read(e.to_string()))?.ok_or(MembershipErr::NoSuchTeam)?;
        Ok(team.members.iter().find(|m| m.user.eq_ignore_ascii_case(email.trim())).map(|m| m.state))
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
                    .limit(super::LISTING_LIMIT)
                    .max_time(super::QUERY_MAX_TIME)
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
pub enum MembershipErr {
    NoSuchTeam,
    Read(String),
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

    #[tokio::test]
    async fn a_region_binds_once_to_a_team_or_a_person_and_never_moves() {
        let d = Directory::in_memory();
        d.upsert_user("alice@x.io", "Alice").await.unwrap();
        d.claim_username("alice@x.io", "alice").await.unwrap().unwrap();
        d.create("acme", "Acme", "alice@x.io", "").await.unwrap().unwrap();
        assert_eq!(d.get("acme").await.unwrap().unwrap().region, "", "an existing team is unbound");
        assert_eq!(d.bind_region("acme", "r1").await.unwrap().as_deref(), Some("r1"));
        assert_eq!(d.bind_region("acme", "r2").await.unwrap().as_deref(), Some("r1"), "set once");
        assert_eq!(d.bind_region("alice", "r2").await.unwrap().as_deref(), Some("r2"), "a person's handle binds their own record");
        assert_eq!(d.user_by_handle("alice").await.unwrap().unwrap().region, "r2");
        assert_eq!(d.bind_region("nobody", "r1").await.unwrap(), None);
    }

    #[tokio::test]
    async fn create_stores_the_region_with_the_team() {
        let d = Directory::in_memory();
        d.upsert_user("alice@x.io", "Alice").await.unwrap();
        let t = d.create("acme", "Acme", "alice@x.io", "r1").await.unwrap().unwrap();
        assert_eq!(t.region, "r1");
        assert_eq!(d.get("acme").await.unwrap().unwrap().region, "r1");
        assert_eq!(d.bind_region("acme", "r2").await.unwrap().as_deref(), Some("r1"), "created bound stays bound");
    }

    #[tokio::test]
    async fn unbound_teams_lists_only_teams_without_a_region() {
        let d = Directory::in_memory();
        d.upsert_user("alice@x.io", "Alice").await.unwrap();
        d.claim_username("alice@x.io", "alice").await.unwrap().unwrap();
        d.create("old", "Old", "alice@x.io", "").await.unwrap().unwrap();
        d.create("fresh", "Fresh", "alice@x.io", "r1").await.unwrap().unwrap();
        assert_eq!(d.unbound_teams().await.unwrap(), vec!["old".to_string()], "a person's handle is never listed");
    }

    #[tokio::test]
    async fn unbound_users_lists_handles_without_a_region_and_binds_by_email_once() {
        let d = Directory::in_memory();
        d.upsert_user("alice@x.io", "Alice").await.unwrap();
        d.claim_username("alice@x.io", "alice").await.unwrap().unwrap();
        d.upsert_user("nohandle@x.io", "N").await.unwrap();
        d.create("old", "Old", "alice@x.io", "").await.unwrap().unwrap();
        assert_eq!(d.unbound_users().await.unwrap(), vec!["alice".to_string()], "teams and handle-less people are not listed");
        assert_eq!(d.bind_user_region("NoHandle@x.io", "r1").await.unwrap().as_deref(), Some("r1"));
        assert_eq!(d.bind_user_region("nohandle@x.io", "r2").await.unwrap().as_deref(), Some("r1"), "set once");
        assert_eq!(d.bind_user_region("ghost@x.io", "r1").await.unwrap(), None);
    }

    #[test]
    fn an_older_team_document_still_parses() {
        let old = r#"{"_id":"acme","name":"Acme","createdBy":"a@x.io","createdAt":{"$date":{"$numberLong":"0"}},"members":[]}"#;
        let t: Team = serde_json::from_str(old).unwrap();
        assert!(!t.public);
        assert!(t.pins.is_empty());
        assert_eq!(t.tagline, "");
    }

    #[test]
    fn a_member_row_without_state_reads_active() {
        let old = r#"{"user":"a@x.io","role":"owner","joinedAt":{"$date":{"$numberLong":"0"}}}"#;
        let m: Member = serde_json::from_str(old).unwrap();
        assert_eq!((m.state, m.paused_at, m.paused_by), (MemberState::Active, None, None));
    }

    async fn team_of_two() -> Directory {
        let d = Directory::in_memory();
        d.upsert_user("alice@x.io", "Alice").await.unwrap();
        d.upsert_user("bob@x.io", "Bob").await.unwrap();
        d.create("acme", "Acme", "alice@x.io", "").await.unwrap().unwrap();
        assert_eq!(d.add_member("acme", "bob@x.io", Role::Member).await.unwrap(), AddMember::Added);
        d
    }

    fn member(d: &Team, email: &str) -> Member {
        d.members.iter().find(|m| m.user == email).cloned().unwrap()
    }

    #[tokio::test]
    async fn set_member_state_round_trips_and_stamps_paused_at_and_by() {
        let d = team_of_two().await;
        assert_eq!(d.set_member_state("acme", "Bob@x.io", MemberState::Paused, "alice@x.io").await.unwrap(), Membership::Done);
        let b = member(&d.get("acme").await.unwrap().unwrap(), "bob@x.io");
        assert_eq!(b.state, MemberState::Paused);
        assert!(b.paused_at.is_some());
        assert_eq!(b.paused_by.as_deref(), Some("alice@x.io"));
        assert_eq!(b.role, Role::Member, "the role is kept");
        assert_eq!(d.set_member_state("acme", "bob@x.io", MemberState::Active, "alice@x.io").await.unwrap(), Membership::Done);
        let b = member(&d.get("acme").await.unwrap().unwrap(), "bob@x.io");
        assert_eq!((b.state, b.paused_at, b.paused_by), (MemberState::Active, None, None));
        assert_eq!(d.set_member_state("acme", "ghost@x.io", MemberState::Paused, "a").await.unwrap(), Membership::NotAMember);
        assert_eq!(d.set_member_state("nope", "bob@x.io", MemberState::Paused, "a").await.unwrap(), Membership::NoSuchTeam);
    }

    #[tokio::test]
    async fn the_last_active_owner_cannot_be_paused() {
        let d = team_of_two().await;
        assert_eq!(d.set_member_state("acme", "alice@x.io", MemberState::Paused, "x").await.unwrap(), Membership::LastOwner);
        assert_eq!(d.set_role("acme", "bob@x.io", Role::Owner).await.unwrap(), Membership::Done);
        assert_eq!(d.set_member_state("acme", "bob@x.io", MemberState::Paused, "x").await.unwrap(), Membership::Done);
        assert_eq!(
            d.set_member_state("acme", "alice@x.io", MemberState::Paused, "x").await.unwrap(),
            Membership::LastOwner,
            "a paused owner does not count"
        );
    }

    #[tokio::test]
    async fn slugs_for_omits_a_paused_team() {
        let d = team_of_two().await;
        assert_eq!(d.slugs_for("bob@x.io").await.unwrap(), vec!["acme".to_string()]);
        d.set_member_state("acme", "bob@x.io", MemberState::Paused, "alice@x.io").await.unwrap();
        assert!(d.slugs_for("bob@x.io").await.unwrap().is_empty());
        assert_eq!(d.slugs_for("alice@x.io").await.unwrap(), vec!["acme".to_string()]);
    }

    #[tokio::test]
    async fn membership_distinguishes_no_team_not_member_and_paused() {
        let d = team_of_two().await;
        assert_eq!(d.membership("nope", "bob@x.io").await, Err(MembershipErr::NoSuchTeam));
        assert_eq!(d.membership("acme", "ghost@x.io").await, Ok(None));
        assert_eq!(d.membership("acme", "bob@x.io").await, Ok(Some(MemberState::Active)));
        d.set_member_state("acme", "bob@x.io", MemberState::Paused, "alice@x.io").await.unwrap();
        assert_eq!(d.membership("acme", "bob@x.io").await, Ok(Some(MemberState::Paused)));
    }
}
