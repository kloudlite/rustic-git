import { expect, test } from "bun:test";
import { place } from "./shell-nav";

/** The chrome decides what it is looking at from the URL alone. These are the six
 *  shapes that decision has to get right; the owner is the half that regressed. */
const me = "ada";

test("the root is the signed-in person's own namespace", () => {
  expect(place("/", me)).toEqual({ kind: "org", owner: "ada" });
});

test("a root page names nobody's namespace, so it falls back to the person", () => {
  expect(place("/settings", me)).toEqual({ kind: "org", owner: "ada" });
});

test("a team's own page is the team's namespace, not the person's", () => {
  expect(place("/team", me)).toEqual({ kind: "org", owner: "team" });
});

test("a second segment that is not reserved names a repo in that owner", () => {
  expect(place("/team/repo", me)).toEqual({ kind: "repo", owner: "team", repo: "repo" });
});

test("a reserved second segment is a section of the team, not a repo", () => {
  expect(place("/team/registries", me)).toEqual({ kind: "org", owner: "team" });
  expect(place("/team/repos", me)).toEqual({ kind: "org", owner: "team" });
});

test("a third segment under registries is the image itself", () => {
  expect(place("/team/registries/img/tags", me)).toEqual({ kind: "image", owner: "team", image: "img" });
});
