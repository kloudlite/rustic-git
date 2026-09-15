import { expect, test } from "bun:test";
import { pauseConfirm, removalConfirm } from "./team-removal";

test("the remove dialog names the date seven days out and what stays", () => {
  const t = removalConfirm("Ana", "acme", Date.UTC(2026, 8, 15, 12));
  expect(t).toContain("deleted on Sep 22, 2026 (7 days)");
  expect(t).toContain("Pushed snapshots, repos, images and team environments stay with the team.");
  expect(t).toContain("within about 5 minutes");
});

test("the pause dialog says nothing is deleted", () => {
  expect(pauseConfirm("Ana")).toContain("nothing is deleted; unpausing restores access but starts nothing");
});
