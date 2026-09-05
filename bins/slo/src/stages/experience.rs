//! Stage 14 · Experience: every remaining verb a person can perform, walked once an hour.
//!
//! This file is the DISPATCH TABLE and nothing else: `IDS` is the order the stage reports in, and
//! `run` is one arm per id calling into a sibling module. The steps themselves live in
//! `experience_ws`, `experience_teams`, `experience_env` and `experience_admin` — one file per
//! group of ids, because the stage is one catalogue and several hands, and a stage file everyone
//! edits is a stage file nobody can merge.
//!
//! `IDS` is the addendum's own table order: identity and packages, then teams (create → invite →
//! role → shared repo → workspace → remove → delete), then the repo and PR verbs, then
//! environments, then the volume/quota/admin reads, and the two whole-journey observations
//! (`feed.experience`, `home.persists`) last, because both assert something about what everything
//! BEFORE them did.
//!
//! A few arms are empty (`{}`): several ids are one journey on one object — the packages pair, the
//! four environment ids — and the call that walks them reports all of them. The empty arm is what
//! keeps the id in the order, and `ids_are_the_catalogues_experience_stage` is what keeps the list
//! and the catalogue equal. Nothing here may report an id twice or skip one silently: a run is
//! exactly-once complete, which is what lets the console tell a grey stage from a broken one.

use super::experience_env;
use crate::ctx::Ctx;

/// The catalogue's stage name, verbatim.
pub const EXPERIENCE: &str = "14 · Experience";

/// Every id this stage owns, in the order it will probe them.
pub const IDS: &[&str] = &[
    "ws.packages.add",
    "ws.packages.remove",
    "ws.seeded",
    "key.platform.regenerate",
    "id.username",
    "id.profile.upsert",
    "id.cli.tokens",
    "id.cli.sshconfig",
    "key.ssh.lifecycle",
    "team.create",
    "team.invite.accept",
    "team.role.set",
    "team.repo.shared",
    "team.workspace",
    "team.member.remove",
    "team.delete",
    "team.invite.revoke",
    "team.environment",
    "repo.protection",
    "repo.commit.patch",
    "repo.compare",
    "repo.description",
    "pr.comment",
    "pr.close",
    "commit.verify",
    "pr.merge.strategies",
    "pr.mergeability",
    "env.services.multi",
    "env.clone",
    "env.restore.inplace",
    "env.stop.start",
    "env.attach.pair",
    "vol.history",
    "vol.list",
    "quota.view",
    "request.approve",
    "admin.stop.workspace",
    "admin.stop.environment",
    "admin.delete.workload",
    "admin.screens",
    "admin.workloads.read",
    "audit.export",
    "req.decide.kinds",
    "req.legacy.union",
    "region.status",
    "superadmin.grant",
    "feed.experience",
    "home.persists",
];

/// One arm per id, walked in `IDS` order. The `_` arm is what keeps a run exactly-once complete
/// while an id is being added: the catalogue names it before anyone has written its step, and a
/// skip with a reason is a truthful sample where a missing one is a hole nobody can see.
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
            // The four environment ids are one journey on one environment, walked here so the
            // chain (a clone of an environment that never came up is not a measurement) stays in
            // one place; they report in this same order.
            "env.services.multi" => experience_env::environments(c).await,
            "env.clone" | "env.restore.inplace" | "env.stop.start" => {}
            "vol.history" => experience_env::history(c).await,
            "quota.view" => experience_env::quota_view(c).await,
            "request.approve" => super::experience_admin::request_approve(c).await,
            "admin.stop.workspace" => super::experience_admin::admin_stop(c).await,
            "superadmin.grant" => super::experience_admin::superadmin_grant(c).await,
            "feed.experience" => super::experience_admin::feed(c).await,
            // The 2026-09-05 coverage review's batch, in `experience_gaps`.
            "id.username" => super::experience_gaps::username(c).await,
            "id.profile.upsert" => super::experience_gaps::profile_upsert(c).await,
            "id.cli.tokens" => super::experience_gaps::cli_tokens(c).await,
            "id.cli.sshconfig" => super::experience_gaps::sshconfig(c).await,
            "key.ssh.lifecycle" => super::experience_gaps::key_lifecycle(c).await,
            "repo.description" => super::experience_gaps::description(c).await,
            "pr.merge.strategies" => super::experience_gaps::merge_strategies(c).await,
            "pr.mergeability" => super::experience_gaps::mergeability(c).await,
            "team.invite.revoke" => super::experience_gaps::invite_revoke(c).await,
            "team.environment" => super::experience_gaps::team_environment(c).await,
            "env.attach.pair" => super::experience_gaps::attach_pair(c).await,
            "vol.list" => super::experience_gaps::vol_list(c).await,
            "admin.stop.environment" => super::experience_gaps::admin_stop_environment(c).await,
            "admin.delete.workload" => super::experience_gaps::admin_delete(c).await,
            "admin.screens" => super::experience_gaps::screens(c).await,
            "admin.workloads.read" => super::experience_gaps::workloads(c).await,
            "audit.export" => super::experience_gaps::audit_export(c).await,
            "req.decide.kinds" => super::experience_gaps::decide_kinds(c).await,
            "req.legacy.union" => super::experience_gaps::legacy_union(c).await,
            "region.status" => super::experience_gaps::region_status(c).await,
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
