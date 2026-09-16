import { expect, test } from "bun:test";
import { deleteNowConfirm, pauseConfirm, removalConfirm, removalKey } from "./team-removal";

test("the remove dialog names the date seven days out and what stays", () => {
  const t = removalConfirm("Ana", "acme", Date.UTC(2026, 8, 15, 12));
  expect(t).toContain("deleted on Sep 22, 2026 (7 days)");
  expect(t).toContain("Pushed snapshots, repos, images and team environments stay with the team.");
  expect(t).toContain("within about 5 minutes");
});

// The bench folder dies with the Bench, so the transcripts are the one thing a removal takes that
// no snapshot can give back. Both delete confirms must say so before the person clicks.
test("both delete confirms say the bench transcripts and sessions go too", () => {
  for (const t of [removalConfirm("Ana", "acme", Date.UTC(2026, 8, 15, 12)), deleteNowConfirm("ana", "acme")]) {
    expect(t).toContain("chat transcripts and sessions");
  }
});

test("the pause dialog says nothing is deleted", () => {
  expect(pauseConfirm("Ana")).toContain("nothing is deleted; unpausing restores access but starts nothing");
});

test("delete-now names both the person and the team", () => {
  const t = deleteNowConfirm("ana", "acme");
  expect(t).toContain("ana");
  expect(t).toContain("acme");
});

// The superadmin console's Pending removals lists every team, so the same handle can appear
// twice — once per team it was removed from. The confirm text (what `RemovalRow`'s visible
// paragraph and aria-label both render) must differ, or a person reading two rows for "ana" could
// not tell which row deletes which team's data.
test("a person pending removal from two teams gets a distinct confirm per team", () => {
  const inAcme = deleteNowConfirm("ana", "acme");
  const inOpsLab = deleteNowConfirm("ana", "ops-lab");
  expect(inAcme).not.toBe(inOpsLab);
  expect(inAcme).toContain("acme");
  expect(inAcme).not.toContain("ops-lab");
  expect(inOpsLab).toContain("ops-lab");
  expect(inOpsLab).not.toContain("acme");
});

test("removalKey distinguishes the same owner pending in two different teams", () => {
  expect(removalKey("acme", "ana")).not.toBe(removalKey("ops-lab", "ana"));
  expect(removalKey("acme", "ana")).toBe(removalKey("acme", "ana"));
});
