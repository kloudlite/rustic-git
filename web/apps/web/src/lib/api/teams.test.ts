import { expect, mock, test } from "bun:test";

const fetched: { url: string; init: RequestInit }[] = [];
mock.module("./client", () => ({
  call: async (url: string, init: RequestInit) => (fetched.push({ url, init }), { ok: true, value: undefined }),
}));
const teams = await import("./teams");

test("pause, unpause, removals and delete now request shapes", () => {
  teams.pauseTeamMember("t", "acme", "a@x.io");
  teams.unpauseTeamMember("t", "acme", "a@x.io");
  teams.listTeamRemovals("t", "acme");
  teams.deleteRemovalNow("t", "acme", "ana");
  expect(fetched.map((f) => `${f.init.method} ${f.url}`)).toEqual([
    "POST /v1/teams/acme/members/a%40x.io/pause",
    "POST /v1/teams/acme/members/a%40x.io/unpause",
    "GET /v1/teams/acme/removals",
    "POST /v1/teams/acme/members/ana/delete-now",
  ]);
  expect(JSON.parse(String(fetched[3].init.body))).toEqual({ person: "ana", team: "acme" });
});
