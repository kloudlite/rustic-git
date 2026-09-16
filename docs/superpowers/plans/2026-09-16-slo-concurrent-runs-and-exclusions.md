# SLO console: concurrent runs and incident exclusions

> **For agentic workers:** implemented task by task by Opus implementers dispatched from the
> main session, which reviews each task's diff itself. Steps use `- [ ]` for tracking.

**Goal:** the superadmin SLO screens show every run in flight (the hourly suite is four parallel
group runs now), and a superadmin can exclude an acknowledged incident window from the budgets
without hiding the raw numbers.

**Architecture:** the admin API stops assuming one run in flight (`running: Vec<Run>`), learns the
probe's group partition through the shared catalogue, and gains a `kloudlite.slo_exclusions` table
that the budget/burn SQL anti-joins. The web folds sibling group runs into one Job, gives `yielded`
and `lost` their own states, and offers "Exclude this window…" with a required note.

**Spec:** none — owner-approved design in chat, 2026-09-16 ~11:45 IST (this file is the record).

## Global constraints

- `kloudlite` database: the admin process is its only writer; migrations are numbered
  `CREATE … IF NOT EXISTS`, never edit one that shipped (`crates/workspaces/src/history/`).
- Every admin write goes through `crate::audit::record`.
- Every caller-shaped value that reaches SQL goes through an allow-list or `ident()`; there are no
  bound parameters on that path.
- Web: copy existing siblings (`superadmin/ui/*`, `slo/runs-table.tsx`), tokens over raw colours,
  `--radius: 0`. Fixtures must keep `superadmin.test.ts` "row for row with deploy/slo.md" green.
- Wire names = Rust field names (no `rename_all`).
- Gates per task: `cargo clippy --workspace --all-targets -- -D warnings`; the crate's `--lib`
  tests; web `bun run lint && bun run typecheck && bun run test --force` when web is touched.
- Commits: imperative sentence case, no attribution; push to `platform desktop-login` only.

---

### Task 1: the API sees every run in flight, and groups

**Files**
- Modify: `crates/workspaces/src/slo/catalogue.rs` — add `HOURLY_GROUPS`, `group_of(id) -> u8`,
  `journey_for_group(suite, group) -> Vec<(stage, ids)>` (the hourly journey filtered to the ids
  whose `group_of` matches; other suites: whole journey for group 0).
- Modify: `bins/slo/src/suite.rs` — `HOURLY_GROUPS`, `BENCH_IDS`, `group_of` become re-exports/uses
  of the catalogue's; test `the_hourly_groups_partition_the_journey` moves to the catalogue.
- Modify: `crates/workspaces/src/history/slo.rs` — `running_sql` drops `LIMIT 1` (order by
  `started`), returns `Vec<Run>`; add display state `lost`: in `RUN_COLS`, select
  `if(state = 'running' AND updated < now() - INTERVAL 30 MINUTE, 'lost', state) AS state`;
  `RunState::Lost` variant (`as_str` "lost"); `Run` gains `group: Option<u8>` parsed from the id
  (`-g{n}` suffix) in Rust, not SQL.
- Modify: `crates/workspaces/src/api/admin/slo.rs` — `Overview.running: Vec<Run>`; `run_detail`
  slices `journey` with `journey_for_group(suite, run.group.unwrap_or(0))`.
- Tests: `history/slo.rs` inline (`running_sql` has no `LIMIT`, `lost` expression present, id
  parse `hourly-1-g3 → Some(3)`, `fast-1 → None`); catalogue partition test; a `run_detail`
  test that a `-g3` run's journey holds only bench ids.

**Interfaces produced:** `GET /admin/slo` → `running: Run[]`; `Run.group?: number`;
`Run.state` ∈ running|passed|failed|yielded|skipped|lost; `GET /admin/slo/runs/{id}.journey`
is the group's slice.

- [ ] catalogue gains the partition + test (moved, not duplicated)
- [ ] probe uses the catalogue's partition; `cargo test -p kloudlite-slo` green
- [ ] history: `running` is a list, `lost`, `group`
- [ ] api: `Overview.running: Vec<Run>`, sliced journey; `crates/workspaces/tests/api_admin_slo.rs`
      still green
- [ ] commit

### Task 2: the web shows Jobs, not "the run"

**Files**
- Modify: `web/apps/web/src/lib/api/admin.ts` — `SloRunState` adds `"lost"`; `SloRun.group?:
  number`; `SloOverview.running: SloRun[]`.
