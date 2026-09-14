import { call } from "./client";

/**
 * A person's SPACE — their workspaces and bench in one team, or their personal ones — follows one
 * environment, chosen here. `team` is the team slug, or the person's own handle for the personal
 * space. The api answers only for the caller; see `crates/workspaces/src/api/me.rs`.
 */
export type ApiSpaceEnvironment = { team: string; environment: string; region: string | null };

export function myEnvironments(token: string) {
  return call<ApiSpaceEnvironment[]>("/v1/me/environments", { method: "GET", token });
}

/** 404 not your team or no such environment; 409 the environment is not that team's. */
export function setMyEnvironment(token: string, team: string, environment: string) {
  return call<ApiSpaceEnvironment>(`/v1/me/environments/${encodeURIComponent(team)}`, {
    method: "PUT", token, body: JSON.stringify({ environment }),
  });
}

/** Idempotent. */
export function clearMyEnvironment(token: string, team: string) {
  return call<void>(`/v1/me/environments/${encodeURIComponent(team)}`, { method: "DELETE", token });
}

/** The space an owner page belongs to: the owner itself, which for your own handle is your
 *  personal space. Lowercased the way the api folds it. */
export function spaceTeam(owner: string) {
  return owner.toLowerCase();
}

/** Whether `environment` is what the caller's space in `team` uses. */
export function spaceUses(choices: ApiSpaceEnvironment[], team: string, environment: string) {
  return choices.some((c) => c.team === spaceTeam(team) && c.environment === environment);
}
