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
    // Walked right after 6, whose workspace and environment it grants between; numbered 15
    // because stage names are stored in ClickHouse and renumbering would rewrite history.
    "15 · Cluster controller",
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
        .map(|name| {
            let ids = CATALOGUE.iter().filter(|s| s.stage == name && walks(suite, s.suite)).map(|s| s.id);
            (name, ids.collect())
        })
        .collect()
}

/// The hourly suite is an Indexed Job of this many pods, each walking one GROUP of the journey as
/// its own run (`hourly-{ts}-g{n}`), filing only that group's ids.
///
/// The partition is by id, and state decides it rather than the stage list: Experience reads stage
/// 2's repo and key and revokes that key, stage 15 grants between stage 5's workspace and stage
/// 6's environment, and stage 7 stops that environment — so all of that stays in one pod (0). What
/// moves out shares nothing with it: the intercept journey stands up its own team (1), the seed
/// failure its own workspace (2), and every step on the owner's bench (3), because
/// `bench.idle.wake` needs every client gone for `benchIdleSecs` and stage 5's stop/start of that
/// same bench would reset it. A pod with no index (a hand run) walks everything, as before.
///
/// It lives here, beside the journey, because the console has to slice a group run's journey the
/// same way the probe walked it: the partition is a fact about the catalogue, not about the binary.
pub const HOURLY_GROUPS: u8 = 4;

/// Group 3. Not `bench.workspace.tool_roundtrip` or `bench.shell.workspace`: both run in group 0's
/// workspace, so group 0 walks them after waiting for this group to finish (`suite::wait_for_group`).
const BENCH_IDS: [&str; 21] = [
    "bench.create",
    "bench.start.p95",
    "bench.tunnel",
    "bench.idle.wake",
    "bench.session.roundtrip",
    "bench.tools.own_hands",
    "bench.proposal.asked",
    "bench.exchange.both_views",
    "bench.two_clients",
    "bench.tool.token",
    "bench.tool.audience",
    "bench.tool.revoked",
    "shell.up",
    "shell.fenced",
    "shell.no_tools",
    "bench.no_hands",
    "bench.pkg_needs_workspace",
    "bench.shell.roundtrip",
    "bench.push.p95",
    "bench.pkg.add",
    "agent.tree.run",
];

/// Group 1: every id the intercept journey owns. It is also the skip list a probe run that cannot
/// get there files, and a missing id reads as passed — so it must name all of them, not only the
/// ones a given path reaches.
pub const INTERCEPT_IDS: [&str; 13] = [
    "env.space.bench",
    "env.intercept",
    "env.intercept.proxy.up",
    "env.intercept.delivered",
    "env.intercept.remap",
    "env.intercept.peer",
    "env.intercept.bench",
    "env.intercept.proxy.restart",
    "env.intercept.release",
    "env.intercept.fallback",
    "env.intercept.refused",
    "env.intercept.udp.refused",
    "env.intercept.tools.refused",
];

/// The hourly group that walks `id`.
pub fn group_of(id: &str) -> u8 {
    if BENCH_IDS.contains(&id) {
        3
    } else if id == "ws.seed.failed" || id == "team.member.paused" {
        2
    } else if INTERCEPT_IDS.contains(&id) {
        1
    } else {
        0
    }
}

/// The journey ONE group walks: the suite's journey with every other group's ids dropped, and a
/// stage that lost all of its own ids dropped with them. A stage that carries no ids at all (Boot,
/// Teardown) is kept — every pod boots, and the parent always runs teardown. Any suite but hourly
/// is ungrouped and walks the whole journey.
pub fn journey_for_group(suite: Suite, group: u8) -> Vec<(&'static str, Vec<&'static str>)> {
    let full = journey(suite);
    if suite != Suite::Hourly {
        return full;
    }
    full.into_iter()
        .filter_map(|(name, ids)| {
            let stageless = ids.is_empty();
            let mine: Vec<&str> = ids.into_iter().filter(|id| group_of(id) == group).collect();
            (stageless || !mine.is_empty()).then_some((name, mine))
        })
        .collect()
}

