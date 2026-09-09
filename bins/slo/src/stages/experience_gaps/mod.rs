//! Stage 14's remaining verbs: the twenty ids the 2026-09-05 coverage review found unprobed.
//!
//! A sibling of `experience.rs` like `experience_ws`, `experience_teams`, `experience_env` and
//! `experience_admin` — one file per group of ids, because the stage is one catalogue and several
//! hands. This one is the batch that batch review named, kept together rather than scattered
//! across the four existing files so a reader can hold the review and the code side by side.
//!
//! Three of the review's proposals are implemented differently from its wording, and each is a
//! finding about the code rather than a shortcut:
//!
//! 1. **`id.username` cannot claim a free handle.** `claim_username` RENAMES the caller
//!    (`crates/api/src/teams.rs`), and the probe's tenant already has one — a step that claimed a
//!    fresh handle would rename `slo-probe` and break every later run. So the id measures the two
//!    refusals the route owes: a taken handle is a 409 and a malformed one a 400.
//! 2. **`id.profile.upsert` cannot upsert.** `POST /v1/users` is peer-only BECAUSE it mints a
//!    session, and the probe holds no peer secret. The invariant worth an SLO is exactly that: a
//!    session token must not be able to renew itself for as long as its holder likes.
//! 3. **`req.decide.kinds` brings its own team.** An access approval grants the ASKER membership
//!    of `access.team`, and `team.delete` has already taken the stage's own team by the time this
//!    runs — so this makes and takes back a team of its own rather than depending on the order.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;
use serde_json::{json, Value};

use super::git::BASE_BRANCH;
use super::{admin, api, call, get, poll_json, post, raw};
use crate::ctx::Ctx;
use crate::drill::{undoing, UNDO_SLACK};
use crate::tools;
use super::{clip, id_of};

mod identity;
pub(crate) use identity::*;
mod pulls;
pub(crate) use pulls::*;
mod teams;
pub(crate) use teams::*;
mod console;
pub(crate) use console::*;


/// Per-step ceilings, each at or above its catalogue target for the reason every other stage
/// states: a slow answer must be a breach with a number, never a step the probe cut off.
pub(super) const QUICK: Duration = Duration::from_secs(20);

pub(super) const READ_CEILING: Duration = Duration::from_secs(15);

pub(super) const KEY_BODY: Duration = Duration::from_secs(60);

pub(super) const KEY_CEILING: Duration = Duration::from_secs(KEY_BODY.as_secs() + UNDO_SLACK);

pub(super) const MERGE_CEILING: Duration = Duration::from_secs(300);

pub(super) const MERGEABILITY_CEILING: Duration = Duration::from_secs(60);

pub(super) const TEAM_ENV_CEILING: Duration = Duration::from_secs(240);

pub(super) const ATTACH_PAIR_CEILING: Duration = Duration::from_secs(120);

pub(super) const ADMIN_ENV_CEILING: Duration = Duration::from_secs(180);

pub(super) const ADMIN_DELETE_CEILING: Duration = Duration::from_secs(180);

pub(super) const DECIDE_CEILING: Duration = Duration::from_secs(90);
pub(super) const SSHCONFIG_CEILING: Duration = Duration::from_secs(45);


/// Every admin write carries a note onto its audit row; an empty one is a 422.
pub(super) const NOTE: &str = "slo probe";


pub(super) const QUOTA_GB: u64 = 1;

// ── identity ────────────────────────────────────────────────────────────────


/// `kl`'s own subcommand name (`WsCmd::SshConfig`, which clap spells with the dash). Named here so
/// the one string the CLI contract turns on is next to the test that pins it.
pub(super) const KL_SSH_CONFIG: &str = "ssh-config";


/// All a removed key gets: one api round trip. Hits are not cached, so the very next
/// authentication reads the store and finds nothing — anything past this is a credential the
/// fleet forgot to forget.
pub(super) const REVOCATION_WINDOW: Duration = Duration::from_secs(10);


/// The four `merge_worker.rs` implements, in the order the web offers them.
pub(super) const STRATEGIES: [&str; 4] = ["merge", "squash", "rebase", "fast-forward"];


/// How long one strategy's tree has to appear on `main` after its change reports `merged`.
pub(super) const MERGE_LAND: Duration = Duration::from_secs(30);


/// How long `redis` has to answer once its StatefulSet reports a ready replica.
pub(super) const SVC_ANSWERS: Duration = Duration::from_secs(30);


