import { expect, test } from "bun:test";
import { place, shellWidthClass } from "./shell-nav";

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

test("the environments list is a section of the owner, not a repo", () => {
  expect(place("/team/environments", me)).toEqual({ kind: "org", owner: "team" });
});

test("a third segment under environments is the environment itself, with its own tabs", () => {
  expect(place("/team/environments/env-1", me)).toEqual({ kind: "env", owner: "team", env: "env-1" });
  expect(place("/team/environments/env-1/snapshots", me)).toEqual({ kind: "env", owner: "team", env: "env-1" });
});

// `/superadmin` hangs off the root, not off a namespace: without this the chrome reads
// `superadmin` as an owner handle and shows the wrong crumb on every superadmin page.
test("the superadmin area is its own place, not an org", () => {
  expect(place("/superadmin", "karthik")).toEqual({ kind: "superadmin" });
  expect(place("/superadmin/owners", "karthik")).toEqual({ kind: "superadmin" });
  expect(place("/superadmin/clusters", "karthik")).toEqual({ kind: "superadmin" });
});

test("only the superadmin console drops the centred container", () => {
  expect(shellWidthClass("/superadmin", me)).toBe("w-full px-7");
  expect(shellWidthClass("/superadmin/owners/acme", me)).toBe("w-full px-7");
  // Every other place keeps the 1120 px column the rest of the app is laid out in.
  expect(shellWidthClass("/", me)).toBe("mx-auto max-w-page px-6");
  expect(shellWidthClass("/team/repo", me)).toBe("mx-auto max-w-page px-6");
  expect(shellWidthClass("/settings", me)).toBe("mx-auto max-w-page px-6");
});
