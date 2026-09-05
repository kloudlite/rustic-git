//! Stage 14 · Experience: every remaining verb a person can perform, walked once an hour.
//!
//! A SCAFFOLD. The catalogue, the windows, the CronJob and the teardown sweeps are in place; the
//! steps themselves are not, and every id skips with "not implemented yet" until they are. That is
//! deliberate rather than an empty stage: a run is exactly-once complete — every id in the suite
//! reports on every path — so the console renders the stage as grey rather than as a run that
//! silently reported 77 of 105 ids, and `SloProbeMissing` stays honest for the fast ids the hourly
//! run does produce.
//!
//! `IDS` is the order the stage will run in, which is the addendum's own table order: identity and
//! packages, then teams (create → invite → role → shared repo → workspace → remove → delete), then
//! the repo and PR verbs, then environments, then the volume/quota/admin reads, and the two
//! whole-journey observations (`feed.experience`, `home.persists`) last, because both assert
//! something about what everything BEFORE them did.
//!
//! ponytail: skips only, no probe code — the second implementer fills `run` in id order and
//! deletes this note when the last skip is gone.

use crate::ctx::Ctx;

/// The catalogue's stage name, verbatim.
pub const EXPERIENCE: &str = "14 · Experience";

/// Every id this stage owns, in the order it will probe them.
pub const IDS: &[&str] = &[
    "ws.packages.add",
    "ws.packages.remove",
    "ws.seeded",
    "key.platform.regenerate",
    "team.create",
    "team.invite.accept",
    "team.role.set",
    "team.repo.shared",
    "team.workspace",
    "team.member.remove",
    "team.delete",
    "repo.protection",
    "repo.commit.patch",
    "repo.compare",
    "pr.comment",
    "pr.close",
    "commit.verify",
    "env.services.multi",
    "env.clone",
    "env.restore.inplace",
    "env.stop.start",
    "vol.history",
    "quota.view",
    "request.approve",
    "admin.stop.workspace",
    "superadmin.grant",
    "feed.experience",
    "home.persists",
];

/// One arm per id, walked in `IDS` order. An id whose arm is still `_` skips, which is what keeps
/// the run exactly-once complete while the stage is half-filled — and what lets four implementers
/// land their own ids without editing each other's lines.
pub async fn run(c: &mut Ctx) {
    for id in IDS {
        match *id {
            // Owned by `experience_ws`. `ws.packages.add` creates the workspace the pair runs
            // against and `ws.packages.remove` follows it, so one call covers both ids.
            "ws.packages.add" => super::experience_ws::packages(c).await,
            "ws.packages.remove" => {}
            "ws.seeded" => super::experience_ws::seeded(c).await,
            "key.platform.regenerate" => super::experience_ws::platform_key(c).await,
            "home.persists" => super::experience_ws::home_persists(c).await,
            "team.create" => super::experience_teams::create(c).await,
            "team.invite.accept" => super::experience_teams::invite_accept(c).await,
            "team.role.set" => super::experience_teams::role_set(c).await,
            "team.repo.shared" => super::experience_teams::repo_shared(c).await,
            "team.workspace" => super::experience_teams::workspace(c).await,
            "team.member.remove" => super::experience_teams::member_remove(c).await,
            "team.delete" => super::experience_teams::delete(c).await,
            "repo.protection" => super::experience_teams::protection(c).await,
            "repo.commit.patch" => super::experience_teams::commit_patch(c).await,
            "repo.compare" => super::experience_teams::compare(c).await,
            "pr.comment" => super::experience_teams::comment(c).await,
            "pr.close" => super::experience_teams::close(c).await,
            "commit.verify" => super::experience_teams::verify(c).await,
            _ => c.skip(id, "not implemented yet"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kloudlite_workspaces::slo::catalogue::{journey, Suite};

    /// The stage reports exactly the ids the catalogue says it owns — no more, no fewer, and none
    /// twice. The whole point of the scaffold is that the run stays complete while it is empty.
    #[test]
    fn ids_are_the_catalogues_experience_stage() {
        let (_, catalogued) = journey(Suite::Hourly)
            .into_iter()
            .find(|(name, _)| *name == EXPERIENCE)
            .expect("the hourly journey walks Experience");
        let mut mine = IDS.to_vec();
        mine.sort_unstable();
        let mut theirs = catalogued;
        theirs.sort_unstable();
        assert_eq!(mine, theirs);
        mine.dedup();
        assert_eq!(mine.len(), IDS.len(), "an id is listed twice");
    }
}
