import { describe, expect, test } from "bun:test";
import type { SloRun, SloStatus, SloStep } from "@/lib/api";
import { budgetLabel, exclusionPayload, burnLabel, groupByFeature, jobDone, jobsOf, msLabel, progressOf, runStateLabel, runTone, targetMs, treeOf, windowLabel } from "@/lib/slo";

const slo = (id: string, feature: string, state: SloStatus["state"]): SloStatus => ({
  id,
  feature,
  sli: id,
  target: "99.9 %",
  suite: "fast",
  attainment_30d: 0.999,
  total_30d: 100,
  budget_30d: 1,
  budget_left: 0.5,
  burn_short: null,
  burn_long: null,
  window_short_secs: 3600,
  window_long_secs: 21600,
  last: null,
  state,
  excluded: 0,
});

const step = (slo_id: string, stage: string): SloStep => ({
  slo_id,
  stage,
  ts: "2026-09-05T00:00:00Z",
  ok: true,
  ms: 12,
  skipped: false,
  detail: "",
});

describe("budgetLabel", () => {
  test("reads as what is left", () => {
    expect(budgetLabel(1.08, 9)).toBe("12 % left");
  });
  // A breaching SLO has spent more budget than it had; "-30 % left" would read as a bug.
  test("says an overspend in its own words", () => {
    expect(budgetLabel(-2.7, 9)).toBe("30 % over");
  });
  test("no sample is a dash, never 0 %", () => {
    expect(budgetLabel(null, 9)).toBe("—");
  });
  test("a 100 % target has no budget to divide by", () => {
    expect(budgetLabel(0, 0)).toBe("no budget · 0 bad");
    expect(budgetLabel(-4, 0)).toBe("no budget · 4 bad");
  });
});

describe("burnLabel", () => {
  test("is a multiple of the budget's own rate, to one decimal", () => {
    expect(burnLabel(1.44)).toBe("1.4×");
    expect(burnLabel(null)).toBe("—");
  });
});

describe("windowLabel", () => {
  test("names the window in its largest whole unit", () => {
    expect(windowLabel(3600)).toBe("1 h");
    expect(windowLabel(21600)).toBe("6 h");
    expect(windowLabel(2419200)).toBe("4 w");
  });
});

describe("groupByFeature", () => {
  test("keeps the catalogue's feature order", () => {
    const g = groupByFeature([slo("a", "Git", "ok"), slo("b", "Registry", "ok"), slo("c", "Git", "ok")]);
    expect(g.map((x) => x.feature)).toEqual(["Git", "Registry"]);
    expect(g[0].slos.map((s) => s.id)).toEqual(["a", "c"]);
  });
  test("sorts burning first inside a feature", () => {
    const g = groupByFeature([slo("ok", "Git", "ok"), slo("un", "Git", "unknown"), slo("burn", "Git", "burning")]);
    expect(g[0].slos.map((s) => s.id)).toEqual(["burn", "un", "ok"]);
  });
});

describe("msLabel", () => {
  test("says a step in ms, a stage in seconds and a run in minutes", () => {
    expect(msLabel(412)).toBe("412 ms");
    expect(msLabel(3_240)).toBe("3.2 s");
    expect(msLabel(64_000)).toBe("1 m 04 s");
    expect(msLabel(null)).toBe("—");
  });
});

describe("targetMs", () => {
  test("reads the latency ceiling out of a rendered target", () => {
    expect(targetMs("95 % ≤ 2000 ms")).toBe(2000);
    expect(targetMs("99.9 % ≤ 30000 ms")).toBe(30000);
  });
  // An availability-only SLO has no ceiling: a step's ms must never be red for want of a target.
  test("an availability target has no ceiling", () => {
    expect(targetMs("99.9 %")).toBeNull();
    expect(targetMs(undefined)).toBeNull();
  });
});

const JOURNEY = [
  { name: "0 · Boot", ids: [] },
  { name: "1 · Identity", ids: ["id.signin", "id.token.mint"] },
  { name: "2 · Git", ids: ["git.push.ok", "git.clone.p95"] },
  { name: "3 · Registry", ids: ["reg.push.ok"] },
];

