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
    Hourly,
    Weekly,
    Monthly,
}

impl Suite {
    /// The CronJob schedule this suite's samples arrive on — every 5 min / weekly / monthly,
    /// which is also the window `SloProbeMissing` and the burn-rate maths use per suite.
    pub fn period_secs(&self) -> u64 {
        match self {
            Suite::Fast => 300,
            Suite::Hourly => 3_600,
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
            "hourly" => Some(Suite::Hourly),
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
            Suite::Hourly => "hourly",
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
    "14 · Experience",
];

/// Every stage `suite` walks, with the ids it probes. Weekly is the fast journey plus its own
/// stage and monthly is weekly plus its own — never a different journey, the same rule
/// `suite::fast()` is built on.
pub fn journey(suite: Suite) -> Vec<(&'static str, Vec<&'static str>)> {
    // Hourly is the ONE suite whose extra stage is not the next entry in `STAGES`: it is the fast
    // journey plus Experience, and it never walks the weekly or monthly stages. So the walk is a
    // prefix plus an explicit tail rather than a slice — appending Experience before Weekly to
    // keep the slice trick would have renumbered two stages that are already stored in ClickHouse.
    let (last, tail) = match suite {
        Suite::Fast => ("11 · Teardown", None),
        Suite::Hourly => ("11 · Teardown", Some("14 · Experience")),
        Suite::Weekly => ("12 · Weekly", None),
        Suite::Monthly => ("13 · Monthly", None),
    };
    let upto = STAGES.iter().position(|s| *s == last).map(|i| i + 1).unwrap_or(STAGES.len());
    STAGES[..upto]
        .iter()
        .copied()
        .chain(tail)
        .map(|name| (name, CATALOGUE.iter().filter(|s| s.stage == name).map(|s| s.id).collect()))
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
    // NOT "sign-in with a passkey succeeds": WebAuthn is verified in the web app, which holds the
    // relying-party identity and the challenge (`crates/api/src/passkeys.rs`) — this tier only
    // stores the credential and answers the lookup a sign-in makes. So the SLI is the half a
    // headless probe can honestly walk: the store round trip, and that the lookup stays peer-only.
    Slo { id: "id.signin.passkey", feature: "Identity", sli: "A passkey registers, lists back and its sign-in lookup is peer-only", target: avail(99.9), suite: Suite::Fast, stage: "1 · Identity" },

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
    Slo { id: "git.push.ssh", feature: "Git hosting", sli: "Push of one commit over SSH succeeds", target: avail(99.9), suite: Suite::Fast, stage: "2 · Git" },
    Slo { id: "repo.lifecycle", feature: "Git hosting", sli: "A repo is created, listed, deleted and its slug freed", target: bound(10_000), suite: Suite::Fast, stage: "2 · Git" },
    // The three page loads beside `web.repo.page`, in the same stage for the same reason: they are
    // the app's own front door, and the only thing that says the shell renders at all.
    Slo { id: "web.org.page", feature: "Git hosting", sli: "The web app's org page loads", target: p95(1_500), suite: Suite::Fast, stage: "2 · Git" },
    Slo { id: "web.repo.settings", feature: "Git hosting", sli: "The web app's repo settings page loads", target: p95(1_500), suite: Suite::Fast, stage: "2 · Git" },
    Slo { id: "web.workspaces.page", feature: "Workspaces", sli: "The web app's workspaces and environments pages load", target: p95(1_500), suite: Suite::Fast, stage: "2 · Git" },

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
    Slo { id: "reg.image.delete", feature: "Container registry", sli: "Deleting a tag removes it from the tag list and deleting an image removes it from the catalogue", target: bound(10_000), suite: Suite::Fast, stage: "4 · Registry" },
    // `_catalog` and `/api/{owner}/images` are the two any-node exceptions to the routing rule, so
    // a routing regression shows here before it shows anywhere a person would notice.
    Slo { id: "reg.catalogue", feature: "Container registry", sli: "The image catalogue lists a pushed image from any node", target: bound(5_000), suite: Suite::Fast, stage: "4 · Registry" },

    // Stage 5 · workspace
    Slo { id: "ws.create.p95", feature: "Workspaces", sli: "Creating a workspace completes", target: p95(90_000), suite: Suite::Fast, stage: "5 · Workspace" },
    // "…and its home is the shared export": a pod started before its node's NFS mount is up
    // hostPaths an empty local directory and strands the owner's dotfiles, which an exec that only
    // opened a channel — or only echoed — would pass straight through.
    Slo { id: "ws.exec.ok", feature: "Workspaces", sli: "Exec into a running workspace pod returns the command's output, from a pod whose home is the shared export", target: avail(99.9), suite: Suite::Fast, stage: "5 · Workspace" },
    Slo { id: "homes.rw.p95", feature: "Workspaces", sli: "A read/write round trip on the shared home completes", target: p95(200), suite: Suite::Fast, stage: "5 · Workspace" },
    Slo { id: "gw.tunnel.p95", feature: "Workspaces", sli: "Opening a gateway SSH tunnel completes", target: p95(3_000), suite: Suite::Fast, stage: "5 · Workspace" },
    Slo { id: "gw.unregistered.refused", feature: "Workspaces", sli: "The gateway refuses an unregistered key", target: avail(99.9), suite: Suite::Fast, stage: "5 · Workspace" },
    Slo { id: "ws.push.p95", feature: "Workspaces", sli: "Pushing a workspace snapshot completes", target: p95(60_000), suite: Suite::Fast, stage: "5 · Workspace" },
    Slo { id: "ws.clone.p95", feature: "Workspaces", sli: "Cloning a workspace completes", target: p95(60_000), suite: Suite::Fast, stage: "5 · Workspace" },
    // The sentence, not merely the status: `quota::refuse` answers `"{dimension}: {used} of
    // {limit} in use; request more under Quota"`, and a 409 naming the wrong dimension is a gate
    // that refused for a reason nobody asked about.
    Slo { id: "quota.refused", feature: "Workspaces", sli: "An over-quota create is refused with 409 naming the dimension, what is used and the limit", target: avail(99.9), suite: Suite::Fast, stage: "5 · Workspace" },
    // Create is only one of the four verbs behind `guard_alloc`; restore, clone and push route
    // through the same gate and none was probed.
    Slo { id: "env.quota.refused", feature: "Workspaces", sli: "An over-quota restore, clone and push are each refused with 409", target: avail(99.9), suite: Suite::Fast, stage: "5 · Workspace" },

    // Stage 6 · environment
    Slo { id: "env.create.p95", feature: "Environments", sli: "Creating an environment completes", target: p95(120_000), suite: Suite::Fast, stage: "6 · Environment" },
    Slo { id: "env.dns", feature: "Environments", sli: "A service in an environment resolves a sibling by bare name and connects to it", target: avail(99.9), suite: Suite::Fast, stage: "6 · Environment" },
    Slo { id: "env.attach", feature: "Environments", sli: "Attaching a workspace to an environment takes effect", target: bound(10_000), suite: Suite::Fast, stage: "6 · Environment" },
    Slo { id: "env.detach", feature: "Environments", sli: "Detaching a workspace from an environment takes effect", target: bound(10_000), suite: Suite::Fast, stage: "6 · Environment" },
    Slo { id: "env.push.p95", feature: "Environments", sli: "Pushing an environment snapshot completes", target: p95(90_000), suite: Suite::Fast, stage: "6 · Environment" },
    Slo { id: "env.exec.ok", feature: "Environments", sli: "Exec into a running service pod of the environment succeeds", target: avail(99.9), suite: Suite::Fast, stage: "6 · Environment" },
    // 120 s, not the workspace clone's 60: an environment copies LIVE bytes from the node that
    // holds it and then waits for every service's StatefulSet, where a workspace clone grafts onto
    // a cut. `env.clone` (hourly) is the same verb on a STOPPED source; this one is the running
    // source, which is what a person actually clicks.
    Slo { id: "env.clone.p95", feature: "Environments", sli: "Cloning a running environment completes with its services ready", target: p95(120_000), suite: Suite::Fast, stage: "6 · Environment" },

    // Stage 7 · lifecycle
    Slo { id: "ws.stop.p95", feature: "Workspace lifecycle", sli: "Stopping a workspace completes", target: p95(15_000), suite: Suite::Fast, stage: "7 · Lifecycle" },
    Slo { id: "ws.replicated", feature: "Workspace lifecycle", sli: "A stopped workspace's final sync point reaches a replica, named by that replica", target: bound(300_000), suite: Suite::Fast, stage: "7 · Lifecycle" },
    Slo { id: "ws.start.p95", feature: "Workspace lifecycle", sli: "Starting a workspace completes", target: p95(30_000), suite: Suite::Fast, stage: "7 · Lifecycle" },
    Slo { id: "ws.restore", feature: "Workspace lifecycle", sli: "Restoring a workspace from a past snapshot succeeds", target: avail(99.9), suite: Suite::Fast, stage: "7 · Lifecycle" },
    // The environment twin of the four ids above — the owner's rule is that every workspace SLO
    // has an environment counterpart at the same cadence, because the two control planes converge
    // through different reconcilers and a green workspace says nothing about an environment.
    Slo { id: "env.stop.p95", feature: "Environments", sli: "Stopping an environment completes", target: p95(30_000), suite: Suite::Fast, stage: "7 · Lifecycle" },
    Slo { id: "env.replicated", feature: "Environments", sli: "A stopped environment's final sync point reaches a replica", target: bound(300_000), suite: Suite::Fast, stage: "7 · Lifecycle" },
    Slo { id: "env.start.p95", feature: "Environments", sli: "Starting an environment completes", target: p95(60_000), suite: Suite::Fast, stage: "7 · Lifecycle" },
    Slo { id: "env.restore", feature: "Environments", sli: "Restoring an environment from a past snapshot succeeds", target: avail(99.9), suite: Suite::Fast, stage: "7 · Lifecycle" },
    Slo { id: "vol.refusals", feature: "Workspace lifecycle", sli: "Deleting a sync point or a running worktree's base snapshot is refused", target: avail(99.9), suite: Suite::Fast, stage: "7 · Lifecycle" },
    Slo { id: "vol.detached.restorable", feature: "Workspace lifecycle", sli: "A detached volume's snapshot can still be restored", target: avail(99.9), suite: Suite::Fast, stage: "7 · Lifecycle" },
    // `retire_pass` is the rule at BOTH ends, so the SLI names both: the sweep that takes an
    // orphaned tree, and the Volume with no owner entry and no snapshot behind it.
    Slo { id: "vol.orphan.collected", feature: "Workspace lifecycle", sli: "An orphaned volume directory is collected, and a Volume with no owner entry and no snapshot is deleted", target: bound(300_000), suite: Suite::Fast, stage: "7 · Lifecycle" },
    // `cleanup_parent`'s detach-or-keep rule, which is exactly where a lost detach strands bytes
    // nothing on any tier can find again. Both directions, or neither says anything.
    Slo { id: "wt.delete", feature: "Workspace lifecycle", sli: "Deleting a workspace or environment drops the worktree and leaves the volume iff a snapshot remains", target: bound(60_000), suite: Suite::Fast, stage: "7 · Lifecycle" },
    Slo { id: "snap.delete", feature: "Workspace lifecycle", sli: "Deleting a snapshot removes it from history, and the last one of a detached volume takes the volume with it", target: avail(99.9), suite: Suite::Fast, stage: "7 · Lifecycle" },

    // Stage 8 · admin
    Slo { id: "req.queue", feature: "Admin", sli: "A Request CR is queued and answerable by an admin", target: bound(5_000), suite: Suite::Fast, stage: "8 · Admin" },
    Slo { id: "audit.row", feature: "Admin", sli: "Every admin write produces an audit row, and the same write reaches `kloudlite.events` as `admin.<action>`", target: avail(99.9), suite: Suite::Fast, stage: "8 · Admin" },
    Slo { id: "signals.fresh", feature: "Admin", sli: "The Signals table reflects a rule transition, and a rule with no covering samples reads `unknown` rather than `ok`", target: bound(120_000), suite: Suite::Fast, stage: "8 · Admin" },
    Slo { id: "history.api", feature: "Admin", sli: "The history API answers a chart query", target: avail(99.9), suite: Suite::Fast, stage: "8 · Admin" },

    // Stage 9 · security
    Slo { id: "sec.private.repo", feature: "Security", sli: "A private repo is unreadable to a non-collaborator", target: avail(100.0), suite: Suite::Fast, stage: "9 · Security" },
    Slo { id: "sec.cross.owner", feature: "Security", sli: "One owner's objects are invisible to another owner", target: avail(100.0), suite: Suite::Fast, stage: "9 · Security" },
    Slo { id: "sec.admin.claim", feature: "Security", sli: "An admin route refuses a token without the superadmin claim", target: avail(100.0), suite: Suite::Fast, stage: "9 · Security" },
    Slo { id: "sec.user.process", feature: "Security", sli: "The ordinary API process has no admin route mounted", target: avail(100.0), suite: Suite::Fast, stage: "9 · Security" },
    // Both halves: the ClusterRole allows exactly three spec writes (`Volume.spec.restoreTo`,
    // `Volume.spec.quotaGb`, `take_volume`'s test-patch), and a policy that refused EVERYTHING
    // would pass an SLI that only watched the refusal.
    Slo { id: "sec.agent.spec", feature: "Security", sli: "The admission policy refuses a spec write outside the allowed fields and still admits the allowed ones", target: avail(100.0), suite: Suite::Fast, stage: "9 · Security" },
    Slo { id: "id.token.revoked", feature: "Security", sli: "A revoked token is refused", target: avail(99.9), suite: Suite::Fast, stage: "9 · Security" },
    Slo { id: "repo.visibility", feature: "Security", sli: "Flipping a repo private hides it from a non-collaborator and flipping it public restores it", target: avail(100.0), suite: Suite::Fast, stage: "9 · Security" },

    // Stage 10 · edge and pipeline
    Slo { id: "edge.dns", feature: "Edge and pipeline", sli: "The public hostname resolves", target: avail(99.99), suite: Suite::Fast, stage: "10 · Edge" },
    Slo { id: "edge.cert", feature: "Edge and pipeline", sli: "The TLS certificate is valid for the public hostname", target: avail(99.9), suite: Suite::Fast, stage: "10 · Edge" },
    Slo { id: "edge.origin", feature: "Edge and pipeline", sli: "Cloudflare reaches the origin", target: avail(99.9), suite: Suite::Fast, stage: "10 · Edge" },
    Slo { id: "edge.ssh.lb", feature: "Edge and pipeline", sli: "The SSH load balancer accepts a connection", target: avail(99.9), suite: Suite::Fast, stage: "10 · Edge" },
    Slo { id: "tel.log.latency", feature: "Edge and pipeline", sli: "A structured log line reaches HyperDX", target: bound(60_000), suite: Suite::Fast, stage: "10 · Edge" },
    Slo { id: "tel.pod.coverage", feature: "Edge and pipeline", sli: "Every pod is scraped by the region's collector", target: bound(60_000), suite: Suite::Fast, stage: "10 · Edge" },
    Slo { id: "tel.stream.lag", feature: "Edge and pipeline", sli: "The Redis events stream consumer lag stays low", target: bound(60_000), suite: Suite::Fast, stage: "10 · Edge" },
    Slo { id: "tel.ch.disk", feature: "Edge and pipeline", sli: "ClickHouse disk usage is reported", target: bound(60_000), suite: Suite::Fast, stage: "10 · Edge" },

    // Hourly · Experience. The owner's addendum: every remaining verb a person can perform, walked
    // once an hour on top of the fast journey — so an hourly run is also a fast sample.
    Slo { id: "ws.packages.add", feature: "Workspaces", sli: "Adding a package to a running workspace makes it runnable (`which`)", target: p95(180_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "ws.packages.remove", feature: "Workspaces", sli: "Removing it makes it disappear from the profile", target: p95(120_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "ws.seeded", feature: "Workspaces", sli: "A workspace created from a repo and branch has that clone checked out", target: p95(180_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "key.platform.regenerate", feature: "Identity", sli: "Regenerating the platform key keeps seeding working", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "team.create", feature: "Teams", sli: "A team can be created by a person", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "team.invite.accept", feature: "Teams", sli: "An invite is created, previewed and accepted once", target: bound(5_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "team.role.set", feature: "Teams", sli: "A member's role changes and is reflected in the profile", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "team.repo.shared", feature: "Teams", sli: "A member clones a team repo; a non-member is refused", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "team.workspace", feature: "Teams", sli: "A team workspace lands in the team namespace and starts", target: p95(90_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "team.member.remove", feature: "Teams", sli: "A removed member loses access to the team repo", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "team.delete", feature: "Teams", sli: "Deleting the team removes its profile and refuses its slug", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "repo.protection", feature: "Git hosting", sli: "A protected branch refuses a direct push and still merges via a PR", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "repo.commit.patch", feature: "Git hosting", sli: "An edit made through the web commit endpoint lands in the log", target: bound(5_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "repo.compare", feature: "Git hosting", sli: "Comparing two branches lists the right commits", target: bound(1_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "pr.comment", feature: "Pull requests", sli: "A comment on a PR is readable back", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "pr.close", feature: "Pull requests", sli: "A closed PR is refused a merge", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "commit.verify", feature: "Git hosting", sli: "The signature endpoint answers for a pushed commit", target: bound(1_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "env.services.multi", feature: "Environments", sli: "An environment with two services has both ready and resolving each other", target: p95(180_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "env.clone", feature: "Environments", sli: "A stopped environment clones with all services ready", target: p95(180_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "env.restore.inplace", feature: "Environments", sli: "Restore in place brings a service's data back", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "env.stop.start", feature: "Environments", sli: "Stop then start round trip", target: p95(120_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "vol.history", feature: "Workspace lifecycle", sli: "History lists pushes newest first with their messages; refs answer", target: bound(1_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "quota.view", feature: "Admin", sli: "`GET /v1/quota` reflects the objects the run holds", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "request.approve", feature: "Admin", sli: "An approved quota request raises the quota and unblocks the refused create", target: bound(60_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "admin.stop.workspace", feature: "Admin", sli: "An admin stop is visible to the owner as `stopped`", target: bound(30_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "superadmin.grant", feature: "Security", sli: "Granting superadmin adds the account to the roster and revoking takes it off", target: avail(100.0), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "feed.experience", feature: "Pull requests", sli: "The feed shows the team and repo events of this run", target: bound(30_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "home.persists", feature: "Workspaces", sli: "A file written in one workspace is read from a fresh workspace's home, with the cache and state directories still local", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    // The 2026-09-05 coverage review's remaining verbs. Each sits in the Experience stage because
    // its nearest existing twin does — every one of them is a whole flow rather than a request.
    Slo { id: "id.username", feature: "Identity", sli: "Claiming a username succeeds once and the second claim is refused", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "id.cli.tokens", feature: "Identity", sli: "A CLI token is listed and, once revoked, is refused", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "id.profile.upsert", feature: "Identity", sli: "A profile upsert is saved and read back", target: bound(5_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "id.cli.sshconfig", feature: "Identity", sli: "`kl ws sshconfig` writes a host block naming a running workspace", target: bound(15_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "key.ssh.lifecycle", feature: "Identity", sli: "A newly added SSH key clones, and after removal the same key is refused", target: bound(30_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "repo.description", feature: "Git hosting", sli: "A repo description is saved and read back", target: bound(5_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "pr.merge.strategies", feature: "Pull requests", sli: "Each merge strategy — merge, squash, rebase, fast-forward — lands the expected tree", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "pr.mergeability", feature: "Pull requests", sli: "Mergeability is reported clean for a clean change and dirty for a conflicting one", target: bound(30_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "team.invite.revoke", feature: "Teams", sli: "A revoked invite token is refused", target: avail(100.0), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "team.environment", feature: "Teams", sli: "A team environment lands in the team namespace and its services resolve", target: p95(180_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "env.attach.pair", feature: "Environments", sli: "Deleting an attached workspace removes the environment-side policy", target: bound(30_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "vol.list", feature: "Workspace lifecycle", sli: "The volume list names every volume the run holds", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "admin.stop.environment", feature: "Admin", sli: "An admin stop of an environment is visible to the owner as `stopped`", target: bound(30_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "admin.delete.workload", feature: "Admin", sli: "An admin delete takes a workspace and an environment away", target: bound(60_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "admin.screens", feature: "Admin", sli: "The owners, clusters and overview console screens answer", target: bound(10_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "admin.workloads.read", feature: "Admin", sli: "`GET /admin/workloads` lists every roll target", target: bound(5_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "audit.export", feature: "Admin", sli: "The audit CSV export answers with a header and this run's rows", target: bound(10_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "req.decide.kinds", feature: "Admin", sli: "An access request grants membership and a denied request is closed with its reason", target: bound(60_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "req.legacy.union", feature: "Admin", sli: "The retired quota-request queue is unioned into the admin queue and migrates", target: bound(10_000), suite: Suite::Hourly, stage: "14 · Experience" },
    // The region CREATE is deliberately not probed: `crd::Region` has no delete on any tier — a
    // second POST only retires or renames one — so a probe region would be permanent shared state.
    Slo { id: "region.status", feature: "Admin", sli: "The region list and this run's cluster status answer", target: bound(5_000), suite: Suite::Hourly, stage: "14 · Experience" },

    // Weekly
    Slo { id: "git.push.large", feature: "Git hosting", sli: "Push of a large commit over HTTP succeeds", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "reg.push.large", feature: "Container registry", sli: "Pushing a large image layer succeeds", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "ws.cold.profile", feature: "Workspaces", sli: "A cold package profile builds successfully", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "ws.profile.reuse", feature: "Workspaces", sli: "A repeat package set is published from the profile index, not rebuilt", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "ws.cross.node", feature: "Workspaces", sli: "A workspace started on a peer node reads its replica correctly", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "homes.cross.node", feature: "Workspaces", sli: "The shared home is consistent across nodes", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "env.cross.node", feature: "Environments", sli: "An environment started on a peer node reads its replica correctly", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "cp.failover", feature: "Control plane", sli: "The leader lease fails over to another pod", target: bound(30_000), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "settings.live", feature: "Control plane", sli: "A live settings change takes effect on the next beat", target: bound(60_000), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "settings.revert", feature: "Control plane", sli: "Reverting to a stored settings version restores it", target: bound(60_000), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "settings.roll", feature: "Control plane", sli: "A Boot-marked save is refused with 409 while one of its readers is mid-rollout", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    // Weekly, and only the KEEP-BIASED half: `BLOB_GRACE` is a fixed hour and the weekly CronJob's
    // own `activeDeadlineSeconds` is 3600, so no run can watch an unreferenced blob be reclaimed
    // in-band. What it CAN prove is the rule the sweep is written around — a sibling's layer
    // survives a delete — which is the failure that loses somebody's image.
    Slo { id: "reg.gc.sweep", feature: "Container registry", sli: "A blob a sibling image still references survives that image's deletion and a GC pass", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },

    // Monthly
    Slo { id: "bak.tarball.age", feature: "Backups", sli: "The latest backup tarball is recent", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    Slo { id: "bak.daily.slots", feature: "Backups", sli: "Every daily backup slot is present", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    Slo { id: "bak.versioning", feature: "Backups", sli: "Backup versioning is enabled and retains history", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    Slo { id: "bak.cosmos", feature: "Backups", sli: "The Cosmos backup for HyperDX succeeds", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    Slo { id: "drill.dead.node", feature: "Resilience drills", sli: "A dead-node drill heals every replica onto a live node", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    Slo { id: "drill.drain", feature: "Resilience drills", sli: "A drain drill succeeds without interrupting a running worktree", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    Slo { id: "drill.redis.down", feature: "Resilience drills", sli: "The system keeps operating correctly with Redis down", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    Slo { id: "cluster.decommission", feature: "Resilience drills", sli: "A decommission is refused until the agent stamps `drained`, then cordons the node", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
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
            // Only the id column is fenced in the doc. Stripping backticks from every cell used to
            // be harmless and is not any more: an SLI may legitimately start or end with one
            // (`` `GET /v1/quota` reflects … ``), and trimming it there compared two different
            // strings and blamed the catalogue.
            .map(|l| {
                l.trim_matches('|')
                    .split('|')
                    .enumerate()
                    .map(|(i, c)| {
                        let c = c.trim();
                        if i == 0 { c.trim_matches('`') } else { c }.to_string()
                    })
                    .collect()
            })
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
        let (fast, hourly, weekly, monthly) = (
            journey(Suite::Fast),
            journey(Suite::Hourly),
            journey(Suite::Weekly),
            journey(Suite::Monthly),
        );
        // Monthly is the only suite that walks every stage; Experience is hourly's alone, which is
        // why the stage list is not simply a prefix of `STAGES` for every suite.
        assert_eq!(names(&monthly), &STAGES[..STAGES.len() - 1]);
        assert_eq!(*names(&fast).last().unwrap(), "11 · Teardown");
        assert!(names(&weekly).starts_with(&names(&fast)));
        assert!(names(&monthly).starts_with(&names(&weekly)));
        assert_eq!(names(&hourly), [names(&fast), vec!["14 · Experience"]].concat());
        // Boot and Teardown probe nothing and are still there.
        assert!(fast.iter().any(|(n, ids)| *n == "0 · Boot" && ids.is_empty()));
        assert!(fast.iter().any(|(n, ids)| *n == "11 · Teardown" && ids.is_empty()));
        // Every id exactly once across the two journeys that between them cover every stage, and
        // every stage a catalogue row names is one of `STAGES`.
        let mut all = ids(&monthly);
        let experience: Vec<&str> = ids(&hourly).into_iter().filter(|id| !all.contains(id)).collect();
        all.extend(experience);
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
