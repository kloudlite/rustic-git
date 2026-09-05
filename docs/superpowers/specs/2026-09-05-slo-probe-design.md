# SLO probe: a synthetic user, every five minutes, reported in the superadmin console

**Status:** design, approved in conversation on 2026-09-05. **Depends on:** the history layer
(`crates/workspaces/src/history/`, ClickStack on AKS) and the superadmin console v2.

## Problem

`deploy/alerts.md` watches symptoms — 5xx ratios, lease renewals, Azure's own availability
numbers. None of it answers the question the owner actually asks: *can a person, right now, push
a repo, open a workspace, ssh into it, push a snapshot and get it back?* Eleven of the hundred
SLOs in the catalogue (`deploy/slo.md`, the visual is the "Kloudlite SLOs" artifact) have an
alert; the other eighty-nine are only observable by doing the thing. The owner wants to know
first, before any user does.

## Decision, in one paragraph

A new binary, `kloudlite-slo`, walks one synthetic user's whole day — identity, git over
HTTP and SSH, a pull request, the registry, a workspace, an environment, the lifecycle verbs,
the admin queue, the security refusals, the edge, the telemetry pipeline — as a Kubernetes
`CronJob` on AKS every five minutes, with a weekly and a monthly suite that add the heavy checks
and the resilience drills. Every step is timed and judged against the catalogue, which lives in
Rust (`crates/workspaces/src/slo/catalogue.rs`) with `deploy/slo.md` held equal to it by a
test, exactly as `deploy/alerts.md` is held to `history::alerts`. The probe reports **while it
runs** — a `PUT` after every stage — to the admin process, the only writer of the `kloudlite`
database, which stores runs and per-SLI results in ClickHouse, computes 30-day attainment,
error budget and burn rate, and evaluates two new rules in the existing 30 s evaluator:
`SloProbeMissing` and `SloBurn`. The console gets a ninth area, **SLOs**: what is running now
step by step, every SLO's budget, the last twenty runs, and one run's detail with the failing
step's message. A firing `SloBurn` reaches the owner the way every other rule does (HyperDX
alert → webhook) and, additionally, the admin process posts one line to
`KLOUDLITE_SLO_WEBHOOK` on every run failure, so a broken journey is a message within the
same five minutes it broke.

Not chosen: a shell script in a `busybox` CronJob (no structured logs, no shared JWT/HTTP code,
no typed catalogue); pushing results as Prometheus metrics (a five-minute Job is never scraped
reliably, and the admin process would still have to fold them); a third-party synthetic
monitor (cannot exec into a workspace pod or exercise the gateway with a platform key); a
separate ClickHouse writer (the single-writer rule for `kloudlite.*` stays).

## Components