/// One path segment, with the characters that would SPLIT it escaped.
///
/// An ssh credential's id is its `SHA256:<base64>` fingerprint, and base64 contains `/` — so
/// `/v1/keys/{id}` built by interpolation is three segments, matches no route, and falls through
/// to the GET-only fallback as a 405. See the report: the same shape is why teardown's own key
/// sweep has been failing silently.
pub(super) fn path_seg(id: &str) -> String {
    id.chars()
        .map(|ch| match ch {
            '/' => "%2F".to_string(),
            '+' => "%2B".to_string(),
            '%' => "%25".to_string(),
            other => other.to_string(),
        })
        .collect()
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;

    /// The one judgement `id.cli.sshconfig` turns on. `render` SKIPS a workspace whose name it
    /// will not put in a config and still exits zero, so a file with a header and no host block is
    /// exactly what an exit-code check would call a pass.
    #[test]
    fn the_ssh_config_check_wants_a_real_host_block() {
        let good = "# Managed by kl-connect.\n\nHost run-fast-1\n  HostName ws-abc\n  User kl\n  \
                    ProxyCommand kl-connect ws proxy ws-abc\n  HostKeyAlias ws-abc\n";
        assert!(has_host_block(good, "ws-abc").is_ok());
        // A file the command wrote having skipped every workspace.
        assert!(has_host_block("# Managed by kl-connect.\n", "ws-abc").is_err());
        // Another workspace's block is not this one's.
        assert!(has_host_block(good, "ws-other").is_err());
        // A block with no way to reach the pod is a block ssh cannot use.
        let no_proxy = "Host x\n  HostName ws-abc\n  User kl\n";
        assert!(has_host_block(no_proxy, "ws-abc").is_err());
    }

    /// A denied request that carries no reason is a decision the asker cannot act on — the half a
    /// check on `state` alone would miss.
    #[test]
    fn a_deny_has_to_carry_its_reason() {
        let note = "slo probe denied fast-1";
        assert!(denied_with(&json!({ "state": "denied", "note": note }), note).is_ok());
        assert!(denied_with(&json!({ "state": "denied", "decision": { "note": note } }), note).is_ok());
        assert!(denied_with(&json!({ "state": "denied" }), note).is_err());
        assert!(denied_with(&json!({ "state": "approved", "note": note }), note).is_err());
        assert!(denied_with(&json!({ "state": "pending", "note": note }), note).is_err());
    }

    /// Every id this file owns reports exactly once against a fleet that answers nothing — as a
    /// failure with a reason, or as a skip when its precondition is genuinely absent. A run is
    /// exactly-once complete on every path, which is what lets the console tell a grey stage from
    /// a broken one.
    #[tokio::test]
    async fn every_id_reports_once_with_nothing_reachable() {
        let mut c = testkit::ctx().await;
        c.kube = None;
        username(&mut c).await;
        profile_upsert(&mut c).await;
        cli_tokens(&mut c).await;
        sshconfig(&mut c).await;
        key_lifecycle(&mut c).await;
        description(&mut c).await;
        merge_strategies(&mut c).await;
        mergeability(&mut c).await;
        invite_revoke(&mut c).await;
        team_environment(&mut c).await;
        attach_pair(&mut c).await;
        vol_list(&mut c).await;
        admin_stop_environment(&mut c).await;
        admin_delete(&mut c).await;
        screens(&mut c).await;
        workloads(&mut c).await;
        audit_export(&mut c).await;
        decide_kinds(&mut c).await;
        legacy_union(&mut c).await;
        region_status(&mut c).await;
        for id in IDS {
            assert_eq!(c.steps.iter().filter(|s| s.slo_id == id).count(), 1, "{id}");
        }
        assert_eq!(c.steps.len(), IDS.len(), "an id nobody asked for was reported");
        // Nothing anywhere carries a credential: these steps mint CLI tokens and read invitations.
        for s in &c.steps {
            assert!(!s.detail.contains(&c.probe_jwt), "a jwt reached a detail: {s:?}");
        }
    }

    /// Every id this file owns, which is also the set `experience.rs` dispatches to it.
    const IDS: [&str; 20] = [
        "id.username",
        "id.profile.upsert",
        "id.cli.tokens",
        "id.cli.sshconfig",
        "key.ssh.lifecycle",
        "repo.description",
        "pr.merge.strategies",
        "pr.mergeability",
        "team.invite.revoke",
        "team.environment",
        "env.attach.pair",
        "vol.list",
        "admin.stop.environment",
        "admin.delete.workload",
        "admin.screens",
        "admin.workloads.read",
        "audit.export",
        "req.decide.kinds",
        "req.legacy.union",
        "region.status",
    ];
}