- Modify: `web/apps/web/src/lib/slo.ts` — add `jobsOf(runs): SloJob[]` where `SloJob = { key,
  suite, region, started, runs: SloRun[], state }`: runs of one suite whose `started` are within
  15 min form one job (the probe's sibling rule, `SIBLING_WINDOW_SECS = 900`); a run with no
  group is a job of one. Job state: failed if any failed; running if any running; lost if any
  lost and none running; yielded if all yielded; else passed (skipped counts as passed here, as
  the probe does). `runTone`: `yielded` → "muted", `lost` → "warn", `running` → "info".
- Modify: `slo/page.tsx` — "Running now" KPI: `running.length` runs across `jobsOf(running).length`
  jobs; the panel becomes one `JobCard` per running job (suite · `k of n groups done` · one
  `StageRow`-style line per group: group number, stage, progress, state); when idle, the last
  finished job.
- Modify: `slo/runs-table.tsx` — one row per job; sibling runs shown as group chips (`g0 ✓ g1 ● g2 ✗
  g3 ○`), expandable to the per-run rows that exist today; glyph per state incl. yielded (hollow
  dot) and lost (dash).
- Modify: `slo/run-tree.tsx` — header shows `group n of 4` when `run.group` is set; the sliced
  journey from Task 1 makes stage counts right without further change.
- Modify: `superadmin/page.tsx` — `lastRun` → last finished job.
- Modify: `web/apps/web/src/lib/fixtures/superadmin.ts` — `runId` gains an optional group; add a
  running hourly job of four group runs (g0 running at stage 6, g1 passed, g2 yielded, g3 running
  at bench) and one `lost` run; `SLO_OVERVIEW.running` is the array; detail stubs slice the
  journey the same way the api does (`groupOf` mirrored in the fixture from `deploy/slo.md`'s
  ids — keep the row-for-row test green).
- Tests: `lib/slo.test.ts` — `jobsOf` folds four siblings, splits two starts 20 min apart, job
  state precedence; `runTone` for yielded/lost.

- [ ] types + `jobsOf` + tests
- [ ] page, runs table, run tree, overview KPI
- [ ] fixtures; `KLOUDLITE_ADMIN_FIXTURES=1` screenshot of `/superadmin/slo` via
      `scripts/superadmin-screens.mjs` into `.local/screens/` (report the path)
- [ ] commit

### Task 3: incident exclusions (API)

**Files**
- Modify: `crates/workspaces/src/history/` — next migration number: table
  `kloudlite.slo_exclusions (id String, from DateTime, to DateTime, slo_ids Array(String),
  note String, by String, created DateTime) ENGINE = ReplacingMergeTree(created) ORDER BY id`
  (empty `slo_ids` = every SLO). Reader `exclusions(h) -> Vec<Exclusion>`; writer
  `put_exclusion`, `delete_exclusion` (a delete is a tombstone row `deleted DateTime` — add the
  column; readers filter `deleted = 0`).
- Modify: `history/slo.rs` — `FROM_JOINED` gains
  `AND NOT EXISTS (SELECT 1 FROM kloudlite.slo_exclusions AS x FINAL WHERE x.deleted = 0 AND
  r.ts BETWEEN x.from AND x.to AND (empty(x.slo_ids) OR has(x.slo_ids, r.slo_id)))`;
  `STATUS_COLS` gains `countIf(excluded_att) AS excluded_att` computed from the same predicate
  as a column (so the screen can say "N samples excluded") — do this as a LEFT-join-derived
  boolean `excluded`, then `WHERE NOT excluded` for the counts and `countIf(excluded)` before the
  filter; pick the one shape that keeps both `statuses_sql` and `burn_sql` on a single scan.
  `SloStatus` gains `excluded: u64`.
- Modify: `api/admin/slo.rs` — `GET /admin/slo/exclusions`, `POST /admin/slo/exclusions`
  (body `{from, to, slo_ids: [], note}`; 422 on empty note, `to <= from`, window > 7 days, or an
  id not in the catalogue), `DELETE /admin/slo/exclusions/{id}`; both writes through
  `audit::record` (`admin.slo-exclude`, `admin.slo-unexclude`) and both refused without
  ClickHouse (503, like every `/admin/slo*`).
- Tests: SQL string tests (anti-join present in both statements, `FINAL` kept), validation 422s,
  audit rows written once, `api_admin_slo.rs` 503/401 rows for the three new paths.

- [ ] migration + reader/writer
- [ ] SQL + `excluded` column
- [ ] routes + audit + tests
- [ ] commit

### Task 4: incident exclusions (web) and the first exclusion

**Files**
- Modify: `lib/api/admin.ts` — `SloExclusion`, `adminSloExclusions`, `adminSloExclude`,
  `adminSloUnexclude`; `SloStatus.excluded`.
- Modify: `slo/slo-table.tsx` — beside each budget, `· 12 excluded` when > 0 (muted).
- Create: `slo/exclusions.tsx` — Section "Excluded windows": table (window in IST, SLOs or
  "all", note, who, when, Remove); "Exclude a window…" opens the existing dialog pattern from
  repo `settings/` destructive actions: from/to (defaults: the failed run's start/finish when
  opened from a run), SLO multi-select defaulting to all, required note.
- Modify: `slo/runs/[id]/page.tsx` — on a failed run, an "Exclude this window…" button
  prefilled with the run's window and its failed ids.
- Fixtures: two exclusions; route stubs for the three paths.
- Tests: the dialog's payload shaping (`lib/slo.test.ts`), note required.
- After the roll (main session, not the implementer): create the first exclusion via the UI —
  2026-09-15 04:00–10:00 IST, all SLOs, note "k3s apiserver watch-cache freeze; agents frozen
  (incident, fixed by apiserver restart + agent restarts)".

- [ ] api client + table column
- [ ] exclusions section + dialog + run-page button
- [ ] fixtures + tests + screenshot
- [ ] commit

## Self-review

- Every screen that read `running` as one object is named (slo/page.tsx :44–47/:66–68/:92,
  superadmin/page.tsx :54, fixtures :844).
- `lost` is a READ-side state; no row is rewritten, so a late heartbeat still wins.
- Exclusions never delete samples; raw counts stay in `slo_results`, and the screen states the
  excluded count. A window is capped at 7 days so an exclusion cannot quietly become a policy.
- The sibling rule (same suite, ±15 min) lives once in the probe and once in the web; the api
  does not group, on purpose — a list is a list, and the web is the only reader that draws jobs.
