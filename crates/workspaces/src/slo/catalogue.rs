//! Every SLO the probe judges, plus `deploy/slo.md`'s human twin held equal to it by
//! `the_catalogue_matches_deploy_slo_md` — the same pattern as `history::alerts` and
//! `deploy/alerts.md`, and for the same reason: a catalogue that can drift from the doc a human
//! reads is a catalogue nobody trusts.
//!
//! A latency SLO is "good" when the step succeeded AND took at most `max_ms` — that is the only
//! shape (see the design's "The catalogue"), so `Target` carries both in one place rather than as
//! two fields the reader has to reconcile.

/// Which run produces samples for an SLO, and the rolling window its attainment is computed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suite {
    Fast,
    Weekly,
    Monthly,
}

impl Suite {
    /// The CronJob schedule this suite's samples arrive on — every 5 min / weekly / monthly,
    /// which is also the window `SloProbeMissing` and the burn-rate maths use per suite.
    pub fn period_secs(&self) -> u64 {
        match self {
            Suite::Fast => 300,
            Suite::Weekly => 604_800,
            Suite::Monthly => 2_592_000,
        }
    }

    /// The inverse of `as_str`. One parser for the CLI flag, the teardown sweep's name check and
    /// anything else that reads a suite back out of a string — a second `match` somewhere would
    /// be a second place a renamed variant has to be remembered.
    pub fn parse(s: &str) -> Option<Suite> {
        match s {
            "fast" => Some(Suite::Fast),
            "weekly" => Some(Suite::Weekly),
            "monthly" => Some(Suite::Monthly),
            _ => None,
        }
    }

    /// The catalogue's own "Suite" column, verbatim — never derive it from `Debug`, which would
    /// silently rename the column the moment someone reorders the variants.
    pub fn as_str(&self) -> &'static str {
        match self {
            Suite::Fast => "fast",
            Suite::Weekly => "weekly",
            Suite::Monthly => "monthly",
        }
    }
}

/// `good_pct` over 30 days, and — for a latency SLO — the ceiling a "good" sample must also meet.
/// `max_ms: None` means the SLO is pass/fail only (an availability check, a refusal, a security
/// invariant): there is nothing else for it to be "good" at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Target {
    pub good_pct: f64,
    pub max_ms: Option<u32>,
}

impl Target {
    /// `"99.9 %"` or `"95 % ≤ 2000 ms"` — the exact text `deploy/slo.md`'s Target column carries,
    /// so the two can be compared string-for-string instead of re-parsing the doc's number format.
    pub fn render(&self) -> String {
        match self.max_ms {
            Some(ms) => format!("{} % ≤ {ms} ms", format_pct(self.good_pct)),
            None => format!("{} %", format_pct(self.good_pct)),
        }
    }
}

/// Trims `format!("{p:.2}")` down to the shortest form that round-trips — `100.00` -> `100`,
/// `99.90` -> `99.9`, `99.99` stays `99.99` — because the catalogue only ever uses a handful of
/// percentages and the doc should read the way a person would type it, not with padding zeros.
fn format_pct(p: f64) -> String {
    let s = format!("{p:.2}");
    let s = s.trim_end_matches('0');
    s.trim_end_matches('.').to_string()
}

pub struct Slo {
    /// Stable, the ClickHouse key: `"git.push.ok"`, `"ws.create.p95"`.
    pub id: &'static str,
    pub feature: &'static str,
    /// The catalogue's SLI column, verbatim.
    pub sli: &'static str,
    pub target: Target,
    pub suite: Suite,
    /// The journey stage this SLO is probed in — `deploy/slo.md`'s "Journey step" column,
    /// verbatim, and the value the probe stamps on every step it records. One of `STAGES`.
    pub stage: &'static str,
}

