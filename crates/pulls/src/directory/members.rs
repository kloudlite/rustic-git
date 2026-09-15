//! Membership writes and the strict membership read: add, change role, pause, remove. Split out
//! of `directory::teams`, which keeps the team document itself, listings and invitations.

use super::{Backend, Directory, Member, MemberState, Role};
use kloudlite_core::{err, Result};
use mongodb::bson::{doc, to_bson, DateTime};

impl Directory {
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
        // Already there: a re-pause must keep the original pausedAt/pausedBy.
        if current.state == state {
            return Ok(Membership::Done);
        }
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
        if matched {
            return Ok(Membership::Done);
        }
        // Nothing matched: either the guard held (last active owner) or the row went meanwhile.
        let gone = self.get(slug).await?.is_none_or(|t| !t.members.iter().any(|m| m.user == email));
        Ok(if gone { Membership::NotAMember } else { Membership::LastOwner })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::Team;

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

    #[tokio::test]
    async fn a_paused_member_is_not_a_member_for_access_but_keeps_their_row() {
        let d = team_of_two().await;
        d.set_member_state("acme", "bob@x.io", MemberState::Paused, "alice@x.io").await.unwrap();
        assert!(!d.is_member("bob@x.io", "acme").await.unwrap(), "may_act reads this");
        let t = d.get("acme").await.unwrap().unwrap();
        assert_eq!(Directory::active_role_of(&t, "bob@x.io"), None);
        assert_eq!(Directory::role_of(&t, "bob@x.io"), Some(Role::Member));
        assert!(d.is_member("alice@x.io", "acme").await.unwrap());
    }

    #[tokio::test]
    async fn a_repause_keeps_the_original_stamp() {
        let d = team_of_two().await;
        d.set_member_state("acme", "bob@x.io", MemberState::Paused, "alice@x.io").await.unwrap();
        let first = member(&d.get("acme").await.unwrap().unwrap(), "bob@x.io");
        assert_eq!(d.set_member_state("acme", "bob@x.io", MemberState::Paused, "carol@x.io").await.unwrap(), Membership::Done);
        let again = member(&d.get("acme").await.unwrap().unwrap(), "bob@x.io");
        assert_eq!((again.paused_at, again.paused_by), (first.paused_at, first.paused_by));
    }
}
