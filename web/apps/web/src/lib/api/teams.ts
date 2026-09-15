import { cache } from "react";
import { call } from "./client";
import type { ApiTeam } from "./client";

/**
 * Teams: profile, members, roles, invitations.
 */
export function createTeam(token: string, slug: string, name: string, region: string) {
  return call<ApiTeam>("/v1/teams", {
    method: "POST",
    token,
    body: JSON.stringify({ slug, name, region }),
  });
}

// `cache()`: the shell reads this for the owner switcher and the page beneath it reads it again
// for its own owner list — one request, one `/v1/teams`.
export const listTeams = cache(function listTeams(token: string) {
  return call<ApiTeam[]>("/v1/teams", { method: "GET", token });
});

export type ApiRole = "owner" | "admin" | "member";

export type ApiTeamMember = {
  email: string;
  name: string;
  username?: string;
  role: ApiRole;
  joinedAt: string;
  /** Absent from an api older than pausing, which reads as active. */
  state?: "active" | "paused";
};

/** One team as its settings page needs it: the members joined onto their directory
 *  rows, and the caller's own role so the page knows which controls to draw. */
export type ApiTeamDetail = {
  slug: string;
  name: string;
  description: string;
  createdAt: string;
  yourRole: ApiRole;
  members: ApiTeamMember[];
  /** Open invitations. Empty for a plain member, who cannot invite and so is not told. */
  invites: ApiInvite[];
  public: boolean;
  tagline: string;
  location: string;
  website: string;
  email: string;
  pins: string[];
};

/** A public repo as the anonymous profile route shows it — not the full `ApiRepo`,
 *  since a stranger gets no `_id`, owner, or `createdBy`. */
export type ApiPublicRepo = { name: string; description: string; public: boolean; createdAt: number };

/** The team home page, read anonymously: no token, no membership-gated fields. */
export type ApiTeamProfile = {
  slug: string;
  name: string;
  description: string;
  tagline: string;
  location: string;
  website: string;
  email: string;
  memberCount: number;
  pins: string[];
  repos: ApiPublicRepo[];
};

export type TeamProfileInput = {
  public: boolean;
  tagline: string;
  location: string;
  website: string;
  email: string;
  pins: string[];
};

export type ApiInvite = { id: string; email: string; role: ApiRole; invitedBy: string; expiresAt: string };

/** Returned once, at creation: the token goes into the email and nowhere else. */
export type ApiIssuedInvite = { id: string; token: string; email: string; role: ApiRole; team_name: string };

export type ApiInvitePreview = { team: string; teamName: string; email: string; role: ApiRole; invitedBy: string };

export const teamPath = (slug: string) => `/v1/teams/${encodeURIComponent(slug)}`;

export function getTeam(token: string, slug: string) {
  return call<ApiTeamDetail>(teamPath(slug), { method: "GET", token });
}

/** The team home page's data, anonymous — cached per render like `listRepos`. */
export const getTeamProfile = cache(function getTeamProfile(slug: string) {
  return call<ApiTeamProfile>(`${teamPath(slug)}/profile`, { method: "GET" });
});

export function updateTeam(
  token: string,
  slug: string,
  body: { name: string; description: string; profile?: TeamProfileInput },
) {
  return call<void>(teamPath(slug), { method: "PATCH", token, body: JSON.stringify(body) });
}

export function createInvite(token: string, slug: string, email: string, role: ApiRole) {
  return call<ApiIssuedInvite>(`${teamPath(slug)}/invites`, {
    method: "POST",
    token,
    body: JSON.stringify({ email, role }),
  });
}

export function revokeInvite(token: string, slug: string, id: string) {
  return call<void>(`${teamPath(slug)}/invites/${encodeURIComponent(id)}`, { method: "DELETE", token });
}

export function previewInvite(token: string, invite: string) {
  return call<ApiInvitePreview>(`/v1/invites/${encodeURIComponent(invite)}`, { method: "GET", token });
}

export function acceptInvite(token: string, invite: string) {
  return call<{ team: string }>(`/v1/invites/${encodeURIComponent(invite)}/accept`, { method: "POST", token });
}

export function setTeamRole(token: string, slug: string, email: string, role: ApiRole) {
  return call<void>(`${teamPath(slug)}/members/${encodeURIComponent(email)}`, {
    method: "PATCH",
    token,
    body: JSON.stringify({ role }),
  });
}

export function removeTeamMember(token: string, slug: string, email: string) {
  return call<void>(`${teamPath(slug)}/members/${encodeURIComponent(email)}`, { method: "DELETE", token });
}

export function deleteTeam(token: string, slug: string) {
  return call<void>(teamPath(slug), { method: "DELETE", token });
}

const memberPath = (slug: string, who: string) => `${teamPath(slug)}/members/${encodeURIComponent(who)}`;

export function pauseTeamMember(token: string, slug: string, email: string) {
  return call<void>(`${memberPath(slug, email)}/pause`, { method: "POST", token });
}

export function unpauseTeamMember(token: string, slug: string, email: string) {
  return call<void>(`${memberPath(slug, email)}/unpause`, { method: "POST", token });
}

/** A removed member whose bench and workspaces are still waiting out the grace period. */
export type ApiRemoval = { owner: string; delete_at: string };

export function listTeamRemovals(token: string, slug: string) {
  return call<ApiRemoval[]>(`${teamPath(slug)}/removals`, { method: "GET", token });
}

/** `owner` is the person's handle; the body names both again, which is the api's confirmation. */
export function deleteRemovalNow(token: string, slug: string, owner: string) {
  return call<{ deletes_enabled: boolean }>(`${memberPath(slug, owner)}/delete-now`, {
    method: "POST",
    token,
    body: JSON.stringify({ person: owner, team: slug }),
  });
}