/// The journey, in order. Boot and Teardown carry no SLO of their own and are still stages: the
/// console renders the walk a run makes, and a stage that vanishes when it has no ids would make
/// a run that died at boot look like a run that never started.
pub const STAGES: &[&str] = &[
    "0 · Boot",
    "1 · Identity",
    "2 · Git",
    "3 · Pull request",
    "4 · Registry",
    "5 · Workspace",
    "6 · Environment",
    "7 · Lifecycle",
    "8 · Admin",
    "9 · Security",
    "10 · Edge",
    "11 · Teardown",
    "12 · Weekly",
    "13 · Monthly",
];

/// Every stage `suite` walks, with the ids it probes. Weekly is the fast journey plus its own
/// stage and monthly is weekly plus its own — never a different journey, the same rule
/// `suite::fast()` is built on.
pub fn journey(suite: Suite) -> Vec<(&'static str, Vec<&'static str>)> {
    let last = match suite {
        Suite::Fast => "11 · Teardown",
        Suite::Weekly => "12 · Weekly",
        Suite::Monthly => "13 · Monthly",
    };
    let upto = STAGES.iter().position(|s| *s == last).map(|i| i + 1).unwrap_or(STAGES.len());
    STAGES[..upto]
        .iter()
        .map(|name| (*name, CATALOGUE.iter().filter(|s| s.stage == *name).map(|s| s.id).collect()))
        .collect()
}

/// The SLO with this id, or `None` — the admin API's `PUT /admin/slo/runs/{id}` checks every
/// reported step's `slo_id` against this before it reaches a query, exactly as `slo_id` is checked
/// in the design's Admin API section.
pub fn find(id: &str) -> Option<&'static Slo> {
    CATALOGUE.iter().find(|s| s.id == id)
}

const fn avail(pct: f64) -> Target {
    Target { good_pct: pct, max_ms: None }
}

const fn p95(ms: u32) -> Target {
    Target { good_pct: 95.0, max_ms: Some(ms) }
}

const fn bound(ms: u32) -> Target {
    Target { good_pct: 99.9, max_ms: Some(ms) }
}

