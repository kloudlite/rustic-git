import "server-only";
import { cache } from "react";
import type { Commit } from "@/lib/browse";

/**
 * The api server, from the web app's server side only.
 *
 * Nothing here reaches the browser. The web app holds no database connection and
 * no signing key — it holds a peer secret used exactly once per session, at
 * sign-in, and after that presents the user's own token. Data lives behind the
 * api server so there is one writer, one place that decides what a valid handle
 * is, and one process holding the credentials.
 */

const BASE = (process.env.RUSTIC_GIT_API_URL ?? "http://rustic-git-api").replace(/\/$/, "");
const PEER_SECRET = process.env.RUSTIC_GIT_PEER_SECRET ?? "";

export type ApiUser = {
  _id: string;
  name: string;
  username?: string;
};

export type ApiTeam = {
  _id: string;
  name: string;
  createdBy: string;
  members: { user: string; role: "owner" | "admin" | "member" }[];
};

export type SignIn = { user: ApiUser; token: string | null; expiresIn: number };

/** What a call can end in. `conflict` is not a failure — a taken handle is an
 *  ordinary answer the form has to render, so it is a value rather than a throw. */
export type ApiResult<T> =
  | { ok: true; value: T }
  | { ok: false; kind: "conflict" | "invalid" | "unauthorized" | "notFound" | "unavailable"; message: string };

async function call<T>(
  path: string,
  init: RequestInit & { token?: string; asUser?: string },
): Promise<ApiResult<T>> {
  const headers = new Headers(init.headers);
  headers.set("content-type", "application/json");
  if (init.token) {
    headers.set("authorization", `Bearer ${init.token}`);
  } else if (init.asUser) {
    // Only for sign-in, which is where a token comes from. Every later call
    // carries the user's own token instead.
    headers.set("x-rustic-git-peer", PEER_SECRET);
    headers.set("x-rustic-git-owner", init.asUser);
  }

  let res: Response;
  try {
    res = await fetch(`${BASE}${path}`, { ...init, headers, cache: "no-store" });
  } catch {
    // The api server being unreachable is not the user's problem to read about.
    return { ok: false, kind: "unavailable", message: "The service is unavailable. Try again." };
  }

  if (res.ok) {
    // 204 carries no body, and `json()` on an empty one throws. A delete answers
    // with nothing to say, which is not the same as failing.
    if (res.status === 204) return { ok: true, value: undefined as T };
    return { ok: true, value: (await res.json()) as T };
  }

  const message = (await res.text()).trim();
  if (res.status === 409) return { ok: false, kind: "conflict", message };
  if (res.status === 400) return { ok: false, kind: "invalid", message };
  if (res.status === 401) return { ok: false, kind: "unauthorized", message };
  // The api answers 404 for a namespace the caller may not act in, deliberately:
  // whether it exists is not theirs to learn. The page renders it as one too.
  if (res.status === 404) return { ok: false, kind: "notFound", message };
  return { ok: false, kind: "unavailable", message: "The service is unavailable. Try again." };
}

/** Records the person and returns their token. Called once, at sign-in. */
export function signIn(email: string, name: string) {
  return call<SignIn>("/v1/users", {
    method: "POST",
    asUser: email,
    body: JSON.stringify({ email, name }),
  });
}

/** Claims a handle. Returns a NEW token: the old one asserts they have none. */
export function claimUsername(token: string, username: string) {
  return call<SignIn>("/v1/users/username", {
    method: "POST",
    token,
    body: JSON.stringify({ username }),
  });
}

export function createTeam(token: string, slug: string, name: string) {
  return call<ApiTeam>("/v1/teams", {
    method: "POST",
    token,
    body: JSON.stringify({ slug, name }),
  });
}

export function listTeams(token: string) {
  return call<ApiTeam[]>("/v1/teams", { method: "GET", token });
}

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

/** A credential's metadata. The secret is never here — a token is readable exactly
 *  once, in the reply to the call that created it. */
export type ApiCredential = {
  _id: string;
  kind: "token" | "sshkey" | "signingkey";
  owner: string;
  createdBy: string;
  name: string;
};

export type IssuedToken = ApiCredential & { token: string };

export function listTokens(token: string, owner: string) {
  return call<ApiCredential[]>(`/v1/tokens?owner=${encodeURIComponent(owner)}`, { method: "GET", token });
}

export function createToken(token: string, owner: string, name: string) {
  return call<IssuedToken>("/v1/tokens", { method: "POST", token, body: JSON.stringify({ owner, name }) });
}

export function revokeToken(token: string, id: string) {
  return call<void>(`/v1/tokens/${encodeURIComponent(id)}`, { method: "DELETE", token });
}

export function listKeys(token: string, owner: string, kind: "ssh" | "signing" = "ssh") {
  const k = kind === "signing" ? "&kind=signing" : "";
  return call<ApiCredential[]>(`/v1/keys?owner=${encodeURIComponent(owner)}${k}`, {
    method: "GET",
    token,
  });
}

export function addKey(
  token: string,
  owner: string,
  name: string,
  key: string,
  signing = false,
) {
  return call<ApiCredential>("/v1/keys", {
    method: "POST",
    token,
    body: JSON.stringify({ owner, name, key, signing }),
  });
}

/** What a commit's signature amounts to. `unsigned` is the ordinary case, not a
 *  warning; `unverified` always carries a reason written for a person. */