| Component | Where | What it does |
| --- | --- | --- |
| Catalogue | `crates/workspaces/src/slo/catalogue.rs`, `deploy/slo.md` | Every SLO: `id`, feature, SLI text, `Target { good_pct, max_ms }`, suite. One test holds the two equal. |
| Probe | `bins/slo` → `kloudlite-slo`, image `ghcr.io/kloudlite/kloudlite-slo` | Runs a suite; one stage at a time; reports after each stage; tears down always. Subcommands `run --suite fast|weekly|monthly`, `bootstrap`. |
| Store | `history::schema` migrations 8 and 9: `kloudlite.slo_runs`, `kloudlite.slo_results` | `ReplacingMergeTree`, 400 d TTL, keyed so a re-sent report is idempotent. |
| Admin API | `crates/workspaces/src/api/admin/slo.rs` | `PUT /admin/slo/runs/{id}` (the probe's report), `GET /admin/slo`, `GET /admin/slo/runs`, `GET /admin/slo/runs/{id}`. |
| Rules | `history::alerts` catalogue, `deploy/alerts.md` | `SloProbeMissing`, `SloBurn`. Tier `Central`. |
| Notify | `history::notify` | One JSON line to `KLOUDLITE_SLO_WEBHOOK` on run failure and on a `SloBurn` transition. Optional; unset means nothing. |
| Console | `web/apps/web/src/app/(shell)/superadmin/slo/` | Area nine. Running-now tracker, SLO table with budgets, runs table, run detail. Overview tile. Fixtures. |
| Deploy | `deploy/kloudlite.yaml` | Three `CronJob`s, ServiceAccount + Role, Secret `kloudlite-slo`, `Quota` CRs for the two probe owners (applied on k3s). |

## The catalogue

Every SLO reduces to *good over total* in a rolling 30-day window; a latency SLO is "good"
when the step succeeded **and** took at most `max_ms`. That is the only shape, so budgets,
burn rates and the table are one code path.

```rust
pub struct Slo {
    pub id: &'static str,        // "git.push.ok", "ws.create.p95" — stable, the ClickHouse key
    pub feature: &'static str,   // "Git hosting"
    pub sli: &'static str,       // the catalogue's SLI column, verbatim
    pub target: Target,
    pub suite: Suite,            // Fast | Weekly | Monthly — which run produces samples
}
pub struct Target { pub good_pct: f64, pub max_ms: Option<u32> }
pub enum Suite { Fast, Weekly, Monthly }
pub const CATALOGUE: &[Slo] = &[ … ];
```

`deploy/slo.md` is the human table: `| id | Feature | SLI | Target | Suite | Journey step |`.
`the_catalogue_matches_deploy_slo_md` parses the table and compares ids, targets and suites.
An SLO that exists in the visual but is not probed (email link, passkey registration, the
backup restore drill) is in the table with suite `manual` and no id, and never in `CATALOGUE`.

## The probe

**Identity.** The probe mounts the `kloudlite-jwt` Secret and mints its own tokens with
`kloudlite_core::jwt`: a user token for `slo-probe`, one for `slo-other` (the second tenant),
and an admin token (`mint_admin(.., superadmin: true)`) used only for `/admin/*` calls. It never
holds a password, an Azure credential or a HyperDX key. `bootstrap` (a one-off Job, idempotent)
claims the two usernames through `POST /v1/users/username`, pushes the registry canary image
`slo-probe/canary` if it is missing, and registers nothing else — the Quota CRs are yaml.

**Shape.** `Suite` is an ordered list of `Stage`s; a stage is `fn(&mut Ctx) -> Vec<Step>` where
`Step { id: &'static str, ok: bool, ms: u32, detail: String, skipped: bool }`. A step whose
precondition failed earlier (no workspace to ssh into) reports `skipped: true` with the reason;
skipped is **no sample**, never a failure — the failure was already counted where it happened.
Every step has a timeout (`Ctx::deadline`, default 120 s, per-step overrides for create/wait
steps). The last stage is teardown and runs unconditionally, including after a panic: the release
profile aborts on panic, so the stages run in a child process (`run --inner`, a re-exec of the
same binary) and the parent tears down and files the final report whatever the child's exit. Run ids are `{suite}-{unix seconds}`.

**Reporting while running.** After every stage the probe `PUT`s the whole run so far —
`{ run_id, suite, region, started, finished: null, state: "running", stage: "5 · Workspace",
steps: [...] }` — to `/admin/slo/runs/{id}`; the final `PUT` carries `finished` and
`state: passed|failed`. The admin process upserts; a lost `PUT` is repaired by the next one.
Ten seconds of `AutoRefresh` on the console is therefore a live step tracker.

**Tools in the image.** `git`, `openssh-client`, `crane` (pinned release, checksum in the
Dockerfile), `kubectl` (pinned), `kl` (built in the same `cargo build`), `openssl`,
`dig` (`bind9-dnsutils`), `curl`. Debian bookworm, uid 1001, read-only root, `/tmp` emptyDir
for the git and crane working trees.

**Access.** ServiceAccount `kloudlite-slo` in ns `kloudlite` with a Role limited to
`pods: get, list, delete` (the leader failover drill) and a mounted copy of the k3s kubeconfig
Secret the api tier already has (exec into workspace pods, node taint/label for the drills,
`auth can-i` as the agent SA). The gateway is reached through Cloudflare like a user's `kl`.

**Stages, fast suite** — the journey artifact is the reference; the ids are the catalogue's:

0 boot · 1 identity (`id.signin`, `id.token.mint`, `id.key.usable`, `id.cli.flow`,
`id.jwt.tiers`) · 2 git (`git.push.ok`, `git.push.p95`, `git.clone.ok`, `git.clone.p95`,
`ssh.clone.ok`, `ssh.hostkey`, `ssh.unregistered.refused`, `browse.p95`, `browse.commit.visible`,
`web.repo.page`) · 3 pull request (`pr.merge.p95`, `feed.latency`) · 4 registry
(`reg.token.p95`, `reg.push.ok`, `reg.manifest.p95`, `reg.tags.visible`, `reg.shared.layer`,
`reg.canary`, `reg.visibility`) · 5 workspace (`ws.create.p95`, `ws.exec.ok`, `homes.rw.p95`,
`gw.tunnel.p95`, `gw.unregistered.refused`, `ws.push.p95`, `ws.clone.p95`, `quota.refused`)
· 6 environment (`env.create.p95`, `env.dns`, `env.attach`, `env.detach`, `env.push.p95`)
· 7 lifecycle (`ws.stop.p95`, `ws.replicated`, `ws.start.p95`, `ws.restore`, `vol.refusals`,
`vol.detached.restorable`, `vol.orphan.collected`) · 8 admin (`req.queue`, `audit.row`,
`signals.fresh`, `history.api`) · 9 security (`sec.private.repo`, `sec.cross.owner`,
`sec.admin.claim`, `sec.user.process`, `sec.agent.spec`, `id.token.revoked`) · 10 edge and
pipeline (`edge.dns`, `edge.cert`, `edge.origin`, `edge.ssh.lb`, `tel.log.latency`,
`tel.pod.coverage`, `tel.stream.lag`, `tel.ch.disk`) · 11 teardown · 12 report.

Weekly adds `git.push.large`, `reg.push.large`, `ws.cold.profile`, `ws.profile.reuse`,
`ws.cross.node`, `homes.cross.node`, `cp.failover`, `settings.live`. Monthly adds
`bak.tarball.age`, `bak.daily.slots`, `bak.versioning`, `bak.cosmos`, `drill.dead.node`,
`drill.drain`, `drill.redis.down`. `vol.orphan.collected` waits up to five minutes and is the
long pole of the fast suite; the fast suite's `activeDeadlineSeconds` is 900 (a slow run skips the next tick under `Forbid`).

## The store and the maths

```sql
CREATE TABLE kloudlite.slo_runs (
  run_id String, suite LowCardinality(String), region LowCardinality(String),
  started DateTime64(3), finished Nullable(DateTime64(3)),
  state LowCardinality(String),           -- running | passed | failed
  stage String, steps_total UInt16, steps_failed UInt16, failed_step String, failed_detail String,
  updated DateTime64(3)
) ENGINE = ReplacingMergeTree(updated) ORDER BY (run_id) TTL toDateTime(started) + INTERVAL 400 DAY;

CREATE TABLE kloudlite.slo_results (
  run_id String, slo_id LowCardinality(String), ts DateTime64(3),
  ok UInt8, ms UInt32, skipped UInt8, detail String, stage LowCardinality(String), updated DateTime64(3)
) ENGINE = ReplacingMergeTree(updated) ORDER BY (slo_id, ts, run_id)
  TTL toDateTime(ts) + INTERVAL 400 DAY;
```

`ts` is the step's own timestamp from the probe, never insert time, so a re-sent report
collapses. Every reader queries `FINAL`.

Per SLO over a window: `total = countIf(skipped = 0)`, `good = countIf(ok = 1 AND (max_ms IS
NULL OR ms <= max_ms))`, `attainment = good / total`, `budget = (1 - good_pct/100) * total`,
`budget_left = budget - (total - good)`, `burn(w) = ((total_w - good_w) / total_w) /
(1 - good_pct/100)`. `state` per SLO: `unknown` (no sample in 2 × the suite's period), then `burning` (a burn pair fires, below),
then `breaching` (attainment < target over 30 d), else `ok` — burning outranks breaching because
it is the newer fact. `SloStatus` reports `burn_short`/`burn_long` with their window lengths;
weekly and monthly SLOs have only the long pair.

**`SloBurn`** — multiwindow, multi-burn-rate (Google SRE Workbook ch. 5): fire when
`burn(1h) > 14.4 AND burn(5m) > 14.4`, or `burn(6h) > 6 AND burn(30m) > 6`, per `slo_id`.
With one sample per five minutes a single failure of a 99.9 % SLO already trips the fast
pair — that is the intent: the owner hears about the first failure, and the 6 h pair keeps
paging while it persists. Weekly and monthly SLOs are evaluated with the same formula over
their own periods (windows 4 w / 1 w and 6 m / 2 m). **`SloProbeMissing`** — no `slo_runs`
row with `suite = 'fast'` and `finished` in the last 15 minutes; a stuck or unscheduled
probe is itself the page. Both write transitions to `kloudlite.alerts` and show in Signals.

## Admin API

| Route | Who | Body / answer |
| --- | --- | --- |
| `PUT /admin/slo/runs/{id}` | probe (superadmin claim, like every `/admin/*`) | `RunReport` → 204. Upserts the run row and every step as a result row. Validates `id` as `{suite}-{digits}`, steps ≤ 200, `slo_id` ∈ `CATALOGUE`. On `state: failed` posts the notify line. |
| `GET /admin/slo` | console | `{ slos: [{ id, feature, sli, target, suite, attainment_30d, budget_left, burn_1h, burn_6h, last: { ts, ok, ms }, state }], running: Run \| null, runs: [Run; ≤20], generated }` |
| `GET /admin/slo/runs?suite=&limit=` | console | `[Run]`, newest first, ≤100 |
| `GET /admin/slo/runs/{id}` | console | `Run` plus `steps: [{ slo_id, stage, ok, ms, skipped, detail, ts }]` in probe order |

`Run` is `{ run_id, suite, region, started, finished, state, stage, steps_total, steps_failed,
failed_step, failed_detail, duration_ms }`. No ClickHouse → every read is `503 history
unavailable` and the console renders the flat placeholder; the probe's `PUT` answers 503 too
and the probe retries thrice then logs `slo.report.failed` and exits non-zero (the Job shows
red in `kubectl`, and `SloProbeMissing` fires on the gap).

Every caller-shaped value in the SQL goes through `series::ident` or a typed parameter;
`slo_id` is checked against the catalogue before it reaches a query.

## Console

`/superadmin/slo` (nav entry between Monitoring and Audit; `superadmin-nav.ts`):

- **KPI strip**: Running now (stage name or "idle, last run 3 m ago"), Runs failed today,
  SLOs burning, Lowest budget (`git.push.ok 12 % left`).
- **Running now** `Section`: the in-flight run's stages as a horizontal tracker, one chip per
  step (green, red, grey for skipped, pulsing for the current stage); hidden when idle.
- **SLOs** `Section`: table grouped by feature — SLI, target, 30 d attainment, budget bar
  (`ui/capacity`), burn 1 h / 6 h, last result and age, state pill. Sorted burning → breaching →
  unknown → ok. Row click expands the last ten samples.
- **Runs** `Section`: last twenty — suite, started, duration, state pill, failed step and its
  detail truncated, link to the run page.
- `/superadmin/slo/runs/[id]`: every step in order with ms and detail, the stage headers from
  the journey, and "Open in HyperDX" pre-filtered to `service.name:kloudlite-slo run_id:…`
  when `KLOUDLITE_HYPERDX_URL` is set.
- Overview gets one tile, "SLO", value = SLOs burning, sub = last run state and age, `href`
  to the area; the Overview's attention list gets `slo.failed` and `slo.burning` kinds.
- `AutoRefresh` 10 s on the area page, like Monitoring. Fixtures in `lib/fixtures/superadmin.ts`
  for all three reads; `scripts/superadmin-screens.mjs` gains the two screens.

## Deploy

- `image.yml`: the `slo` Dockerfile stage → `ghcr.io/kloudlite/kloudlite-slo:{sha}`;
  `deploy/pin.sh` rewrites it with the other Rust images.
- `deploy/kloudlite.yaml`: `CronJob/slo-fast` `*/5 * * * *`, `concurrencyPolicy: Forbid`,
  `activeDeadlineSeconds: 900`, `successfulJobsHistoryLimit: 3`, `failedJobsHistoryLimit: 5`;
  `slo-weekly` `0 2 * * 0`; `slo-monthly` `0 3 * * 0` with the binary exiting 0 and doing
  nothing when the day of month is past 7. All three: `KLOUDLITE_LOG_FORMAT=json`,
  `prometheus.io/scrape: "false"`, `restartPolicy: Never`, `backoffLimit: 0`.
- Env (the names `bins/slo/src/config.rs` reads are the contract): `KLOUDLITE_ADMIN_API_URL`,
  `KLOUDLITE_API_URL`, `KLOUDLITE_WEB_URL`, `KLOUDLITE_URL` (git over HTTP),
  `KLOUDLITE_SLO_REGISTRY`, `KLOUDLITE_SLO_SSH_HOST`, `KLOUDLITE_SLO_REGION` (the k3s
  region the workspace goes to), `KLOUDLITE_SLO_HOSTS` (comma list for the edge checks),
  `KLOUDLITE_JWT_SECRET`, `KUBECONFIG`; optional `KLOUDLITE_SLO_SSH_HOSTKEY` and
  `KLOUDLITE_SLO_CANARY_DIGEST` (unset → the dependent ids skip). Shared across the three
  CronJobs through `ConfigMap/kloudlite-slo-env`.
- Secret `kloudlite-slo`: `ssh_key` (the probe user's private key, generated once by the
  operator, its public half registered by the run itself), nothing else.
- Admin Deployment: `KLOUDLITE_SLO_WEBHOOK` optional.
- k3s: `Quota/slo-probe` (`workspaces: 1, environments: 1, snapshots: 10, diskGb: 5`),
  `Quota/slo-other` (all zero), applied by hand per `deploy/k3s/README.md`.
- `deploy/alerts.md` gains the two rows; `deploy/RECOVERY.md` notes that the probe is the
  first thing to read after a rebuild.

## Error handling

- A step's own failure is a sample, not an abort. A stage aborts only when its state is
  unusable (workspace never became Ready → the rest of stage 5 and 7 are `skipped`).
- Teardown deletes by **name prefix** (`run-{id}`), so a crashed previous run's leftovers are
  swept by the next run before it starts, and a run never deletes another's live objects.
- The probe never touches an object it did not create, except in the drills, which are
  bounded: the leader pod (recreated by the StatefulSet), one idle node's taint/label
  (removed in the same run, and by the next run if the last one died), one NetworkPolicy
  named `slo-drill-redis` (same rule).
- Report retry: three attempts, 2 s apart; then exit 3.
- `bootstrap` is safe to re-run.

## Testing

- Unit: catalogue ↔ `deploy/slo.md`; budget and burn maths on fixed samples; `RunReport`
  validation (bad id, unknown `slo_id`, too many steps); SQL builders for every read against
  a ClickHouse-syntax snapshot.
- Probe: each stage has a `dry` mode test where the HTTP client is a recorded stub
  (`httpx` test double) and the step list and skip logic are asserted; the timing code is unit
  tested with a fake clock.
- Web: `bun test` on the budget formatting and the tracker's stage grouping; typecheck; the
  screenshot harness renders both screens from fixtures.
- Live: one manual `kubectl create job --from=cronjob/slo-fast` on dev, all steps green,
  before the CronJob is enabled; then `SloProbeMissing` verified by suspending the CronJob.

## Not doing

Email sign-in and passkey registration (need an inbox / an authenticator). Per-region probe
pods (one probe on AKS reaches every region through the api; a second region gets a second
`CronJob` with a different `KLOUDLITE_SLO_REGION`, no code). Public status page. Alert
routing beyond the one webhook. Node SSH.