describe("treeOf", () => {
  test("a stage nobody has reached yet is pending, with its steps already named", () => {
    const t = treeOf(JOURNEY, []);
    expect(t.map((s) => s.state)).toEqual(["pending", "pending", "pending", "pending"]);
    expect(t[1].steps.map((s) => s.id)).toEqual(["id.signin", "id.token.mint"]);
    expect(t[1].steps.every((s) => s.step === null)).toBe(true);
  });
  test("a stage half reported is running, and sums the ms it has", () => {
    const t = treeOf(JOURNEY, [step("id.signin", "1 · Identity"), step("id.token.mint", "1 · Identity"), step("git.push.ok", "2 · Git")]);
    expect(t.map((s) => s.state)).toEqual(["pending", "passed", "running", "pending"]);
    expect(t[1].ms).toBe(24);
    expect(t[1].ok).toBe(2);
  });
  // A failed run's remaining stages never ran; "pending" would read as a run still in flight.
  test("everything after a failure is skipped, not pending", () => {
    const failed = { ...step("git.push.ok", "2 · Git"), ok: false, detail: "500" };
    const t = treeOf(JOURNEY, [step("id.signin", "1 · Identity"), step("id.token.mint", "1 · Identity"), failed]);
    expect(t.map((s) => s.state)).toEqual(["pending", "passed", "failed", "skipped"]);
  });
  test("a stage whose steps were all skipped is skipped", () => {
    const sk = { ...step("reg.push.ok", "3 · Registry"), ok: false, skipped: true };
    expect(treeOf(JOURNEY, [sk])[3].state).toBe("skipped");
  });
  // An id the catalogue does not list still belongs to the stage that reported it.
  test("keeps a step the journey does not name", () => {
    const t = treeOf(JOURNEY, [step("git.extra", "2 · Git")]);
    expect(t[2].steps.map((s) => s.id)).toEqual(["git.push.ok", "git.clone.p95", "git.extra"]);
  });
});

describe("progressOf", () => {
  test("counts steps done over steps the journey holds", () => {
    expect(progressOf(treeOf(JOURNEY, []))).toEqual({ done: 0, total: 5 });
    expect(progressOf(treeOf(JOURNEY, [step("id.signin", "1 · Identity")]))).toEqual({ done: 1, total: 5 });
  });
});

// The hourly suite is four parallel group runs; "did the hourly pass" is a question about the
// four together, and the api deliberately hands back a flat list.
const runOf = (run_id: string, suite: string, started: string, state: SloRun["state"], group: number | null): SloRun => ({
  run_id,
  suite,
  region: "centralindia-k3s",
  started,
  finished: state === "running" ? null : started,
  state,
  stage: "5 · Workspace",
  steps_total: 10,
  steps_failed: state === "failed" ? 1 : 0,
  failed_step: state === "failed" ? "reg.push.ok" : "",
  failed_detail: "",
  duration_ms: 1_000,
  group,
});