export type ApiVerification = {
  state: "unsigned" | "verified" | "unverified";
  /** GitHub's vocabulary — `valid`, `unknown_key`, `expired_key`, `revoked_key`,
   *  `bad_email`, `invalid`, `unknown_signature_type` — so a client branches on a
   *  fixed set rather than on prose. */
  reasonCode: string;
  signer?: string;
  reason?: string;
};

export function verifyCommit(token: string, owner: string, name: string, sha: string) {
  return call<ApiVerification>(
    `${repoPath(owner, name)}/commits/${encodeURIComponent(sha)}/signature`,
    { method: "GET", token },
  );
}

export function removeKey(token: string, id: string) {
  return call<void>(`/v1/keys/${encodeURIComponent(id)}`, { method: "DELETE", token });
}

export type ApiPasskey = {
  _id: string;
  user: string;
  publicKey: string;
  counter: number;
  transports: string[];
  name: string;
};

export function listPasskeys(token: string) {
  return call<ApiPasskey[]>("/v1/passkeys", { method: "GET", token });
}

export function addPasskey(
  token: string,
  key: { id: string; publicKey: string; counter: number; transports: string[]; name: string },
) {
  return call<ApiPasskey>("/v1/passkeys", { method: "POST", token, body: JSON.stringify(key) });
}

export function removePasskey(token: string, id: string) {
  return call<void>(`/v1/passkeys/${encodeURIComponent(id)}`, { method: "DELETE", token });
}

/** Sign-in only, so it goes over the peer path: there is no session yet, and the
 *  browser must never be able to ask whose credential an id belongs to. */
export function lookupPasskey(id: string) {
  return call<ApiPasskey>("/v1/passkeys/lookup", {
    method: "POST",
    asUser: "passkey-lookup",
    body: JSON.stringify({ id }),
  });
}

export function passkeyUsed(id: string, counter: number) {
  return call<void>(`/v1/passkeys/${encodeURIComponent(id)}/used`, {
    method: "POST",
    asUser: "passkey-lookup",
    body: JSON.stringify({ counter }),
  });
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
    { method: "POST", token, body: JSON.stringify(patch) },
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

export type PullState = "open" | "merged" | "closed";

export type ApiComment = { author: string; body: string; at: number | { $date: unknown } };

/** A proposed change. It names two BRANCHES — the commits and the diff are read
 *  from git on every view, so a push to the branch updates what it contains. */
/** Whether a change could be merged, worked out by the worker ahead of time —
 *  not computed while you wait. Absent means nobody has looked yet. */
export type ApiMergeability = {
  state: "clean" | "behind" | "dirty" | "unknown";
  detail?: string;
  /** Whether the base can simply MOVE to this branch. "clean" no longer implies it: a diverged
   *  branch that the worker merged cleanly is clean too, and fast-forwarding it would fail. */
  fastForward?: boolean;
};

/** A merge that was asked for, and where it got to. */
export type ApiMergeJob = {
  state: "queued" | "running" | "conflicts" | "failed";
  strategy: string;
  detail?: string;
};

export type ApiPull = {
  _id: string;
  repo: string;
  number: number;
  title: string;
  body: string;
  base: string;
  head: string;
  state: PullState;
  author: string;
  /** Full bodies on the detail route only; the LIST sends `commentCount` instead. */
  comments?: ApiComment[];
  commentCount?: number;
  mergeability?: ApiMergeability;
  merge?: ApiMergeJob;
};

/** What one branch would bring to another, straight from the fleet. */
export type ApiComparison = {
  base: string;
  head: string;
  merge_base: string | null;
  fast_forward: boolean;
  commits: Commit[];
  diff: string;
};

const repoPath = (owner: string, name: string) =>
  `/v1/repos/${encodeURIComponent(owner)}/${encodeURIComponent(name)}`;

export function listPulls(token: string, owner: string, name: string) {
  // ponytail: flat 100 cap, no paging; add ?page= when a repo outgrows it
  return call<ApiPull[]>(`${repoPath(owner, name)}/pulls?limit=100`, { method: "GET", token });
}

export function getPull(token: string, owner: string, name: string, number: number) {
  return call<ApiPull>(`${repoPath(owner, name)}/pulls/${number}`, { method: "GET", token });
}

export function openPull(
  token: string,
  owner: string,
  name: string,
  pull: { title: string; body: string; base: string; head: string },
) {
  return call<ApiPull>(`${repoPath(owner, name)}/pulls`, {
    method: "POST",
    token,
    body: JSON.stringify(pull),
  });
}

export function commentOnPull(token: string, owner: string, name: string, number: number, body: string) {
  return call<void>(`${repoPath(owner, name)}/pulls/${number}/comments`, {
    method: "POST",
    token,
    body: JSON.stringify({ body }),
  });
}

/** How the change should land. All three are only offered when the base is an
 *  ancestor of the head — see the server, which refuses anything else. */
export type MergeStrategy = "fast-forward" | "squash" | "merge";

export function mergePull(
  token: string,
  owner: string,
  name: string,
  number: number,
  strategy: MergeStrategy = "fast-forward",
) {
  return call<{ merged: string }>(
    `${repoPath(owner, name)}/pulls/${number}/merge?strategy=${strategy}`,
    { method: "POST", token },
  );
}

export function closePull(token: string, owner: string, name: string, number: number) {
  return call<void>(`${repoPath(owner, name)}/pulls/${number}/close`, { method: "POST", token });
}

export function compareBranches(token: string, owner: string, name: string, base: string, head: string) {
  const q = `base=${encodeURIComponent(base)}&head=${encodeURIComponent(head)}`;
  return call<ApiComparison>(`${repoPath(owner, name)}/compare?${q}`, { method: "GET", token });
}