/// Every id the fast/weekly/monthly stages probe (design's "Stages" paragraph), feature and SLI
/// text following the artifact's wording, targets per the brief: 99.9 % is the default
/// availability target; the exceptions and the `*.p95`/bound millisecond ceilings are called out
/// inline so a reviewer can check one against the design without cross-referencing a table.
pub const CATALOGUE: &[Slo] = &[
    // Stage 1 · identity
    Slo { id: "id.signin", feature: "Identity", sli: "Sign-in over HTTP succeeds", target: avail(99.9), suite: Suite::Fast, stage: "1 · Identity" },
    Slo { id: "id.token.mint", feature: "Identity", sli: "Minting a user JWT succeeds", target: avail(99.9), suite: Suite::Fast, stage: "1 · Identity" },
    Slo { id: "id.key.usable", feature: "Identity", sli: "A freshly minted platform SSH key is usable", target: bound(30_000), suite: Suite::Fast, stage: "1 · Identity" },
    Slo { id: "id.cli.flow", feature: "Identity", sli: "The kl CLI's login-to-command flow completes", target: bound(15_000), suite: Suite::Fast, stage: "1 · Identity" },
    Slo { id: "id.jwt.tiers", feature: "Identity", sli: "A JWT is honoured across every tier", target: avail(99.9), suite: Suite::Fast, stage: "1 · Identity" },

    // Stage 2 · git
    Slo { id: "git.push.ok", feature: "Git hosting", sli: "Push of one commit over HTTP succeeds", target: avail(99.9), suite: Suite::Fast, stage: "2 · Git" },
    Slo { id: "git.push.p95", feature: "Git hosting", sli: "Push of one commit over HTTP completes", target: p95(3_000), suite: Suite::Fast, stage: "2 · Git" },
    Slo { id: "git.clone.ok", feature: "Git hosting", sli: "Clone over HTTP succeeds", target: avail(99.9), suite: Suite::Fast, stage: "2 · Git" },
    Slo { id: "git.clone.p95", feature: "Git hosting", sli: "Clone over HTTP completes", target: p95(2_000), suite: Suite::Fast, stage: "2 · Git" },
    Slo { id: "ssh.clone.ok", feature: "Git hosting", sli: "Clone over SSH succeeds", target: avail(99.9), suite: Suite::Fast, stage: "2 · Git" },
    Slo { id: "ssh.hostkey", feature: "Git hosting", sli: "The SSH host key served matches the pinned fingerprint", target: avail(99.9), suite: Suite::Fast, stage: "2 · Git" },
    Slo { id: "ssh.unregistered.refused", feature: "Git hosting", sli: "SSH from an unregistered key is refused", target: avail(99.9), suite: Suite::Fast, stage: "2 · Git" },
    Slo { id: "browse.p95", feature: "Git hosting", sli: "The Browse API renders a repo page", target: p95(500), suite: Suite::Fast, stage: "2 · Git" },
    Slo { id: "browse.commit.visible", feature: "Git hosting", sli: "A pushed commit becomes visible in Browse", target: bound(5_000), suite: Suite::Fast, stage: "2 · Git" },
    Slo { id: "web.repo.page", feature: "Git hosting", sli: "The web app's repo page loads", target: p95(1_500), suite: Suite::Fast, stage: "2 · Git" },

    // Stage 3 · pull request
    Slo { id: "pr.merge.p95", feature: "Pull requests", sli: "A pull request merge completes", target: p95(60_000), suite: Suite::Fast, stage: "3 · Pull request" },
    Slo { id: "feed.latency", feature: "Pull requests", sli: "A PR event reaches the activity feed", target: bound(30_000), suite: Suite::Fast, stage: "3 · Pull request" },

    // Stage 4 · registry
    Slo { id: "reg.token.p95", feature: "Container registry", sli: "Minting a registry bearer token completes", target: p95(300), suite: Suite::Fast, stage: "4 · Registry" },
    Slo { id: "reg.push.ok", feature: "Container registry", sli: "Pushing an image succeeds", target: avail(99.9), suite: Suite::Fast, stage: "4 · Registry" },
    Slo { id: "reg.manifest.p95", feature: "Container registry", sli: "Fetching a manifest completes", target: p95(500), suite: Suite::Fast, stage: "4 · Registry" },
    Slo { id: "reg.tags.visible", feature: "Container registry", sli: "A pushed tag becomes visible in the tag list", target: bound(5_000), suite: Suite::Fast, stage: "4 · Registry" },
    Slo { id: "reg.shared.layer", feature: "Container registry", sli: "A shared layer is not re-uploaded by a sibling image", target: avail(99.9), suite: Suite::Fast, stage: "4 · Registry" },
    Slo { id: "reg.canary", feature: "Container registry", sli: "The registry canary image pulls successfully", target: avail(99.9), suite: Suite::Fast, stage: "4 · Registry" },
    Slo { id: "reg.visibility", feature: "Container registry", sli: "Image visibility (public vs. private) is enforced", target: avail(99.9), suite: Suite::Fast, stage: "4 · Registry" },

    // Stage 5 · workspace
    Slo { id: "ws.create.p95", feature: "Workspaces", sli: "Creating a workspace completes", target: p95(90_000), suite: Suite::Fast, stage: "5 · Workspace" },
    Slo { id: "ws.exec.ok", feature: "Workspaces", sli: "Exec into a running workspace pod succeeds", target: avail(99.9), suite: Suite::Fast, stage: "5 · Workspace" },
    Slo { id: "homes.rw.p95", feature: "Workspaces", sli: "A read/write round trip on the shared home completes", target: p95(200), suite: Suite::Fast, stage: "5 · Workspace" },
    Slo { id: "gw.tunnel.p95", feature: "Workspaces", sli: "Opening a gateway SSH tunnel completes", target: p95(3_000), suite: Suite::Fast, stage: "5 · Workspace" },
    Slo { id: "gw.unregistered.refused", feature: "Workspaces", sli: "The gateway refuses an unregistered key", target: avail(99.9), suite: Suite::Fast, stage: "5 · Workspace" },
    Slo { id: "ws.push.p95", feature: "Workspaces", sli: "Pushing a workspace snapshot completes", target: p95(60_000), suite: Suite::Fast, stage: "5 · Workspace" },
    Slo { id: "ws.clone.p95", feature: "Workspaces", sli: "Cloning a workspace completes", target: p95(60_000), suite: Suite::Fast, stage: "5 · Workspace" },
    Slo { id: "quota.refused", feature: "Workspaces", sli: "An over-quota create is refused with 409", target: avail(99.9), suite: Suite::Fast, stage: "5 · Workspace" },

    // Stage 6 · environment
    Slo { id: "env.create.p95", feature: "Environments", sli: "Creating an environment completes", target: p95(120_000), suite: Suite::Fast, stage: "6 · Environment" },
    Slo { id: "env.dns", feature: "Environments", sli: "Service-to-service DNS resolves inside an environment's namespace", target: avail(99.9), suite: Suite::Fast, stage: "6 · Environment" },
    Slo { id: "env.attach", feature: "Environments", sli: "Attaching a workspace to an environment takes effect", target: bound(10_000), suite: Suite::Fast, stage: "6 · Environment" },
    Slo { id: "env.detach", feature: "Environments", sli: "Detaching a workspace from an environment takes effect", target: bound(10_000), suite: Suite::Fast, stage: "6 · Environment" },
    Slo { id: "env.push.p95", feature: "Environments", sli: "Pushing an environment snapshot completes", target: p95(90_000), suite: Suite::Fast, stage: "6 · Environment" },

    // Stage 7 · lifecycle
    Slo { id: "ws.stop.p95", feature: "Workspace lifecycle", sli: "Stopping a workspace completes", target: p95(15_000), suite: Suite::Fast, stage: "7 · Lifecycle" },
    Slo { id: "ws.replicated", feature: "Workspace lifecycle", sli: "A stopped workspace's final sync point reaches a replica", target: bound(300_000), suite: Suite::Fast, stage: "7 · Lifecycle" },
    Slo { id: "ws.start.p95", feature: "Workspace lifecycle", sli: "Starting a workspace completes", target: p95(30_000), suite: Suite::Fast, stage: "7 · Lifecycle" },
    Slo { id: "ws.restore", feature: "Workspace lifecycle", sli: "Restoring a workspace from a past snapshot succeeds", target: avail(99.9), suite: Suite::Fast, stage: "7 · Lifecycle" },
    Slo { id: "vol.refusals", feature: "Workspace lifecycle", sli: "Deleting a sync point or a running worktree's base snapshot is refused", target: avail(99.9), suite: Suite::Fast, stage: "7 · Lifecycle" },
    Slo { id: "vol.detached.restorable", feature: "Workspace lifecycle", sli: "A detached volume's snapshot can still be restored", target: avail(99.9), suite: Suite::Fast, stage: "7 · Lifecycle" },
    Slo { id: "vol.orphan.collected", feature: "Workspace lifecycle", sli: "An orphaned volume directory is collected", target: bound(300_000), suite: Suite::Fast, stage: "7 · Lifecycle" },

    // Stage 8 · admin
    Slo { id: "req.queue", feature: "Admin", sli: "A Request CR is queued and answerable by an admin", target: bound(5_000), suite: Suite::Fast, stage: "8 · Admin" },
    Slo { id: "audit.row", feature: "Admin", sli: "Every admin write produces an audit row", target: avail(99.9), suite: Suite::Fast, stage: "8 · Admin" },
    Slo { id: "signals.fresh", feature: "Admin", sli: "The Signals table reflects a rule transition", target: bound(120_000), suite: Suite::Fast, stage: "8 · Admin" },
    Slo { id: "history.api", feature: "Admin", sli: "The history API answers a chart query", target: avail(99.9), suite: Suite::Fast, stage: "8 · Admin" },

    // Stage 9 · security
    Slo { id: "sec.private.repo", feature: "Security", sli: "A private repo is unreadable to a non-collaborator", target: avail(100.0), suite: Suite::Fast, stage: "9 · Security" },
    Slo { id: "sec.cross.owner", feature: "Security", sli: "One owner's objects are invisible to another owner", target: avail(100.0), suite: Suite::Fast, stage: "9 · Security" },
    Slo { id: "sec.admin.claim", feature: "Security", sli: "An admin route refuses a token without the superadmin claim", target: avail(100.0), suite: Suite::Fast, stage: "9 · Security" },
    Slo { id: "sec.user.process", feature: "Security", sli: "The ordinary API process has no admin route mounted", target: avail(100.0), suite: Suite::Fast, stage: "9 · Security" },
    Slo { id: "sec.agent.spec", feature: "Security", sli: "The admission policy refuses a spec write outside the allowed fields", target: avail(100.0), suite: Suite::Fast, stage: "9 · Security" },
    Slo { id: "id.token.revoked", feature: "Security", sli: "A revoked token is refused", target: avail(99.9), suite: Suite::Fast, stage: "9 · Security" },

    // Stage 10 · edge and pipeline
    Slo { id: "edge.dns", feature: "Edge and pipeline", sli: "The public hostname resolves", target: avail(99.99), suite: Suite::Fast, stage: "10 · Edge" },
    Slo { id: "edge.cert", feature: "Edge and pipeline", sli: "The TLS certificate is valid for the public hostname", target: avail(99.9), suite: Suite::Fast, stage: "10 · Edge" },
    Slo { id: "edge.origin", feature: "Edge and pipeline", sli: "Cloudflare reaches the origin", target: avail(99.9), suite: Suite::Fast, stage: "10 · Edge" },
    Slo { id: "edge.ssh.lb", feature: "Edge and pipeline", sli: "The SSH load balancer accepts a connection", target: avail(99.9), suite: Suite::Fast, stage: "10 · Edge" },
    Slo { id: "tel.log.latency", feature: "Edge and pipeline", sli: "A structured log line reaches HyperDX", target: bound(60_000), suite: Suite::Fast, stage: "10 · Edge" },
    Slo { id: "tel.pod.coverage", feature: "Edge and pipeline", sli: "Every pod is scraped by the region's collector", target: bound(60_000), suite: Suite::Fast, stage: "10 · Edge" },
    Slo { id: "tel.stream.lag", feature: "Edge and pipeline", sli: "The Redis events stream consumer lag stays low", target: bound(60_000), suite: Suite::Fast, stage: "10 · Edge" },
    Slo { id: "tel.ch.disk", feature: "Edge and pipeline", sli: "ClickHouse disk usage is reported", target: bound(60_000), suite: Suite::Fast, stage: "10 · Edge" },

    // Weekly
    Slo { id: "git.push.large", feature: "Git hosting", sli: "Push of a large commit over HTTP succeeds", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "reg.push.large", feature: "Container registry", sli: "Pushing a large image layer succeeds", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "ws.cold.profile", feature: "Workspaces", sli: "A cold package profile builds successfully", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "ws.profile.reuse", feature: "Workspaces", sli: "A repeat package set is published from the profile index, not rebuilt", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "ws.cross.node", feature: "Workspaces", sli: "A workspace started on a peer node reads its replica correctly", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "homes.cross.node", feature: "Workspaces", sli: "The shared home is consistent across nodes", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "cp.failover", feature: "Control plane", sli: "The leader lease fails over to another pod", target: bound(30_000), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "settings.live", feature: "Control plane", sli: "A live settings change takes effect on the next beat", target: bound(60_000), suite: Suite::Weekly, stage: "12 · Weekly" },

    // Monthly
    Slo { id: "bak.tarball.age", feature: "Backups", sli: "The latest backup tarball is recent", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    Slo { id: "bak.daily.slots", feature: "Backups", sli: "Every daily backup slot is present", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    Slo { id: "bak.versioning", feature: "Backups", sli: "Backup versioning is enabled and retains history", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    Slo { id: "bak.cosmos", feature: "Backups", sli: "The Cosmos backup for HyperDX succeeds", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    Slo { id: "drill.dead.node", feature: "Resilience drills", sli: "A dead-node drill heals every replica onto a live node", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    Slo { id: "drill.drain", feature: "Resilience drills", sli: "A drain drill succeeds without interrupting a running worktree", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    Slo { id: "drill.redis.down", feature: "Resilience drills", sli: "The system keeps operating correctly with Redis down", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogue_matches_deploy_slo_md() {
        let md = include_str!("../../../../deploy/slo.md");
        let rows: Vec<Vec<String>> = md
            .lines()
            .filter(|l| l.starts_with("| ") && !l.starts_with("| id") && !l.starts_with("| ---"))
            .map(|l| l.trim_matches('|').split('|').map(|c| c.trim().trim_matches('`').to_string()).collect())
            .collect();
        let probed: Vec<&Vec<String>> = rows.iter().filter(|r| r[4] != "manual").collect();
        assert_eq!(probed.len(), CATALOGUE.len(), "row count");
        for r in probed {
            let s = find(&r[0]).unwrap_or_else(|| panic!("{} missing from CATALOGUE", r[0]));
            assert_eq!(s.feature, r[1]);
            assert_eq!(s.sli, r[2]);
            assert_eq!(s.target.render(), r[3]);
            assert_eq!(s.suite.as_str(), r[4]);
            assert_eq!(s.stage, r[5]);
        }
    }

    /// The journey is the console's spine and the probe's own stage list: every stage present in
    /// order even when it probes nothing, each suite a superset of the one before, and the ids a
    /// partition of the catalogue rather than a second hand-kept list that can drift from it.
    #[test]
    fn the_journey_covers_every_stage_and_partitions_the_catalogue() {
        let names = |j: &[(&'static str, Vec<&'static str>)]| -> Vec<&'static str> {
            j.iter().map(|(n, _)| *n).collect()
        };
        let ids = |j: &[(&'static str, Vec<&'static str>)]| -> Vec<&'static str> {
            j.iter().flat_map(|(_, ids)| ids.clone()).collect()
        };
        let (fast, weekly, monthly) =
            (journey(Suite::Fast), journey(Suite::Weekly), journey(Suite::Monthly));
        assert_eq!(names(&monthly), STAGES);
        assert_eq!(names(&fast), &STAGES[..STAGES.len() - 2]);
        assert!(names(&weekly).starts_with(&names(&fast)));
        assert!(names(&monthly).starts_with(&names(&weekly)));
        // Boot and Teardown probe nothing and are still there.
        assert!(fast.iter().any(|(n, ids)| *n == "0 · Boot" && ids.is_empty()));
        assert!(fast.iter().any(|(n, ids)| *n == "11 · Teardown" && ids.is_empty()));
        // Every id exactly once, and every stage a catalogue row names is one of `STAGES`.
        let mut all = ids(&monthly);
        all.sort_unstable();
        let mut want: Vec<&str> = CATALOGUE.iter().map(|s| s.id).collect();
        want.sort_unstable();
        assert_eq!(all, want);
        for s in CATALOGUE {
            assert!(STAGES.contains(&s.stage), "{} names stage {:?}", s.id, s.stage);
        }
        assert_eq!(ids(&fast), CATALOGUE.iter().filter(|s| s.suite == Suite::Fast).map(|s| s.id).collect::<Vec<_>>());
    }

    #[test]
    fn ids_are_unique_and_shaped() {
        let mut seen = std::collections::HashSet::new();
        for s in CATALOGUE {
            assert!(seen.insert(s.id), "{} twice", s.id);
            assert!(s.id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.'), "{}", s.id);
            assert!(s.target.good_pct > 0.0 && s.target.good_pct <= 100.0);
        }
    }
}
