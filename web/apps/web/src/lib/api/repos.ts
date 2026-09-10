import { cache } from "react";
import { SLOW_TIMEOUT_MS, call } from "./client";

/**
 * Repositories: listing, creation, protection, commits, the activity feed.
 */
export type ApiRepo = {
  _id: string;
  owner: string;
  name: string;
  public: boolean;
  description: string;
  createdBy: string;
  /** Unix milliseconds — the api converts the stored BSON date, so the browser
   *  never sees `{"$date":…}`. */
  createdAt: number;
};

/** Cached per render, like `guardRepo`: the shell lists every owner's repos for
 *  ⌘K and the dashboard lists the one it is showing, so the same call would
 *  otherwise go out twice. `cache` dedupes within one request only — the fetch
 *  itself stays `no-store`, so nothing is held across requests. */
export const listRepos = cache(function listRepos(token: string, owner: string) {
  return call<ApiRepo[]>(`/v1/repos?owner=${encodeURIComponent(owner)}`, { method: "GET", token });
});

/** One repo, for the page guard — the guard used to list the whole namespace to
 *  check a single name. Cached per render for the same reason `listRepos` is. */
export const getRepo = cache(function getRepo(token: string, owner: string, name: string) {
  return call<ApiRepo>(`/v1/repos/${encodeURIComponent(owner)}/${encodeURIComponent(name)}`, {
    method: "GET",
    token,
  });
});

export function createRepo(
  token: string,
  repo: { owner: string; name: string; visibility: "public" | "private"; description?: string },
) {
  return call<ApiRepo>("/v1/repos", { method: "POST", token, body: JSON.stringify(repo) });
}

/** A branch protection rule, as the fleet stores and enforces it. */
export type ApiProtection = {
  pattern: string;
  no_force: boolean;
  no_delete: boolean;
};

export function updateRepo(
  token: string,
  owner: string,
  name: string,
  change: { description?: string; visibility?: "public" | "private" },
) {
  return call<void>(`/v1/repos/${encodeURIComponent(owner)}/${encodeURIComponent(name)}`, {
    method: "PATCH",
    token,
    body: JSON.stringify(change),
  });
}

export function deleteRepo(token: string, owner: string, name: string) {
  return call<void>(`/v1/repos/${encodeURIComponent(owner)}/${encodeURIComponent(name)}`, {
    method: "DELETE",
    token,
  });
}

export function listProtection(token: string, owner: string, name: string) {
  return call<ApiProtection[]>(
    `/v1/repos/${encodeURIComponent(owner)}/${encodeURIComponent(name)}/protection`,
    { method: "GET", token },
  );
}

export function setProtection(
  token: string,
  owner: string,
  name: string,
  rule: { pattern: string; remove?: boolean; no_force?: boolean; no_delete?: boolean },
) {
  return call<void>(
    `/v1/repos/${encodeURIComponent(owner)}/${encodeURIComponent(name)}/protection`,
    { method: "POST", token, body: JSON.stringify(rule) },
  );
}

/** One file's worth of a patch. Contents are base64 because a file is arbitrary
 *  bytes and JSON carries text. */
export type FileChange =
  | { path: string; contentBase64: string; executable?: boolean }
  | { path: string; delete: true };

export type Committed = { commit: string; branch: string };

/** Land a set of file changes as ONE commit.
 *
 *  `expect` is the tip the editor was reading. The server re-reads the branch and
 *  refuses if it has moved, so a push that arrives mid-edit is a conflict the
 *  person is told about rather than work silently overwritten.
 *
 *  `newBranch` commits to a new branch instead, leaving the base where it is —
 *  which is how an edit to a protected branch becomes a change to review. */
export function commitPatch(
  token: string,
  owner: string,
  name: string,
  patch: {
    branch: string;
    message: string;
    changes: FileChange[];
    expect?: string;
    newBranch?: string;
  },
) {
  return call<Committed>(
    `/v1/repos/${encodeURIComponent(owner)}/${encodeURIComponent(name)}/commits`,
    { method: "POST", token, body: JSON.stringify(patch), signal: AbortSignal.timeout(SLOW_TIMEOUT_MS) },
  );
}

/** One thing that happened, as the feed shows it. */
export type ApiEvent = {
  kind: "commit" | "pull_opened" | "pull_merged" | "repo_created";
  repo: string;
  actor: string;
  title: string;
  detail: string;
  /** Seconds since the epoch — formatted here, in the reader's locale. */
  at: number;
  href: string;
};

/** What has happened lately across an owner's repos.
 *
 *  Derived from the directory and from git rather than from an event log, so it
 *  is right for repos that existed before the feed did — and can only show what
 *  those two actually record. */
export function activity(token: string, owner: string, limit?: number) {
  const n = limit ? `&limit=${limit}` : "";
  return call<ApiEvent[]>(`/v1/activity?owner=${encodeURIComponent(owner)}${n}`, {
    method: "GET",
    token,
  });
}