/// Whether a run of `suite` produces samples for a row marked `of`.
///
/// A stage is NOT a suite: stage 2 carries fast ids and two hourly ones, so the walk has to ask
/// per ROW rather than assume every id in a stage belongs to whoever walks that stage — a fast run
/// that reported an hourly id would file a sample nobody asked for, and skip it on every
/// five-minute tick.
fn walks(suite: Suite, of: Suite) -> bool {
    match suite {
        Suite::Fast => of == Suite::Fast,
        Suite::Hourly => matches!(of, Suite::Fast | Suite::Hourly),
        Suite::Weekly => matches!(of, Suite::Fast | Suite::Weekly),
        Suite::Monthly => matches!(of, Suite::Fast | Suite::Weekly | Suite::Monthly),
    }
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
    Slo { id: "id.cli.flow", feature: "Identity", sli: "The kl-connect CLI's login-to-command flow completes", target: bound(15_000), suite: Suite::Fast, stage: "1 · Identity" },
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
    // Hourly, not fast: a branch delete is a push and four reads on a repo the fast suite
    // already walks ten other ways, and nobody deletes a branch every five minutes.
    Slo { id: "git.branch.delete", feature: "Git hosting", sli: "A branch pushed by this run is deleted through `DELETE /v1/repos/{owner}/{name}/branches/{branch}` and `refs` no longer lists it", target: avail(99.9), suite: Suite::Hourly, stage: "2 · Git" },
    Slo { id: "git.branch.delete.refused", feature: "Git hosting", sli: "Deleting the default branch answers 409 and it is still listed", target: avail(99.9), suite: Suite::Hourly, stage: "2 · Git" },
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
    // Hourly: a fresh throwaway team, whose only member is its creator, is enough to prove
    // both halves of the rule the registry actually enforces (`may_act(caller, owner)`, never
    // what a token was minted under) — no invite round trip needed every five minutes.
    Slo { id: "reg.team.push", feature: "Container registry", sli: "A team member's personal credential pushes to the team's new image with no 5xx on its first requests, and a non-member's is DENIED", target: avail(99.9), suite: Suite::Hourly, stage: "4 · Registry" },

    // Stage 5 · workspace
    Slo { id: "ws.create.p95", feature: "Workspaces", sli: "Creating a workspace completes", target: p95(90_000), suite: Suite::Fast, stage: "5 · Workspace" },
    // "…and its home is the shared export": a pod started before its node's NFS mount is up
    // hostPaths an empty local directory and strands the owner's dotfiles, which an exec that only
    // opened a channel — or only echoed — would pass straight through.
    Slo { id: "ws.exec.ok", feature: "Workspaces", sli: "Exec into a running workspace pod returns the command's output, from a pod whose home is the shared export", target: avail(99.9), suite: Suite::Fast, stage: "5 · Workspace" },
    Slo { id: "homes.rw.p95", feature: "Workspaces", sli: "A read/write round trip on the shared home completes", target: p95(200), suite: Suite::Fast, stage: "5 · Workspace" },
    Slo { id: "gw.tunnel.p95", feature: "Workspaces", sli: "Opening a gateway SSH tunnel completes", target: p95(3_000), suite: Suite::Fast, stage: "5 · Workspace" },
    Slo { id: "gw.unregistered.refused", feature: "Workspaces", sli: "The gateway refuses an unregistered key", target: avail(99.9), suite: Suite::Fast, stage: "5 · Workspace" },
    // Keys belong to the PERSON and reach a pod only through the `OwnerKeys` projection, so the
    // three halves are probed apart: the projection is Synced, the key opens the workspace, and a
    // removal is honoured by both listeners. A green tunnel with a stale projection is the failure
    // mode a single "ssh works" step would keep quiet about.
    Slo { id: "key.projected", feature: "Workspaces", sli: "A registered key reaches the owner's OwnerKeys projection as Synced", target: bound(30_000), suite: Suite::Fast, stage: "5 · Workspace" },
    Slo { id: "key.live", feature: "Workspaces", sli: "A registered key opens the workspace over the gateway", target: avail(99.9), suite: Suite::Fast, stage: "5 · Workspace" },
    // 330 s, not the git listener's 10: git authenticates from the directory on every request, but
    // a workspace pod only learns of the removal when the api's resync beat (`KEYS_RESYNC_SECS`,
    // 300 s) rewrites the projection.
    Slo { id: "ws.push.p95", feature: "Workspaces", sli: "Pushing a workspace snapshot completes", target: p95(60_000), suite: Suite::Fast, stage: "5 · Workspace" },
    Slo { id: "ws.clone.p95", feature: "Workspaces", sli: "Cloning a workspace completes", target: p95(60_000), suite: Suite::Fast, stage: "5 · Workspace" },
    // The sentence, not merely the status: `quota::refuse` answers `"{dimension}: {used} of
    // {limit} in use; request more under Quota"`, and a 409 naming the wrong dimension is a gate
    // that refused for a reason nobody asked about.
    Slo { id: "quota.refused", feature: "Workspaces", sli: "A verb that fills disk is refused with 409 naming diskGb, what is occupied and the limit", target: avail(99.9), suite: Suite::Fast, stage: "5 · Workspace" },
    // Create is only one of the four verbs behind `guard_alloc`; restore, clone and push route
    // through the same gate and none was probed. Each is stood against the gate by PINCHING the
    // owner's limit, never by asking for a big ceiling: disk is charged by occupied bytes, so a
    // declared size is not an allocation and a restore asking for `u32::MAX` is simply accepted.
    Slo { id: "env.quota.refused", feature: "Workspaces", sli: "A restore, a clone and a push are each refused with 409 when the owner's limit is below what the run occupies or holds", target: avail(99.9), suite: Suite::Fast, stage: "5 · Workspace" },
    // The owner's bench. Its object name is a hash of (owner, team), so it is long-lived rather
    // than a `run-{id}` object: left Running with no client, it sleeps between runs.
    Slo { id: "bench.create", feature: "Benches", sli: "`POST /v1/bench` answers, and a second POST names the same id", target: avail(99.9), suite: Suite::Fast, stage: "5 · Workspace" },
    Slo { id: "bench.start.p95", feature: "Benches", sli: "A started bench reaches phase `ready`", target: p95(90_000), suite: Suite::Fast, stage: "5 · Workspace" },
    Slo { id: "bench.tunnel", feature: "Benches", sli: "A bench token opens the tunnel and `/healthz` answers through it", target: bound(20_000), suite: Suite::Fast, stage: "5 · Workspace" },
    // Hourly, like the intercept journey: the build itself waits on the gate starting a pod,
    // which is too much to pay every five minutes, and the builder must be Stopped going in or
    // the sample is timing someone else's cold start.
    Slo { id: "ws.build.p95", feature: "Workspaces", sli: "`kl container build` of a two-line Dockerfile in the probe workspace, from a non-login exec, is pushed to the probe owner's own image and its manifest is readable through `/v2`; the builder was Stopped before the step", target: p95(180_000), suite: Suite::Hourly, stage: "5 · Workspace" },
    // `kl push` is a registry-side copy through buildx imagetools; the probe promotes the image
    // the build above just pushed and reads the new tag's digest back, so the step proves both
    // the copy and that the credential helper serves imagetools as it serves build.
    Slo { id: "ws.build.promote", feature: "Workspaces", sli: "`kl container push` copies the probe's just-built image to a second tag and `docker buildx imagetools inspect` reads that tag's digest back", target: bound(30_000), suite: Suite::Hourly, stage: "5 · Workspace" },
    // The other two verbs `kl` grew: a person edits their package list and moves their space from
    // inside the workspace, with no browser. Judged on the API's own state rather than on what the
    // command printed — a `kl` that reached nothing would print the same lines.
    Slo { id: "ws.kl.pkg.add", feature: "Workspaces", sli: "`kl pkg add cowsay` inside the probe workspace exits 0 and `GET /v1/workspaces/{id}` then declares the package", target: bound(20_000), suite: Suite::Hourly, stage: "5 · Workspace" },
    // Disk is charged by what a volume OCCUPIES, and the only thing that makes that true is the
    // holding node stamping `usedBytes` on its sync beat: a stamp that stopped would charge every
    // owner the 1 GiB floor forever and nothing else here would notice.
    Slo { id: "vol.usage.stamped", feature: "Workspaces", sli: "200 MB written in the probe workspace through its tool server is charged to the volume: `Volume.status.usedBytes` reads at least that within two sync beats", target: bound(150_000), suite: Suite::Hourly, stage: "5 · Workspace" },
    Slo { id: "ws.kl.env.switch", feature: "Workspaces", sli: "`kl env switch` inside the probe workspace moves the person's own space to the run's environment — `GET /v1/me/environments` names it — and `kl env clear` puts it back", target: bound(10_000), suite: Suite::Hourly, stage: "6 · Environment" },

    // Stage 6 · environment
    Slo { id: "env.create.p95", feature: "Environments", sli: "Creating an environment completes", target: p95(120_000), suite: Suite::Fast, stage: "6 · Environment" },
    Slo { id: "env.dns", feature: "Environments", sli: "A service in an environment resolves a sibling by bare name and connects to it", target: avail(99.9), suite: Suite::Fast, stage: "6 · Environment" },
    Slo { id: "env.attach", feature: "Environments", sli: "Choosing an environment for a space takes effect in its workspace", target: bound(10_000), suite: Suite::Fast, stage: "6 · Environment" },
    Slo { id: "env.detach", feature: "Environments", sli: "Clearing a space's environment takes effect in its workspace", target: bound(10_000), suite: Suite::Fast, stage: "6 · Environment" },
    // The promise itself: a workspace ALREADY running when the space chose resolves the service,
    // with no restart.
    Slo { id: "env.space.live", feature: "Environments", sli: "A second workspace already running when the space chooses an environment resolves its service without a restart", target: bound(10_000), suite: Suite::Fast, stage: "6 · Environment" },
    // A CLONE shares its source's volume, so it has no `{pool}/vol/{id}` of its own — which the
    // janitor's old keep-set read as "not live", deleting a running clone's attach file an hour
    // in. The probe cannot wait out that 1 h floor, so it judges the thing that sweep would
    // destroy: the file is still mounted and the environment's names still resolve INSIDE the
    // clone. Hourly, not fast: it is a second exec against a pod the fast journey already pays for.
    Slo { id: "ws.clone.attach.survives", feature: "Environments", sli: "A cloned workspace, which owns no volume directory of its own, still mounts its attach file: `/etc/resolv.conf` inside the clone names the environment and its service resolves", target: bound(10_000), suite: Suite::Hourly, stage: "6 · Environment" },
    Slo { id: "env.push.p95", feature: "Environments", sli: "Pushing an environment snapshot completes", target: p95(90_000), suite: Suite::Fast, stage: "6 · Environment" },
    Slo { id: "env.exec.ok", feature: "Environments", sli: "Exec into a running service pod of the environment succeeds", target: avail(99.9), suite: Suite::Fast, stage: "6 · Environment" },
    // 120 s, not the workspace clone's 60: an environment copies LIVE bytes from the node that
    // holds it and then waits for every service's StatefulSet, where a workspace clone grafts onto
    // a cut. `env.clone` (hourly) is the same verb on a STOPPED source; this one is the running
    // source, which is what a person actually clicks.
    Slo { id: "env.clone.p95", feature: "Environments", sli: "Cloning a running environment completes with its services ready", target: p95(120_000), suite: Suite::Fast, stage: "6 · Environment" },
    // Hourly, not fast: the journey stands up a SECOND environment (one service to dial from,
    // one to intercept — an intercept scales the real service to 0) and a second workspace,
    // which is too much to pay every five minutes.
    Slo { id: "env.intercept", feature: "Environments", sli: "An intercepted service answers from the attached workspace on a remapped port", target: p95(120_000), suite: Suite::Hourly, stage: "6 · Environment" },
    // The one that matters most: the workspace is STOPPED, never released by hand, so this is
    // the automatic path — and both halves are asserted, because a fallback that also cleared
    // `spec.intercepts` would silently discard what the person asked for.
    Slo { id: "env.intercept.fallback", feature: "Environments", sli: "Stopping the workspace brings the real service back on its own, and the intercept is still in the environment's spec", target: p95(180_000), suite: Suite::Hourly, stage: "6 · Environment" },
    // The proxy's own ids. Every one of them ends in a DIAL whose bytes are checked: the bug this
    // journey exists for is one where every object was perfect and the packets were dropped, so an
    // id that asserts an object's state and not an answer would have passed straight through it.
    Slo { id: "env.intercept.proxy.up", feature: "Environments", sli: "The proxy pod for an intercepted service is Ready and the service's endpoints name it", target: p95(90_000), suite: Suite::Hourly, stage: "6 · Environment" },
    Slo { id: "env.intercept.delivered", feature: "Environments", sli: "A request from an environment pod to the intercepted service is answered by the workspace", target: p95(120_000), suite: Suite::Hourly, stage: "6 · Environment" },
    Slo { id: "env.intercept.remap", feature: "Environments", sli: "The intercepted service answers on its own port while the workspace listens on another", target: avail(99.9), suite: Suite::Hourly, stage: "6 · Environment" },
    // The bug, as a probe: a follower's packets used to be DNAT'd into another owner's namespace,
    // whose ingress admitted only the environment's.
    Slo { id: "env.intercept.peer", feature: "Environments", sli: "A second space following the same environment reaches the intercepted service", target: p95(120_000), suite: Suite::Hourly, stage: "6 · Environment" },
    Slo { id: "env.intercept.bench", feature: "Environments", sli: "The probe owner's bench reaches the intercepted service", target: p95(120_000), suite: Suite::Hourly, stage: "6 · Environment" },
    // The uid assertion is the whole claim of a proxy over baked-in addressing: the workspace's pod
    // comes back with a new IP and the SAME proxy keeps serving.
    Slo { id: "env.intercept.proxy.restart", feature: "Environments", sli: "Killing the intercepting workspace's pod does not interrupt delivery beyond the pod's own restart, and no proxy is recreated", target: p95(180_000), suite: Suite::Hourly, stage: "6 · Environment" },
    Slo { id: "env.intercept.release", feature: "Environments", sli: "Releasing the intercept brings the real service back, removes the proxy and leaves no grant behind", target: p95(180_000), suite: Suite::Hourly, stage: "6 · Environment" },
    // The UDP refusal is SKIPPED, not passed, until `model::Service` carries a protocol: nothing can
    // declare a UDP port today, so the probe has no way to provoke one — and a silent pass would
    // report a guard nobody has as kept.
    Slo { id: "env.intercept.udp.refused", feature: "Environments", sli: "An intercept of a UDP port is refused", target: avail(99.9), suite: Suite::Hourly, stage: "6 · Environment" },
    Slo { id: "env.intercept.tools.refused", feature: "Environments", sli: "An intercept mapping onto the tool server's port is refused", target: avail(99.9), suite: Suite::Hourly, stage: "6 · Environment" },
    Slo { id: "env.intercept.refused", feature: "Environments", sli: "An intercept of a workspace whose space uses no environment, and one naming a port the service does not declare, are both refused", target: avail(99.9), suite: Suite::Hourly, stage: "6 · Environment" },
    // Hourly: the bench may be asleep, and waking it is not a five-minute cost.
    Slo { id: "env.space.bench", feature: "Environments", sli: "The probe owner's bench follows its space's environment in its resolv.conf", target: bound(300_000), suite: Suite::Hourly, stage: "6 · Environment" },
    // Hourly: proving a hidden thing stays hidden is not a five-minute cost, and the builder
    // is not stood up by this id — it asks about whatever the owner's builder already is.
    Slo { id: "builder.hidden", feature: "Environments", sli: "The probe owner's builder is absent from `GET /v1/environments` and its id answers 404 on get, start, push and snapshots", target: avail(99.9), suite: Suite::Hourly, stage: "6 · Environment" },

    // 15 · Cluster controller. The single elected writer of the space grants (stage 1 of
    // `docs/superpowers/specs/2026-09-14-cluster-controller-design.md`). Every id but `ctl.leader`
    // and `ctl.agent.nowrite` judges by CONNECTING, never by reading a policy object: a rendered
    // NetworkPolicy that the CNI never programmed is exactly the outage these exist to catch.
    Slo { id: "ctl.leader", feature: "Cluster controller", sli: "Exactly one controller pod holds the `kloudlite-controller` Lease and has renewed it within its TTL", target: avail(99.9), suite: Suite::Fast, stage: "15 · Cluster controller" },
    Slo { id: "ctl.grant.set", feature: "Cluster controller", sli: "Choosing an environment for a space lets a probe workspace resolve and connect to one of its services", target: bound(30_000), suite: Suite::Fast, stage: "15 · Cluster controller" },
    Slo { id: "ctl.grant.switch", feature: "Cluster controller", sli: "Switching the choice makes the new environment's service reachable and the old one unreachable", target: bound(30_000), suite: Suite::Fast, stage: "15 · Cluster controller" },
    Slo { id: "ctl.grant.cleared", feature: "Cluster controller", sli: "Clearing the choice refuses the connect and leaves no `space-*` policy for that space", target: bound(30_000), suite: Suite::Fast, stage: "15 · Cluster controller" },
    Slo { id: "ctl.failover", feature: "Cluster controller", sli: "Deleting the leader pod elects another within 20 s and a choice made during the gap converges once it is up", target: bound(120_000), suite: Suite::Hourly, stage: "15 · Cluster controller" },
    Slo { id: "ctl.agent.nowrite", feature: "Cluster controller", sli: "Every `space-*` NetworkPolicy in the cluster is managed by `kloudlite-controller` and by no agent", target: avail(99.9), suite: Suite::Hourly, stage: "15 · Cluster controller" },
    Slo { id: "ctl.fanout", feature: "Cluster controller", sli: "One change of a space's choice reconciles at most two environments", target: avail(99.9), suite: Suite::Hourly, stage: "15 · Cluster controller" },

    // Stage 7 · lifecycle
    Slo { id: "ws.stop.p95", feature: "Workspace lifecycle", sli: "Stopping a workspace completes", target: p95(15_000), suite: Suite::Fast, stage: "7 · Lifecycle" },
    // 60 s, not 300: the step's own ceiling is 60 s (lifecycle.rs), so a target of five minutes
    // filed a BREACH for behaviour the probe never waited for. The target is the ceiling the probe
    // actually enforces — the bytes themselves are proved weekly by `ws.cross.node`.
    Slo { id: "ws.replicated", feature: "Workspace lifecycle", sli: "A stopped workspace's final sync point reaches a replica, named by that replica", target: bound(60_000), suite: Suite::Fast, stage: "7 · Lifecycle" },
    Slo { id: "ws.start.p95", feature: "Workspace lifecycle", sli: "Starting a workspace completes", target: p95(30_000), suite: Suite::Fast, stage: "7 · Lifecycle" },
    Slo { id: "ws.restore", feature: "Workspace lifecycle", sli: "Restoring a workspace from a past snapshot succeeds", target: avail(99.9), suite: Suite::Fast, stage: "7 · Lifecycle" },
    // The environment twin of the four ids above — the owner's rule is that every workspace SLO
    // has an environment counterpart at the same cadence, because the two control planes converge
    // through different reconcilers and a green workspace says nothing about an environment.
    Slo { id: "env.stop.p95", feature: "Environments", sli: "Stopping an environment completes", target: p95(30_000), suite: Suite::Fast, stage: "7 · Lifecycle" },
    // 30 s for the same reason as `ws.replicated`: that is this step's ceiling.
    Slo { id: "env.replicated", feature: "Environments", sli: "A stopped environment's final sync point reaches a replica", target: bound(30_000), suite: Suite::Fast, stage: "7 · Lifecycle" },
    Slo { id: "env.start.p95", feature: "Environments", sli: "Starting an environment completes", target: p95(60_000), suite: Suite::Fast, stage: "7 · Lifecycle" },
    Slo { id: "env.restore", feature: "Environments", sli: "Restoring an environment from a past snapshot succeeds", target: avail(99.9), suite: Suite::Fast, stage: "7 · Lifecycle" },
    Slo { id: "vol.refusals", feature: "Workspace lifecycle", sli: "Deleting a sync point or a running worktree's base snapshot is refused", target: avail(99.9), suite: Suite::Fast, stage: "7 · Lifecycle" },
    Slo { id: "vol.detached.restorable", feature: "Workspace lifecycle", sli: "A detached volume's snapshot can still be restored", target: avail(99.9), suite: Suite::Fast, stage: "7 · Lifecycle" },
    // `retire_pass` is the rule at BOTH ends, so the SLI names both: the sweep that takes an
    // orphaned tree, and the Volume with no owner entry and no snapshot behind it.
    Slo { id: "vol.orphan.collected", feature: "Workspace lifecycle", sli: "An orphaned volume directory is collected, and a Volume with no owner entry and no snapshot is deleted", target: bound(60_000), suite: Suite::Fast, stage: "7 · Lifecycle" },
    // `cleanup_parent`'s detach-or-keep rule, which is exactly where a lost detach strands bytes
    // nothing on any tier can find again. Both directions, or neither says anything.
    Slo { id: "wt.delete", feature: "Workspace lifecycle", sli: "Deleting a workspace or environment drops the worktree and leaves the volume iff a snapshot remains", target: bound(60_000), suite: Suite::Fast, stage: "7 · Lifecycle" },
    Slo { id: "snap.delete", feature: "Workspace lifecycle", sli: "Deleting a snapshot removes it from history, and the last one of a detached volume takes the volume with it", target: avail(99.9), suite: Suite::Fast, stage: "7 · Lifecycle" },

    // Stage 8 · admin
    Slo { id: "req.queue", feature: "Admin", sli: "A Request CR is queued and answerable by an admin", target: bound(5_000), suite: Suite::Fast, stage: "8 · Admin" },
    Slo { id: "audit.row", feature: "Admin", sli: "Every admin write produces an audit row, and the same write reaches `kloudlite.events` as `admin.<action>`", target: avail(99.9), suite: Suite::Fast, stage: "8 · Admin" },
    // No millisecond bound any more, and the SLI no longer promises freshness in time: the table
    // is TRANSITIONS, and a stable fleet writes none for days — the newest row's age says nothing
    // about whether the evaluator is running. What a reader can honestly assert is that every
    // recorded row carries the timestamp it transitioned at, which is the assertion added here.
    Slo { id: "signals.fresh", feature: "Admin", sli: "Every recorded signal carries the timestamp it transitioned at, and a rule with no covering samples reads `unknown` rather than `ok`", target: avail(99.9), suite: Suite::Fast, stage: "8 · Admin" },
    Slo { id: "history.api", feature: "Admin", sli: "The history API answers a chart query", target: avail(99.9), suite: Suite::Fast, stage: "8 · Admin" },

    // Stage 9 · security
    Slo { id: "sec.private.repo", feature: "Security", sli: "A private repo is unreadable to a non-collaborator", target: avail(100.0), suite: Suite::Fast, stage: "9 · Security" },
    Slo { id: "sec.cross.owner", feature: "Security", sli: "One owner's objects are invisible to another owner", target: avail(100.0), suite: Suite::Fast, stage: "9 · Security" },
    Slo { id: "sec.admin.claim", feature: "Security", sli: "An admin route refuses a token without the superadmin claim", target: avail(100.0), suite: Suite::Fast, stage: "9 · Security" },
    Slo { id: "sec.user.process", feature: "Security", sli: "The ordinary API process has no admin route mounted", target: avail(100.0), suite: Suite::Fast, stage: "9 · Security" },
    // A 100 % id asserts REFUSALS and nothing else: there is no budget here for a positive that a
    // flaky kube transport can fail. The other half — that the two spec writes the ClusterRole
    // does allow still succeed — is `agent.spec.allowed` below, at 99.9 %.
    Slo { id: "sec.agent.spec", feature: "Security", sli: "The admission policy refuses a spec write outside the allowed fields", target: avail(100.0), suite: Suite::Fast, stage: "9 · Security" },
    Slo { id: "id.token.revoked", feature: "Security", sli: "A revoked token is refused", target: avail(99.9), suite: Suite::Fast, stage: "9 · Security" },
    Slo { id: "repo.visibility", feature: "Security", sli: "A repo flipped private is hidden from a non-collaborator, and is hidden again after being flipped back", target: avail(100.0), suite: Suite::Fast, stage: "9 · Security" },
    // The positive halves of the two ids above, split out for the same reason: a repo that will
    // not read when public is an availability failure, not a leak, and neither belongs in a budget
    // that allows no failures at all.
    Slo { id: "repo.visibility.public", feature: "Git hosting", sli: "A repo flipped public becomes readable to another owner", target: avail(99.9), suite: Suite::Fast, stage: "9 · Security" },
    Slo { id: "agent.spec.allowed", feature: "Security", sli: "The spec write the agent's ClusterRole grants — `Volume.spec.restoreTo` — is still admitted", target: avail(99.9), suite: Suite::Fast, stage: "9 · Security" },
    // The git tier's twin of `sec.user.process`: the browse API mounts on the PEER listener only,
    // and a misconfigured listener would put every browse route on the internet with every other
    // SLO green. Refusal-only, so 100 % like the rest of `sec.*`.
    Slo { id: "sec.peer.listener", feature: "Security", sli: "The git tier's public listener refuses `/api/`, on a repo that exists and one that does not", target: avail(100.0), suite: Suite::Fast, stage: "9 · Security" },

    // Stage 10 · edge and pipeline
    Slo { id: "edge.dns", feature: "Edge and pipeline", sli: "The public hostname resolves", target: avail(99.99), suite: Suite::Fast, stage: "10 · Edge" },
    Slo { id: "edge.cert", feature: "Edge and pipeline", sli: "The TLS certificate is valid for the public hostname", target: avail(99.9), suite: Suite::Fast, stage: "10 · Edge" },
    Slo { id: "edge.origin", feature: "Edge and pipeline", sli: "The origin answers a direct HTTP request on its ingress, the way the proxy reaches it", target: avail(99.9), suite: Suite::Fast, stage: "10 · Edge" },
    Slo { id: "edge.ssh.lb", feature: "Edge and pipeline", sli: "The SSH load balancer accepts a connection", target: avail(99.9), suite: Suite::Fast, stage: "10 · Edge" },
    Slo { id: "tel.log.latency", feature: "Edge and pipeline", sli: "A structured log line reaches HyperDX", target: bound(60_000), suite: Suite::Fast, stage: "10 · Edge" },
    Slo { id: "tel.pod.coverage", feature: "Edge and pipeline", sli: "Every pod is scraped by the region's collector", target: bound(60_000), suite: Suite::Fast, stage: "10 · Edge" },
    Slo { id: "tel.stream.lag", feature: "Edge and pipeline", sli: "The Redis events stream consumer lag stays low", target: bound(60_000), suite: Suite::Fast, stage: "10 · Edge" },
    Slo { id: "tel.ch.disk", feature: "Edge and pipeline", sli: "ClickHouse disk usage is reported", target: bound(60_000), suite: Suite::Fast, stage: "10 · Edge" },
    // The two liveness files nothing else watches. Availability, not a millisecond bound: what is
    // being judged is the AGE OF A HEARTBEAT read inside the step, and a bound would have measured
    // how long the read took instead — a fast pod with a dead lane would have passed it.
    Slo { id: "worker.lane.health", feature: "Control plane", sli: "Every worker lane's heartbeat is fresh, counted against the concurrency the liveness probe counts", target: avail(99.9), suite: Suite::Fast, stage: "10 · Edge" },
    Slo { id: "agent.heartbeat", feature: "Control plane", sli: "Every region agent's heartbeat file is fresh and its DaemonSet is fully ready", target: avail(99.9), suite: Suite::Fast, stage: "10 · Edge" },

    // Hourly · Experience. The owner's addendum: every remaining verb a person can perform, walked
    // once an hour on top of the fast journey — so an hourly run is also a fast sample.
    Slo { id: "ws.packages.add", feature: "Workspaces", sli: "Adding a package to a running workspace makes it runnable (`which`)", target: p95(180_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "ws.packages.remove", feature: "Workspaces", sli: "Removing it makes it disappear from the profile", target: p95(120_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "ws.packages.pin", feature: "Workspaces", sli: "A workspace created with `jq@1.7` locks a 1.7.x, and `jq --version` in the pod says so", target: p95(180_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "ws.packages.pin.unknown", feature: "Workspaces", sli: "`jq@0.0.99` is refused with the nearest versions named", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "ws.packages.pin.uncached", feature: "Workspaces", sli: "`nodejs@20.20.2` (EOL, never built) is refused naming a version that has a cached build", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "ws.packages.update", feature: "Workspaces", sli: "`POST …/packages/update` answers with the lock unchanged for an exact pin", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "ws.packages.pin.lockshape", feature: "Workspaces", sli: "The lock names a nixpkgs revision and a store path", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "ws.seeded", feature: "Workspaces", sli: "A workspace created from a repo and branch has that clone checked out, and its doc names them", target: p95(240_000), suite: Suite::Hourly, stage: "14 · Experience" },
    // Build output lives in the workspace dir since 2026-09-11 and travels with a push: a restore
    // arrives warm. Read on the RESTORED copy, never the source.
    Slo { id: "ws.cache.travels", feature: "Workspaces", sli: "A file written under `~/.cache` before a push is present in a workspace restored from that push", target: p95(240_000), suite: Suite::Hourly, stage: "14 · Experience" },
    // The workspace tool server (`kl ide serve`): up inside a fresh pod, and answering the one
    // call every agent makes first. Read from INSIDE the pod, the way the ssh tunnel would.
    Slo { id: "ide.serve.up", feature: "Workspaces", sli: "`kl ide serve` inside a fresh workspace answers /healthz within 240 s of the create", target: p95(240_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "ide.exec", feature: "Workspaces", sli: "An exec through the workspace's own tool API runs as `kl` and answers exit code 0", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    // A POSITIVE signal, and the reason this id exists at all: three outages came from believing
    // the sandbox was on because nothing said it was off. Only a preflight that actually started
    // `bwrap` writes that line (spec §4.7).
    Slo { id: "ide.sandbox.active", feature: "Workspaces", sli: "The workspace's tool server reports `ide.sandbox.active` after the first exec, so execs really are wrapped", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    // What the sandbox's first working roll broke: `/etc` held only passwd and resolv.conf, so no
    // CA bundle was reachable and every HTTPS fetch in every workspace failed at once.
    Slo { id: "ide.exec.https", feature: "Workspaces", sli: "An HTTPS fetch from inside a wrapped exec succeeds, so a sandboxed command can still reach git, npm and cargo", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    // Subagent trees (spec §4.8). All five read through the workspace's own tool server, because
    // the tree only exists as something a session acts on: a subvolume nobody can address is not
    // the thing being probed.
    Slo { id: "ws.tree.cut", feature: "Workspaces", sli: "A tree asked for on a running workspace is ready within 10 s, lists the source's files, and `main` is refused a path under `.agents/`", target: p95(60_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "ws.tree.isolated", feature: "Workspaces", sli: "A file written in a tree is not there in `main`, and neither is one written in `main` there in the tree", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "ws.tree.no_travel", feature: "Workspaces", sli: "A workspace pushed with a tree restores elsewhere with `.agents/{name}` empty", target: p95(290_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "ws.tree.ports", feature: "Workspaces", sli: "An exec in a tree prints a `$PORT` inside that tree's block, and a detached listener on a port `main` holds is marked failed naming it", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "ws.tree.closed", feature: "Workspaces", sli: "A deleted tree is gone from `status.trees` and from disk within one pass, and the workspace still deletes with a live tree on it", target: p95(120_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "ws.seed.failed", feature: "Workspaces", sli: "A workspace seeded from a repository that does not exist reports `SeedFailed` rather than staying `Creating`", target: p95(240_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "key.platform.regenerate", feature: "Identity", sli: "Regenerating the platform key keeps seeding working", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "team.create", feature: "Teams", sli: "A team can be created by a person", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "team.invite.accept", feature: "Teams", sli: "An invite is created, previewed and accepted once", target: bound(5_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "team.role.set", feature: "Teams", sli: "A member's role changes and is reflected in the profile", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "team.repo.shared", feature: "Teams", sli: "A member clones a team repo; a non-member is refused", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "team.workspace", feature: "Teams", sli: "A team workspace lands in the team namespace and starts", target: p95(90_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "team.member.remove", feature: "Teams", sli: "A removed member loses access to the team repo", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "team.member.paused", feature: "Teams", sli: "A paused member's tool token, team `/v1` and bench tunnel and bench session are refused and their bench is stopped within a minute; after unpause and start the bench folder's canary is still there", target: bound(360_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "team.delete", feature: "Teams", sli: "Deleting the team removes its profile and refuses its slug", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "team.namespace.reaped", feature: "Workspaces", sli: "No team namespace outlives by more than two resync beats the workspaces that used it", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
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
    Slo { id: "env.services.patched", feature: "Environments", sli: "A service added with `PATCH /v1/environments/{id}` comes up, and one removed has its StatefulSet deleted", target: p95(180_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "vol.history", feature: "Workspace lifecycle", sli: "History lists pushes newest first with their messages; refs answer", target: bound(1_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "quota.view", feature: "Admin", sli: "`GET /v1/quota` reflects the objects the run holds", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "request.approve", feature: "Admin", sli: "An approved quota request raises the quota and unblocks the refused create", target: bound(60_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "admin.stop.workspace", feature: "Admin", sli: "An admin stop is visible to the owner as `stopped`", target: bound(30_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "superadmin.grant", feature: "Security", sli: "Granting superadmin adds the account to the roster and revoking takes it off", target: avail(100.0), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "feed.experience", feature: "Pull requests", sli: "The feed shows the team and repo events of this run", target: bound(30_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "home.travels", feature: "Workspaces", sli: "A dotfile and a file under ~/workspace written before a push are present in a workspace restored from it, and absent from a fresh workspace of the same owner", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    // The 2026-09-05 coverage review's remaining verbs. Each sits in the Experience stage because
    // its nearest existing twin does — every one of them is a whole flow rather than a request.
    Slo { id: "id.username", feature: "Identity", sli: "A second username claim is refused as already set, and a malformed handle is rejected", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "id.cli.tokens", feature: "Identity", sli: "A CLI token is listed and, once revoked, is refused", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "id.profile.upsert", feature: "Identity", sli: "A profile upsert is saved and read back", target: bound(5_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "id.cli.sshconfig", feature: "Identity", sli: "`kl-connect ws sshconfig` writes a host block naming a running workspace", target: bound(15_000), suite: Suite::Hourly, stage: "14 · Experience" },
    // Strict, and it can be: credential HITS are not cached at all (`crates/storage/src/auth.rs`,
    // `CACHE_TTL` — only misses are, because the cache is per process while the revocation happens
    // in another one), so a removed key stops working on the very next request, fleet-wide.
    // Hourly, not fast: waiting out a resync beat is 330 s of the fast suite's 900 s deadline
    // for one refusal, and `journey` is keyed by stage — so the row moves stage with its suite.
    Slo { id: "key.revoked", feature: "Workspaces", sli: "A removed key is refused by git and by the workspace", target: bound(330_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "key.ssh.lifecycle", feature: "Identity", sli: "A newly added SSH key clones, and after removal the same key is refused at once", target: bound(30_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "repo.description", feature: "Git hosting", sli: "A repo description is saved and read back", target: bound(5_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "pr.merge.strategies", feature: "Pull requests", sli: "Each merge strategy — merge, squash, rebase, fast-forward — lands the expected tree", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "pr.mergeability", feature: "Pull requests", sli: "Mergeability is reported clean for a clean change and dirty for a conflicting one", target: bound(30_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "team.invite.revoke", feature: "Teams", sli: "A revoked invite token is refused", target: avail(100.0), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "team.environment", feature: "Teams", sli: "A team environment lands in the team namespace and its services resolve", target: p95(180_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "env.space.cleared", feature: "Environments", sli: "Clearing a space's environment removes the environment-side policy", target: bound(30_000), suite: Suite::Hourly, stage: "14 · Experience" },
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

    // The 2026-09-06 coverage review's batch. Same rule as the batch above: one id per gap, in the
    // Experience stage when the thing it walks is a whole flow rather than a request.
    Slo { id: "ws.quota.namespace", feature: "Workspaces", sli: "The owner's namespace carries an `owner-quota` matching the effective Quota, and Kubernetes reports it as the hard stop", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "env.services.policies", feature: "Environments", sli: "An owner's namespace carries the OwnerBinding NetworkPolicies", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    // One id over a fixed list, as `admin.screens` does for the API: 26 of 30 page routes had no
    // load SLO, and 26 ids for one Next.js deployment would be 26 samples of the same fact. Each
    // page is timed individually inside the step against 1500 ms, which is why the target is
    // availability rather than a p95 over the whole walk.
    Slo { id: "web.pages", feature: "Web app", sli: "Every page route in the app's fixed list loads, each within 1500 ms", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    // The LOW rows of the review, grouped by the tier that answers them rather than one id per
    // route: each is a single read whose failure is the same failure, and a per-route id would be
    // a catalogue nobody reads.
    Slo { id: "repo.metadata", feature: "Git hosting", sli: "The browse `lastmod` route answers for a commit this run pushed", target: bound(10_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "id.session.reads", feature: "Identity", sli: "The passkey `used` mark stays peer-only, and the legacy quota-request create and the api's own settings read answer", target: bound(10_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "kl.commands", feature: "Identity", sli: "`kl-connect ws`, `kl-connect ws list --team` and `kl-connect logout` answer", target: bound(30_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "admin.reads", feature: "Admin", sli: "`/admin/nodes`, `/admin/settings/schema` and a cluster status write answer, and an unknown history series is a 404", target: bound(10_000), suite: Suite::Hourly, stage: "14 · Experience" },
    // Benches. 480 s is the region's default `benchIdleSecs` (300) plus a 90 s start plus 90 s of
    // reads; a region that raises the knob raises the probe's ceiling with it.
    Slo { id: "bench.idle.wake", feature: "Benches", sli: "With every client gone past `benchIdleSecs` the bench has no pod, a new connection starts it, and the session list and a transcript read back unchanged", target: bound(480_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "bench.session.roundtrip", feature: "Benches", sli: "A session is created, a no-tools prompt answered, and read back from `/sessions/{id}/messages`", target: bound(60_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "bench.tools.own_hands", feature: "Benches", sli: "A bench session's tools are its own workspace's — the seven ide tools plus `process` and `kl_workspace_ask`, on `127.0.0.1:7788`, with pi's builtins off", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "bench.proposal.asked", feature: "Benches", sli: "A bench session's platform write is asked first: the proposal names the change, a no declines it, and nothing is created", target: bound(120_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "bench.exchange.both_views", feature: "Benches", sli: "An exchange reads back by `?session=` and by `?workspace=`", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "bench.two_clients", feature: "Benches", sli: "Two WebSockets on one session see the same events in the same order", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "bench.tool.token", feature: "Benches", sli: "The probe's login mints a tool token and a `/v1/regions` call inside the bench pod answers JSON", target: bound(120_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "bench.tool.audience", feature: "Benches", sli: "The pod's token is refused on `/v1/bench/session`, `/v1/cli/tokens` and `/v1/keys`", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "bench.tool.revoked", feature: "Benches", sli: "After a stop the next call with the pod's token is 401 at once; after the parent login is revoked a pod call is 401 within 60 s", target: bound(90_000), suite: Suite::Hourly, stage: "14 · Experience" },
    // The whole chain: `/v1`'s address, `allow-bench-tools`, the tool server on the pod IP and the
    // thread file.
    Slo { id: "shell.up", feature: "Benches", sli: "A shell sidecar answers on both pod kinds, opens in the home, and cannot see the workspaces root", target: bound(15_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "shell.fenced", feature: "Security", sli: "The shell port refuses a dial from outside the person's own bench", target: avail(100.0), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "shell.no_tools", feature: "Security", sli: "The tool server answers the token-less shell 401", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "bench.no_hands", feature: "Benches", sli: "A bench session asked to run a command calls no tool: there is no filesystem or shell where it runs", target: bound(60_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "bench.pkg_needs_workspace", feature: "Benches", sli: "A package request with no workspace named is refused and proposes nothing", target: bound(60_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "bench.shell.roundtrip", feature: "Benches", sli: "A shell opened on the bench through `/pty` echoes a marker and exits 0", target: bound(15_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "bench.shell.workspace", feature: "Benches", sli: "A shell opened through the bench into the run's workspace starts in the workspace directory, and a named session reattaches to its own scrollback", target: bound(20_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "bench.workspace.tool_roundtrip", feature: "Benches", sli: "A workspace session on the bench runs `exec echo` in a workspace through its tool server, and the turn lands under `/bench/workspaces/{ws}/`", target: bound(180_000), suite: Suite::Hourly, stage: "14 · Experience" },
    // A bench IS a Workspace now, so its transcripts are cut by the ordinary push and its package
    // list is edited from its own shell. Both are group 3's, walked last: a package edit recreates
    // the pod.
    Slo { id: "bench.push.p95", feature: "Benches", sli: "`POST /v1/workspaces/{bench}/push` completes and the volume's history lists the snapshot as ready", target: p95(60_000), suite: Suite::Hourly, stage: "14 · Experience" },
    Slo { id: "bench.pkg.add", feature: "Benches", sli: "a package added through the API lands in the bench's `spec.packages`", target: bound(20_000), suite: Suite::Hourly, stage: "14 · Experience" },
    // The whole subagent lifecycle as a person drives it, from the bench: the tree and the session
    // STAY after the report — nothing is dropped on completion — and only a close takes them.
    Slo { id: "agent.tree.run", feature: "Benches", sli: "A dispatched agent gets a tree, reports, and leaves both standing; closing it deletes the tree and archives the session", target: p95(300_000), suite: Suite::Hourly, stage: "14 · Experience" },
    // The whole delegation chain end to end: top opens a main by workspace, sends it a
    // `delegate to` instruction, the main hands it to a sub in a cloned workspace, and the
    // sub's push lands back on main's branch.
    Slo { id: "bench.delegate", feature: "Benches", sli: "top → main → sub: the push lands on main's branch, the clone is gone, the child is closed", target: bound(600_000), suite: Suite::Hourly, stage: "14 · Experience" },

    // Weekly
    Slo { id: "git.push.large", feature: "Git hosting", sli: "Push of a large commit succeeds — 90 MiB over HTTP, under Cloudflare's 100 MB upload cap, and 100 MiB over SSH, which has no proxy in front of it", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "reg.push.large", feature: "Container registry", sli: "Pushing a large image layer succeeds", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "ws.cold.profile", feature: "Workspaces", sli: "A cold package profile builds successfully", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "ws.profile.reuse", feature: "Workspaces", sli: "A repeat package set is published from the profile index, not rebuilt", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "ws.cross.node", feature: "Workspaces", sli: "A workspace started on a peer node reads its replica correctly", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "homes.cross.node", feature: "Workspaces", sli: "The shared home is consistent across nodes", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "env.cross.node", feature: "Environments", sli: "An environment started on a peer node reads its replica correctly", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "cp.failover", feature: "Control plane", sli: "The leader lease fails over to another pod", target: bound(30_000), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "settings.live", feature: "Control plane", sli: "A live settings change takes effect on the next beat", target: bound(60_000), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "settings.revert", feature: "Control plane", sli: "Reverting to a stored settings version restores it", target: bound(60_000), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "settings.roll", feature: "Control plane", sli: "A Boot-marked save is refused with 409 while one of its readers is mid-rollout, and nothing is written", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    // Weekly, and only the KEEP-BIASED half: `BLOB_GRACE` is a fixed hour and the weekly CronJob's
    // own `activeDeadlineSeconds` is 3600, so no run can watch an unreferenced blob be reclaimed
    // in-band. What it CAN prove is the rule the sweep is written around — a sibling's layer
    // survives a delete — which is the failure that loses somebody's image.
    Slo { id: "reg.gc.sweep", feature: "Container registry", sli: "A blob a sibling image still references survives that image's deletion and a GC pass", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },

    // The deploy the owner actually worries about, and the one event the fast suite is designed to
    // yield through — which is why it has to be a drill of its own rather than a fast sample.
    Slo { id: "roll.zero.errors", feature: "Control plane", sli: "A rolling restart of the srv tier lands with no failed answer on a concurrent push and pull loop — a 421 the router recovers is not one — and every pod that left logged `ownership.drained`", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "srv.drain.handover", feature: "Control plane", sli: "A drained pod reports `draining` on `/healthz` and its repos are served by a live peer", target: bound(30_000), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "reg.moved.image", feature: "Container registry", sli: "The first pull of an image whose database has just moved nodes succeeds", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "reg.blob.session", feature: "Container registry", sli: "A chunked upload resumes and completes, a cancelled session is gone, a deleted blob 404s and referrers answers for a pushed manifest", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "git.gc.packs", feature: "Git hosting", sli: "After a push and a consolidation pass the repo still clones to the same tree and its index markers still list it", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    // Only the manifest ceiling is testable in band: `max_layer` is 5 GiB and the git `max_body`
    // 2 GiB, and a probe that sent either would be measuring the CronJob's disk. What this catches
    // is the failure people have actually hit — the three limits collapsing into one.
    Slo { id: "reg.limits", feature: "Container registry", sli: "A manifest over its own limit is refused 413 while a blob of the same size is accepted — the two ceilings are different knobs", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "gw.caps", feature: "Workspaces", sli: "The gateway refuses a tunnel past its per-workspace cap and keeps the ones already open", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "admin.workload.roll", feature: "Admin", sli: "A roll of one reader restarts exactly that workload and it returns ready", target: bound(180_000), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "ws.spread", feature: "Workspaces", sli: "A stopped workspace whose volume nothing holds comes back on a different node", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "snap.retain", feature: "Workspace lifecycle", sli: "After several sync beats exactly one Ready sync point per worktree remains and every push is still in history", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "agent.janitor", feature: "Workspaces", sli: "No snapshot record of this run outlives the volume it names", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "srv.lanes", feature: "Control plane", sli: "Pulls of an image reach its pull counter, which is the server lane beat writing it back", target: avail(99.9), suite: Suite::Weekly, stage: "12 · Weekly" },
    Slo { id: "bench.survives.reschedule", feature: "Benches", sli: "After the pod is deleted every session reopens and processes read `lost`", target: bound(180_000), suite: Suite::Weekly, stage: "12 · Weekly" },

    // Monthly
    Slo { id: "bak.tarball.age", feature: "Backups", sli: "The latest backup tarball is recent", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    Slo { id: "bak.daily.slots", feature: "Backups", sli: "Every daily backup slot is present", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    Slo { id: "bak.versioning", feature: "Backups", sli: "Backup versioning is enabled and retains history", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    Slo { id: "bak.cosmos", feature: "Backups", sli: "The Cosmos backup for HyperDX succeeds", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    // NOT AUTOMATED, and the SLI says so rather than the id vanishing: "dead" is the NODE's Ready
    // condition going non-True for `WS_NODE_DEAD_SECS`, which cannot be produced from inside the
    // cluster — a taint evicts pods and leaves the node Ready. The probe files a skip naming the
    // operator recipe in deploy/k3s/README.md, so the console shows "not automated" rather than a
    // green row nothing produced. Same for the two ids below it.
    Slo { id: "drill.dead.node", feature: "Resilience drills", sli: "A dead-node drill heals every replica onto a live node — walked by the operator's node-level drill, not by the probe", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    Slo { id: "drill.drain", feature: "Resilience drills", sli: "A drain of the node holding a running worktree stamps the node draining and leaves that worktree's pod running", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    Slo { id: "drill.redis.down", feature: "Resilience drills", sli: "The system keeps operating correctly with Redis down", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    Slo { id: "cluster.decommission", feature: "Resilience drills", sli: "A decommission is refused until the agent stamps `drained`, then cordons the node", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    Slo { id: "drill.clickhouse.down", feature: "Resilience drills", sli: "With ClickHouse denied, every /v1 verb still works and `/admin/history/*` answers 503, never 500", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    // Both are REFUSALS, so 99.9 % rather than 100 %: only `sec.*` spends no budget at all, and a
    // 409 that never came because the api was down is an availability failure like any other.
    Slo { id: "ws.interrupted", feature: "Workspace lifecycle", sli: "Starting a workspace whose node is down is refused with the sentence naming why, and a clone of it names the cut it grafted onto — walked by the operator's node-level drill", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    Slo { id: "env.clone.interrupted", feature: "Environments", sli: "Cloning an environment whose node is down is refused with 409 — walked by the operator's node-level drill, since there are no live bytes to copy", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    Slo { id: "team.member.removed.cleanup", feature: "Teams", sli: "Within 11 minutes of delete-now the controller's GC deletes a removed member's bench, team workspace, sync points and space choice (with memberRemovalDeletes off they carry a due delete-after and stay), the pushed snapshot and its volume are kept, the removal is audited, and a re-added person finds no bench", target: avail(99.9), suite: Suite::Monthly, stage: "13 · Monthly" },
    // `team.member.removed.dir_down` is MANUAL in `deploy/slo.md`, deliberately not catalogued: the
    // drills suite has no hook that points the api's directory at a black hole, so the probe could
    // only ever skip — and a skipped id reads as a pass in attainment (the 2026-09-09 incident).
    // Catalogue it the day the fault hook exists.
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The four hourly groups together cover every hourly id exactly once, and each covers only
    /// its own: an id in no group would never be walked and read as passed, and one in two groups
    /// would have a sibling's skip overwrite its real sample.
    #[test]
    fn the_hourly_groups_partition_the_journey() {
        let mut want: Vec<&str> = journey(Suite::Hourly).into_iter().flat_map(|(_, ids)| ids).collect();
        let mut seen: Vec<&str> = (0..HOURLY_GROUPS)
            .flat_map(|g| {
                let stages = journey_for_group(Suite::Hourly, g);
                assert!(stages.iter().any(|(_, ids)| !ids.is_empty()), "group {g} walks nothing");
                stages.into_iter().flat_map(move |(_, ids)| {
                    assert!(ids.iter().all(|id| group_of(id) == g), "group {g} holds a sibling's id");
                    ids
                })
            })
            .collect();
        want.sort_unstable();
        seen.sort_unstable();
        assert_eq!(seen, want, "the groups do not cover the hourly catalogue exactly once");
        // Every stage of the journey still renders, ids or not — a run that died at boot must not
        // read as a run that never started.
        let names: Vec<&str> = journey_for_group(Suite::Hourly, 3).into_iter().map(|(n, _)| n).collect();
        assert!(names.contains(&"0 · Boot") && names.contains(&"11 · Teardown"));
        // …but a stage none of whose ids are this group's is dropped, so the console counts the
        // stages this pod actually walked rather than three quarters of skips.
        assert!(!names.contains(&"1 · Identity"), "group 3 walks no Identity id");
        assert!(names.len() < journey(Suite::Hourly).len());
        // Only hourly is grouped; asking for a group of another suite is the whole journey.
        assert_eq!(journey_for_group(Suite::Fast, 3), journey(Suite::Fast));
    }

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

    #[test]
    fn every_bench_id_is_catalogued() {
        for id in ["bench.create", "bench.start.p95", "bench.tunnel", "bench.idle.wake",
                   "bench.session.roundtrip", "bench.exchange.both_views", "bench.two_clients",
                   "bench.survives.reschedule", "bench.workspace.tool_roundtrip",
                   "bench.tool.token", "bench.tool.audience", "bench.tool.revoked",
                   "bench.shell.roundtrip", "bench.shell.workspace",
                   "bench.push.p95", "bench.pkg.add", "agent.tree.run", "bench.delegate"] {
            assert!(find(id).is_some(), "{id} missing from CATALOGUE");
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
            assert!(s.id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_'), "{}", s.id);
            assert!(s.target.good_pct > 0.0 && s.target.good_pct <= 100.0);
        }
    }
}