describe("jobsOf", () => {
  const at = (mins: number) => new Date(Date.UTC(2026, 8, 16, 10, mins)).toISOString();

  test("folds four sibling group runs into one job", () => {
    const jobs = jobsOf([0, 1, 2, 3].map((g) => runOf(`hourly-1-g${g}`, "hourly", at(g), "passed", g)));
    expect(jobs).toHaveLength(1);
    expect(jobs[0].runs.map((r) => r.group)).toEqual([0, 1, 2, 3]);
    // The job started when its earliest group did, whatever order the api answered in.
    expect(jobs[0].started).toBe(at(0));
    expect(jobs[0].state).toBe("passed");
  });

  test("splits two hourly starts twenty minutes apart", () => {
    const jobs = jobsOf([
      runOf("hourly-1-g0", "hourly", at(0), "passed", 0),
      runOf("hourly-2-g0", "hourly", at(20), "passed", 0),
    ]);
    expect(jobs).toHaveLength(2);
  });

  // The real case: a hand-started hourly at 09:56 and the scheduled one at 10:02, which yields
  // because a sibling suite is already in flight. Both walk all four groups, six minutes apart.
  test("two hourly launches six minutes apart stay two jobs, not one of eight groups", () => {
    const jobs = jobsOf([
      ...[0, 1, 2, 3].map((g) => runOf(`hourly-1-g${g}`, "hourly", at(0), "passed", g)),
      ...[0, 1, 2, 3].map((g) => runOf(`hourly-2-g${g}`, "hourly", at(6), "yielded", g)),
    ]);
    expect(jobs).toHaveLength(2);
    expect(jobs.map((j) => j.runs.length)).toEqual([4, 4]);
    expect(jobs[0].state).toBe("passed");
    expect(jobs[1].state).toBe("yielded");
    expect(runStateLabel(jobs[1].state)).toBe("stood aside");
  });

  test("an ungrouped run is a job of one, never folded into a neighbour", () => {
    const jobs = jobsOf([runOf("fast-1", "fast", at(0), "passed", null), runOf("fast-2", "fast", at(1), "passed", null)]);
    expect(jobs).toHaveLength(2);
  });

  test("one group's failure is the job's, and a running group outranks a lost one", () => {
    const states: SloRun["state"][] = ["passed", "yielded", "lost", "failed"];
    const jobs = jobsOf(states.map((st, g) => runOf(`hourly-1-g${g}`, "hourly", at(g), st, g)));
    expect(jobs[0].state).toBe("failed");
    const noFail = jobsOf(
      (["running", "yielded", "lost", "passed"] as SloRun["state"][]).map((st, g) =>
        runOf(`hourly-1-g${g}`, "hourly", at(g), st, g),
      ),
    );
    expect(noFail[0].state).toBe("running");
    expect(jobsOf([runOf("h-g0", "hourly", at(0), "lost", 0), runOf("h-g1", "hourly", at(1), "passed", 1)])[0].state).toBe("lost");
    // All four stood aside: the job stood aside, it did not pass.
    expect(jobsOf([0, 1].map((g) => runOf(`h-g${g}`, "hourly", at(g), "yielded", g)))[0].state).toBe("yielded");
    // A skipped group counts as a pass, exactly as the probe's own roll-up does.
    expect(jobsOf([runOf("h-g0", "hourly", at(0), "skipped", 0), runOf("h-g1", "hourly", at(1), "passed", 1)])[0].state).toBe("passed");
  });

  test("done counts the groups that will report nothing more", () => {
    const job = jobsOf(
      (["running", "passed", "yielded", "running"] as SloRun["state"][]).map((st, g) =>
        runOf(`h-g${g}`, "hourly", at(g), st, g),
      ),
    )[0];
    expect(jobDone(job)).toEqual({ done: 2, total: 4 });
  });

  // The console's "Running now" KPI, 2026-09-16: the api's `running` list is stored state, so six
  // lost rows from a week ago sat beside one live hourly and the tile read "10 runs across 1 job".
  test("lost runs in the running list are neither running runs nor running jobs", () => {
    const running = [
      ...[0, 1, 2, 3].map((g) => runOf(`hourly-2-g${g}`, "hourly", at(30), "running", g)),
      runOf("hourly-1788682088", "hourly", at(0), "lost", null),
      runOf("hourly-1788682099", "hourly", at(1), "lost", null),
    ];
    const jobs = jobsOf(running);
    expect(running.filter((r) => r.state === "running")).toHaveLength(4);
    expect(jobs.filter((j) => j.state === "running")).toHaveLength(1);
    expect(running.filter((r) => r.state === "lost")).toHaveLength(2);
    // Each lost ungrouped run is its own job of one, and none of them is drawn as "Running".
    expect(jobs.filter((j) => j.state === "lost").map((j) => j.runs.length)).toEqual([1, 1]);
  });
});

describe("runTone and its words", () => {
  test("standing aside is not a failure, and a lost pod is a warning", () => {
    expect(runTone("yielded")).toBe("neutral");
    expect(runTone("lost")).toBe("warn");
    expect(runTone("failed")).toBe("critical");
    expect(runTone("running")).toBe("info");
    expect(runStateLabel("yielded")).toBe("stood aside");
    expect(runStateLabel("lost")).toBe("lost — no heartbeat for 30 min");
    expect(runStateLabel("passed")).toBe("passed");
  });
});

describe("exclusionPayload", () => {
  const good = { from: "2026-09-15T04:00", to: "2026-09-15T10:00", sloIds: [], note: " apiserver freeze " };

  // The form's fields carry no zone; they mean IST, and what leaves is the instant.
  test("reads the two fields as IST", () => {
    const r = exclusionPayload(good);
    expect(r.ok && r.body.from).toBe("2026-09-14T22:30:00.000Z");
    expect(r.ok && r.body.to).toBe("2026-09-15T04:30:00.000Z");
    expect(r.ok && r.body.note).toBe("apiserver freeze");
    expect(r.ok && r.body.slo_ids).toEqual([]);
  });

  test("the note is required", () => {
    expect(exclusionPayload({ ...good, note: "   " })).toEqual({ ok: false, message: "note is required" });
  });

  test("a window runs forwards", () => {
    const r = exclusionPayload({ ...good, to: good.from });
    expect(r.ok).toBe(false);
  });

  // Past seven days it is a policy, not an incident — the api refuses it too.
  test("a window is capped", () => {
    const r = exclusionPayload({ ...good, to: "2026-09-25T04:00" });
    expect(r.ok).toBe(false);
  });

  test("selected ids ride along", () => {
    const r = exclusionPayload({ ...good, sloIds: ["reg.push.ok"] });
    expect(r.ok && r.body.slo_ids).toEqual(["reg.push.ok"]);
  });
});
