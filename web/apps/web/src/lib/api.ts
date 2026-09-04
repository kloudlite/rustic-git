import "server-only";
import { cache } from "react";
import type { Commit } from "@/lib/browse";
import type { SnapshotState } from "@/lib/snapshot-state";
import type { QuotaDim, QuotaReport } from "@/lib/quota";
import { auditQueryString, type AuditEntry, type AuditFilter, type AuditPage } from "@/lib/audit";
import { FLAT, type HistoryEvent, type HistorySeries, type SeriesName } from "@/lib/history";
import { fixtureFor } from "@/lib/fixtures/superadmin";

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
// A second base, because the admin surface is a SEPARATE process on a separate host (design doc
// §5) — pointing this at the same host as `BASE` would be a silent way to lose the whole point of
// the split, so there is no fallback to `RUSTIC_GIT_API_URL` here.
const ADMIN_BASE = (process.env.RUSTIC_GIT_ADMIN_API_URL ?? "http://rustic-git-admin").replace(/\/$/, "");
const PEER_SECRET = process.env.RUSTIC_GIT_PEER_SECRET ?? "";
/** How long a call may take before it is answered `unavailable` instead. */
export const TIMEOUT_MS = 5_000;
/** For the calls that run git upstream — a compare or a commit is not a row read. */
export const SLOW_TIMEOUT_MS = 15_000;

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
  | { ok: false; kind: "conflict" | "invalid" | "unauthorized" | "forbidden" | "notFound" | "unavailable"; message: string };

async function callAgainst<T>(
  base: string,
  path: string,
  init: RequestInit & { token?: string; asUser?: string },
): Promise<ApiResult<T>> {
  // `RUSTIC_GIT_ADMIN_FIXTURES=1` answers READS from a seeded module instead of the network, so
  // every superadmin screen renders with realistic data on a laptop with no cluster (spec §C: the
  // screens are verified by screenshot before merge). It sits in `callAgainst` rather than in
  // `adminCall` because three of the console's reads — the superadmin list, the regions list and
  // the default quotas — go to the ordinary host, and a guard that covered only the admin one
  // would leave three sections blank. Unseeded paths answer `undefined` and fall through, so the
  // rest of the app is untouched; the flag is unset in every deployment.
  //
  // Only GET is faked: a decision, a roll or a drain must still reach the real api, because a
  // write that "succeeds" against nothing is a screenshot that lies.
  if (process.env.RUSTIC_GIT_ADMIN_FIXTURES === "1" && (init.method ?? "GET") === "GET") {
    const seeded = fixtureFor(path);
    if (seeded !== undefined) return { ok: true, value: seeded as T };
  }

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
    // Bounded: a hung api pod must not pin a render, or every refresh stacks another one until
    // the heap is gone. A timeout is the same answer as an unreachable server. Callers that do
    // real work upstream (a compare, a commit) pass a longer `signal`.
    res = await fetch(`${base}${path}`, {
      signal: AbortSignal.timeout(TIMEOUT_MS),
      ...init,
      headers,
      cache: "no-store",
    });
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
  // 422 means a value the person typed is unusable, and the api's sentence names WHICH one —
  // showing "the service is unavailable" for that would hide the only useful part.
  if (res.status === 422) {
    let named = message;
    try {
      named = (JSON.parse(message) as { error?: string }).error ?? message;
    } catch {
      // Not the JSON envelope; the raw text is still better than a generic sentence.
    }
    return { ok: false, kind: "invalid", message: named };
  }
  if (res.status === 401) return { ok: false, kind: "unauthorized", message };
  // Signed in, a member, and still refused: the role is not enough. The api says
  // which role it wanted, and that sentence is for the person.
  if (res.status === 403) return { ok: false, kind: "forbidden", message };
  // The api answers 404 for a namespace the caller may not act in, deliberately:
  // whether it exists is not theirs to learn. The page renders it as one too.
  if (res.status === 404) return { ok: false, kind: "notFound", message };
  return { ok: false, kind: "unavailable", message: "The service is unavailable. Try again." };
}

function call<T>(path: string, init: RequestInit & { token?: string; asUser?: string }): Promise<ApiResult<T>> {
  return callAgainst<T>(BASE, path, init);
}

/** Every call the /admin area makes. Same shape as `call`, against the admin host — never the
 *  ordinary one, so an admin page cannot accidentally fall back to a route that does not exist
 *  there (it would 404, not silently authorize as an ordinary user, but the intent is clearer
 *  with its own function). */
function adminCall<T>(path: string, init: RequestInit & { token?: string; asUser?: string }): Promise<ApiResult<T>> {
  return callAgainst<T>(ADMIN_BASE, path, init);
}

/** Records the person and returns their token. Called once, at sign-in. */
export function signIn(email: string, name: string) {
  return call<SignIn>("/v1/users", {
    method: "POST",
    asUser: email,
    body: JSON.stringify({ email, name }),
  });
}

/** Mint a magic sign-in link for `email`. Peer-authenticated: nobody is signed in yet. The
 *  token comes back once and goes into the email; the api keeps only its hash. `clientIp` is
 *  the browser's address as the ingress reported it — the api's per-address bucket on this
 *  route would otherwise see every request as coming from this pod. */
export function requestSignInLink(email: string, clientIp?: string) {
  return call<{ token: string; email: string }>("/v1/signin/email", {
    method: "POST",
    asUser: email,
    headers: clientIp ? { "x-real-ip": clientIp } : undefined,
    body: JSON.stringify({ email }),
  });
}

/** Spend a link. 404 for spent, expired or invented alike. */
export function redeemSignInLink(token: string) {
  return call<{ email: string }>(`/v1/signin/email/${encodeURIComponent(token)}`, {
    method: "POST",
    asUser: "-",
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

const teamPath = (slug: string) => `/v1/teams/${encodeURIComponent(slug)}`;

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
  /** An ssh key's public line, kept so the fleet can build `authorized_keys`. Empty for keys
   *  added before it was kept — those still clone over ssh but cannot reach a workspace. */
  material?: string;
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

/** One CLI login: the device that asked for it, and when it stops working on its own. */
export type ApiCliToken = { id: string; name: string; createdAt: string; expiresAt: string };

/** Defaults to the caller's own handle — a CLI login is personal, so no owner is passed. */
export function listCliTokens(token: string) {
  return call<ApiCliToken[]>("/v1/cli/tokens", { method: "GET", token });
}

export function revokeCliToken(token: string, id: string) {
  return call<void>(`/v1/cli/tokens/${encodeURIComponent(id)}`, { method: "DELETE", token });
}

/** The machine waiting on a device code. Read before the approval page offers a button: the one
 *  check a person can make is "is this my terminal", and that needs the device on screen. */
export type ApiPendingCode = { device: string; expiresAt: string };

export function pendingCliCode(token: string, code: string) {
  return call<ApiPendingCode>(`/v1/cli/code/${encodeURIComponent(code)}`, { method: "GET", token });
}

/** Approves a device code as the signed-in person. 404 covers unknown, expired and
 *  already-approved alike — deliberately, so a guesser learns nothing. */
export function approveCliCode(token: string, code: string) {
  return call<void>("/v1/cli/approve", { method: "POST", token, body: JSON.stringify({ code }) });
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

/** The key the platform issued, which every workspace of the owner's carries. Unlike
 *  `/v1/keys` there is at most one, and it is generated on first read. */
export type ApiPlatformKey = { public: string; fingerprint: string };

export function platformKey(token: string, owner: string) {
  return call<ApiPlatformKey>(`/v1/platform-key?owner=${encodeURIComponent(owner)}`, {
    method: "GET",
    token,
  });
}

/** Replaces the key and revokes the old one — there is no way to keep both. */
export function regeneratePlatformKey(token: string, owner: string) {
  return call<ApiPlatformKey>(`/v1/platform-key?owner=${encodeURIComponent(owner)}`, {
    method: "POST",
    token,
  });
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
  return call<ApiComparison>(`${repoPath(owner, name)}/compare?${q}`, {
    method: "GET",
    token,
    signal: AbortSignal.timeout(SLOW_TIMEOUT_MS),
  });
}

// ── workspaces / environments / volumes ─────────────────────────────────

/** Mirrors `crates/workspaces/src/model.rs::WsState` — lowercase on the wire. */
export type WsState = "creating" | "ready" | "stopped" | "error" | "deleted";
export type EnvState = "creating" | "running" | "stopped" | "error" | "deleted";

export type ApiWorkspace = {
  id: string;
  owner: string;
  /** Empty for personal. */
  team: string;
  name: string;
  region: string;
  state: WsState;
  /** The container image `ws-{id}` runs, the platform image unless set at create — the tools come from Nix. */
  image: string;
  placement: string | null;
  volume: string | null;
  quota_gb: number;
  /** nixpkgs attribute names the workspace declares — what was ASKED for, not what is installed. */
  packages: string[];
  /** The platform's base set every workspace gets on top of its own list; shown, not edited. */
  base_packages?: string[];
  /** The `PackagesReady` condition; absent until the reconciler has reported on the list. */
  packages_status?: { ready: boolean; reason: string; message: string } | null;
  /** Present once the workspace has an sshd with a host key — i.e. once it can be reached.
   *  Absent while it is coming up, and for a stopped one. */
  ssh?: { gateway: string; host_key: string } | null;
  /** The `Replicated` condition, verbatim from the node that wrote it — "safe to start anywhere"
   *  vs "still copying". Absent while running: it is only computed for a stopped parent. */
  replicated?: { ready: boolean; reason: string; message: string } | null;
  /** `Degraded/NodeDead` — the source's node is down, so a start is refused and a clone is the way on. */
  degraded?: { ready: boolean; reason: string; message: string } | null;
  /** `Decommissioning/NodeLeaving` — the node is being retired; the next start lands elsewhere. */
  decommissioning?: { ready: boolean; reason: string; message: string } | null;
  /** What a clone was grafted onto, and whether that cut predates the source's node going down.
   *  Only a clone response carries it — an environment clone never does. */
  based_on?: { snapshot: string; at?: string | null; age_seconds: number; interrupted: boolean } | null;
};

export type ApiMount = { folder: string; path: string };
/** `model::Service`. `ports` is `#[serde(default)]` on the Rust side, so an environment document
 *  written before ports existed deserializes as an empty list — the wire always carries the key. */
export type ApiService = { name: string; image: string; command: string[]; env: Record<string, string>; mounts: ApiMount[]; ports: number[] };

export type ApiEnvironment = {
  id: string;
  owner: string;
  name: string;
  region: string;
  state: EnvState;
  placement: string | null;
  volume: string | null;
  services: ApiService[];
  /** The snapshot the volume last landed on, when an in-place restore put one there — only
   *  `GET /v1/environments/{id}` fills it in. Absent means "current" is simply the newest record. */
  restored_to?: string | null;
  /** When that restore was asked for: a record pushed after it, descending from `restored_to`,
   *  is where the environment has moved on to; one from before is a sibling branch. */
  restore_requested_at?: string | null;
  /** Why this environment is mid-restore (`Draining`, `Restoring`, `Requested`), or absent. */
  restoring?: string | null;
  /** The `Replicated` condition, verbatim from the node that wrote it — "safe to start anywhere"
   *  vs "still copying". Absent while running: it is only computed for a stopped parent. */
  replicated?: { ready: boolean; reason: string; message: string } | null;
  /** `Degraded/NodeDead` — the source's node is down, so a start is refused and a clone is the way on. */
  degraded?: { ready: boolean; reason: string; message: string } | null;
  /** `Decommissioning/NodeLeaving` — the node is being retired; the next start lands elsewhere. */
  decommissioning?: { ready: boolean; reason: string; message: string } | null;
};

/** The caller's workspaces in `team`, or their personal ones when it is absent or their own
 *  handle. A team page never shows personal work and the personal page never shows a team's:
 *  each (team, person) pair is its own namespace on the cluster. */
export function listWorkspaces(token: string, team?: string) {
  const q = team ? `?team=${encodeURIComponent(team)}` : "";
  return call<ApiWorkspace[]>(`/v1/workspaces${q}`, { method: "GET", token });
}

/** `crates/workspaces/src/api.rs::NewWorkspace`. `repo`/`branch` come as a pair — the api
 *  refuses a repo without a branch, since "the default branch" is a different workspace
 *  depending on when it was made. */
export function createWorkspace(
  token: string,
  body: {
    team?: string;
    name: string;
    region: string;
    quota_gb: number;
    image?: string;
    repo?: string;
    branch?: string;
    packages?: string[];
  },
) {
  return call<ApiWorkspace>("/v1/workspaces", { method: "POST", token, body: JSON.stringify(body) });
}

/** Replace the declared package list. The whole list, not a delta: the api merge-patches
 *  `spec.packages` with exactly what is sent. */
export function setWorkspacePackages(token: string, id: string, packages: string[]) {
  return call<ApiWorkspace>(`/v1/workspaces/${encodeURIComponent(id)}`, {
    method: "PATCH",
    token,
    body: JSON.stringify({ packages }),
  });
}

/** `crates/workspaces/src/model.rs::Region`, narrowed to what the app reads (`listRegions`'s only
 *  caller uses just `status` and `id`); `name`, `storage_account`, `blob_container` have no
 *  reader anywhere. */
export type ApiRegion = { id: string; status: string };

export function listRegions(token: string) {
  return call<ApiRegion[]>("/v1/regions", { method: "GET", token });
}

export function listEnvironments(token: string, owner?: string) {
  const qs = owner ? `?owner=${encodeURIComponent(owner)}` : "";
  return call<ApiEnvironment[]>(`/v1/environments${qs}`, { method: "GET", token });
}

/** One environment, by id. 404 when it is gone — which is exactly what an ARCHIVED row is, so
 *  the environment page falls back to the volume's snapshot records for its name and services. */
export function getEnvironment(token: string, id: string) {
  return call<ApiEnvironment>(`/v1/environments/${encodeURIComponent(id)}`, { method: "GET", token });
}

/** Snapshot + upload + register, atomically. The answer is only the REQUEST's id: the snapshot
 *  record appears in the volume's history when the push lands, which is what the page polls for. */
export function pushEnvironment(token: string, id: string, message?: string) {
  return call<{ id: string }>(`/v1/environments/${encodeURIComponent(id)}/push`, {
    method: "POST",
    token,
    body: message ? JSON.stringify({ message }) : undefined,
  });
}

/** Put a past snapshot back into THIS environment's own volume, rather than into a new one.
 *  202 with nothing to read: the controllers scale the services down, swap the subvolume and
 *  bring them back, and the environment's own state is where that progress shows. */
export function restoreEnvironmentInPlace(token: string, id: string, snapshotId: string) {
  return call<ApiEnvironment>(`/v1/environments/${encodeURIComponent(id)}/restore-in-place`, {
    method: "POST",
    token,
    body: JSON.stringify({ snapshot_id: snapshotId }),
  });
}

export function deleteWorkspace(token: string, id: string) {
  return call<ApiWorkspace>(`/v1/workspaces/${encodeURIComponent(id)}`, { method: "DELETE", token });
}

export function deleteEnvironment(token: string, id: string) {
  return call<ApiEnvironment>(`/v1/environments/${encodeURIComponent(id)}`, { method: "DELETE", token });
}

/** The one mutating verb: snapshot + upload + register, atomically. `message` is optional. */
export function pushWorkspace(token: string, id: string, message?: string) {
  return call<ApiWorkspace>(`/v1/workspaces/${encodeURIComponent(id)}/push`, {
    method: "POST",
    token,
    body: message ? JSON.stringify({ message }) : undefined,
  });
}

/** The one local-copy verb — the server picks `clone_local` vs `clone_running` itself,
 *  keyed on whether the source's container is running. */
export function cloneWorkspace(token: string, id: string, name: string) {
  return call<ApiWorkspace>(`/v1/workspaces/${encodeURIComponent(id)}/clone`, {
    method: "POST",
    token,
    body: JSON.stringify({ name }),
  });
}

/** Same one local-copy verb as `cloneWorkspace`, for an environment — pauses its compose
 *  project (not a single container) around the copy. */
export function cloneEnvironment(token: string, id: string, name: string) {
  return call<ApiEnvironment>(`/v1/environments/${encodeURIComponent(id)}/clone`, {
    method: "POST",
    token,
    body: JSON.stringify({ name }),
  });
}

/** New workspace grafted onto an explicit past snapshot, not the source's
 *  current tip — see `crates/workspaces/src/api.rs::restore_ws`. */
export function restoreWorkspace(
  token: string,
  name: string,
  snapshotId: string,
  // Each field OVERRIDES the snapshot's own frozen definition, so an omitted one must stay off
  // the wire entirely — sending `image: undefined`'s JSON hole would read as "no image".
  extra?: { image?: string; packages?: string[] },
) {
  return call<ApiWorkspace>(`/v1/workspaces/restore`, {
    method: "POST",
    token,
    // The snapshot id is enough: the api tier finds the volume it belongs to. No source
    // workspace is named, because a restore is most wanted when there no longer is one.
    body: JSON.stringify({
      name,
      snapshot_id: snapshotId,
      ...(extra?.image ? { image: extra.image } : {}),
      ...(extra?.packages !== undefined ? { packages: extra.packages } : {}),
    }),
  });
}

/** New environment grafted onto a past snapshot — `restore_env`'s twin of `restoreWorkspace`.
 *  A snapshot freezes the service list beside the data, so omitting `services` restores what was
 *  pushed; an empty list means the same on the server. Only a non-empty list overrides. */
export function restoreEnvironment(token: string, name: string, snapshotId: string, services?: ApiService[]) {
  return call<ApiEnvironment>(`/v1/environments/restore`, {
    method: "POST",
    token,
    body: JSON.stringify({ name, snapshot_id: snapshotId, ...(services ? { services } : {}) }),
  });
}

export function startWorkspace(token: string, id: string) {
  return call<void>(`/v1/workspaces/${encodeURIComponent(id)}/start`, { method: "POST", token });
}

export function stopWorkspace(token: string, id: string) {
  return call<{ warning?: string }>(`/v1/workspaces/${encodeURIComponent(id)}/stop`, { method: "POST", token });
}

export function startEnvironment(token: string, id: string) {
  return call<ApiEnvironment>(`/v1/environments/${encodeURIComponent(id)}/start`, { method: "POST", token });
}

export function stopEnvironment(token: string, id: string) {
  return call<ApiEnvironment & { warning?: string }>(`/v1/environments/${encodeURIComponent(id)}/stop`, { method: "POST", token });
}

/** `crates/workspaces/src/api.rs::VolumeSummary` — one row per VOLUME that has ever been
 *  pushed, read from the server tier's registry rather than from live workspaces. A snapshot
 *  outlives the thing it was taken of, so a row can name a source that no longer exists. */
export type ApiVolumeSummary = {
  name: string;
  kind: "workspace" | "environment";
  volume: string | null;
  /** What the source was called; the volume id when a record carries no provenance. */
  display_name: string;
  /** The workspace/environment is gone. The snapshots are not. */
  deleted: boolean;
  /** How many PUSHES are on this volume — the only thing keeping it once its workspace or
   *  environment is gone. Sync points are not counted; they are never shown. */
  snapshots: number;
  /** RFC3339 of the newest push; `null` while the only push is still being taken. */
  last_push_at: string | null;
};

/** `kind` narrows to `workspace` or `environment`. The Environments page asks for `environment`
 *  to find its ARCHIVED rows — volumes with snapshots and no live environment left. A workspace's
 *  snapshots are that one person's undo history and are reached only from their own row. */
export function listVolumes(token: string, kind?: "workspace" | "environment", owner?: string) {
  const qs = new URLSearchParams();
  if (kind) qs.set("kind", kind);
  // A team's page must show that team's archived rows and not the caller's personal ones — the
  // same filter `listEnvironments` passes, for the same reason.
  if (owner) qs.set("owner", owner);
  const q = qs.toString();
  return call<ApiVolumeSummary[]>(`/v1/volumes${q ? `?${q}` : ""}`, { method: "GET", token });
}

/** `crates/workspaces/src/api.rs::snapshot_rows` — the volume's SNAPSHOTS, newest
 *  first. Sync points are internal and never appear here. The row also carries
 *  `phase`, left undeclared here — no reader in this app looks at it yet; add it if one needs to.
 *  The wire also carries `lineage` (always `[]`) and `region` (always `""`), left undeclared for
 *  the same reason: nothing reads them, and a type that names a field invites one to. */
export type ApiCommitRecord = {
  id: string;
  /** The definition frozen at push time — `null` for snapshots taken before it was recorded. */
  state: SnapshotState | null;
  /** The snapshot this one was pushed on top of — derived server-side from the blob chain. A push
   *  after an in-place restore grafts onto the restored record, which is what makes a branch. */
  parent?: string | null;
  message?: string;
  /** RFC3339. camelCase because `/history` builds its rows by hand rather than serializing
   *  `CommitRecord` (`crates/workspaces/src/api.rs:2027`); `null` when the object carries no
   *  creation timestamp. Read it through `snapshotTime` (`lib/snapshot.ts`), never by hand. */
  createdAt: string | null;
};

/** Deletes the volume and every `Snapshot` on it. A volume's snapshots are
 *  the only thing keeping it once its workspace or environment is gone, so this is what finally
 *  removes it: the Snapshots section's own "Delete volume". The bytes go with it — each node
 *  holding the subvolume drops it on its next beat. 409 while a working copy still uses it. */
export function deleteVolume(token: string, name: string) {
  return call<void>(`/v1/volumes/${encodeURIComponent(name)}`, { method: "DELETE", token });
}

/** Drops ONE snapshot from a volume's lineage. The environment's disk is untouched — this
 *  removes the record, not the data it points at. */
export function deleteVolumeSnapshot(token: string, name: string, snapshot: string) {
  return call<void>(
    `/v1/volumes/${encodeURIComponent(name)}/snapshots/${encodeURIComponent(snapshot)}`,
    { method: "DELETE", token },
  );
}

export function volumeHistory(token: string, name: string) {
  return call<ApiCommitRecord[]>(`/v1/volumes/${encodeURIComponent(name)}/history`, { method: "GET", token });
}

/** An owner's ceiling and what is against it. Computed by the api on every request — there is no
 *  cached number to be stale. */
export function getQuota(owner: string, token: string) {
  return call<QuotaReport>(`/v1/quota?owner=${encodeURIComponent(owner)}`, { method: "GET", token });
}

export type QuotaRequestDoc = {
  id: string;
  owner: string;
  requested: Partial<Record<QuotaDim, number>>;
  reason: string;
  state: "pending" | "approved" | "denied";
  decidedBy?: string | null;
  decidedAt?: string | null;
  note?: string | null;
  createdAt?: string | null;
};

export type { RequestDoc, RequestKind } from "@/lib/requests";
import type { RequestDoc } from "@/lib/requests";

/** `requestedBy` is set by the api from the session, never sent — a request that could name its
 *  own author is not evidence of who asked. */
export function createRequest(
  body: { owner?: string; kind: string; reason: string } & Record<string, unknown>,
  token: string,
) {
  return call<RequestDoc>("/v1/requests", { method: "POST", token, body: JSON.stringify(body) });
}

/** The caller's own and their teams'; `owner` narrows to one they may act on. */
export function listRequests(owner: string | undefined, token: string) {
  const q = owner ? `?owner=${encodeURIComponent(owner)}` : "";
  return call<RequestDoc[]>(`/v1/requests${q}`, { method: "GET", token });
}

/** The whole queue, every owner and every kind, unioned over the new CRD and the legacy one.
 *  `kind`/`owner`/`state` are server-side narrowing; anything finer stays client-side. */
export function adminListRequests(token: string, filter?: { kind?: string; owner?: string; state?: string }) {
  const q = new URLSearchParams();
  if (filter?.kind) q.set("kind", filter.kind);
  if (filter?.owner) q.set("owner", filter.owner);
  if (filter?.state) q.set("state", filter.state);
  const qs = q.toString();
  return adminCall<RequestDoc[]>(`/admin/requests${qs ? `?${qs}` : ""}`, { method: "GET", token });
}

/** `quota` is the operator's edited grant on a quota decision; `resolution` is REQUIRED on an
 *  `other` approve (there is nothing else for that approve to do) and optional elsewhere. `note`
 *  is required on every deny. */
export function adminDecideRequest(
  id: string,
  decision: "approve" | "deny",
  body: { note?: string; quota?: Partial<Record<QuotaDim, number>>; resolution?: string },
  token: string,
) {
  return adminCall<RequestDoc>(`/admin/requests/${encodeURIComponent(id)}/${decision}`, {
    method: "POST",
    token,
    body: JSON.stringify(body),
  });
}

// ── /admin (a separate host — crates/workspaces/src/api/admin.rs) ──────────

/** The whole queue, every owner — there is no default `owner` filter. `filter.owner`/`filter.state`
 *  are server-side narrowing (Task 2's `?owner=&state=`); anything finer (free text, dimension,
 *  age) stays client-side over the fetched page, per the ladder. */
export function adminListQuotaRequests(token: string, filter?: { owner?: string; state?: string }) {
  const q = new URLSearchParams();
  if (filter?.owner) q.set("owner", filter.owner);
  if (filter?.state) q.set("state", filter.state);
  const qs = q.toString();
  return adminCall<QuotaRequestDoc[]>(`/admin/quota-requests${qs ? `?${qs}` : ""}`, { method: "GET", token });
}

/** `note` required and non-empty (422 otherwise) — a quota write is dangerous per the Global
 *  Constraint, same rule as deny/roll/drain. The api's `WriteQuotaBody` is `{ spec, note }`, the
 *  spec wrapped, so a flat body would parse as a missing spec. */
export function adminWriteQuota(owner: string, spec: Record<QuotaDim, number>, note: string, token: string) {
  return adminCall<Record<QuotaDim, number>>(`/admin/quota/${encodeURIComponent(owner)}`, {
    method: "PUT",
    token,
    body: JSON.stringify({ spec, note }),
  });
}

/** `GET /admin/owners` — every owner's usage against their limit, tightest-first (the api's own
 *  sort, never re-sorted here except by the list's own controls). */
export type OwnerRow = {
  owner: string;
  isTeam: boolean;
  limit: Record<QuotaDim, number>;
  used: Record<QuotaDim, number>;
  /** `"own"` when the owner has an explicit `Quota`, `"default"` when riding the fallback table. */
  source: "own" | "default";
  /** A `QuotaRequest` still pending for this owner. */
  pending: boolean;
};

export function adminOwners(token: string) {
  return adminCall<OwnerRow[]>("/admin/owners", { method: "GET", token });
}

/** `GET /admin/owners/{slug}` — everything the detail page shows without a second click.
 *  `requests` and `audit` are already truncated server-side (last 5 / last 10); the page links to
 *  the Requests and Audit areas, filtered to this owner, for the rest. */
export type OwnerDetail = OwnerRow & {
  workspaces: ApiWorkspace[];
  environments: ApiEnvironment[];
  volumes: ApiVolumeSummary[];
  requests: QuotaRequestDoc[];
  audit: AuditEntry[];
};

export function adminOwnerDetail(slug: string, token: string) {
  return adminCall<OwnerDetail>(`/admin/owners/${encodeURIComponent(slug)}`, { method: "GET", token });
}

// `/admin/workspaces/{id}` and `/admin/environments/{id}` reuse the SAME handlers `/v1` calls for
// the caller's own objects, just with the owner taken from the object rather than the token — see
// `crates/workspaces/src/api/admin.rs`'s "cross-owner list / stop / delete" section, wrapped there
// to take the note every admin write carries — acting on somebody else's working copy is the
// loudest thing this console does, and the api 422s an empty one.
export function adminStopWorkspace(id: string, token: string, note: string) {
  return adminCall<ApiWorkspace>(`/admin/workspaces/${encodeURIComponent(id)}/stop`, { method: "POST", token, body: JSON.stringify({ note }) });
}

export function adminDeleteWorkspace(id: string, token: string, note: string) {
  return adminCall<ApiWorkspace>(`/admin/workspaces/${encodeURIComponent(id)}`, { method: "DELETE", token, body: JSON.stringify({ note }) });
}

export function adminStopEnvironment(id: string, token: string, note: string) {
  return adminCall<ApiEnvironment>(`/admin/environments/${encodeURIComponent(id)}/stop`, { method: "POST", token, body: JSON.stringify({ note }) });
}

export function adminDeleteEnvironment(id: string, token: string, note: string) {
  return adminCall<ApiEnvironment>(`/admin/environments/${encodeURIComponent(id)}`, { method: "DELETE", token, body: JSON.stringify({ note }) });
}

export type AdminNode = { name: string; ready: boolean; decommission: boolean; decommissionStatus: string | null };

export function adminAudit(token: string, filter: AuditFilter) {
  return adminCall<AuditPage>(`/admin/audit${auditQueryString(filter)}`, { method: "GET", token });
}

/** Raw `Response`, not `ApiResult` — the CSV export route streams this straight through to the
 *  browser rather than parsing it, so it needs the actual body and status, not `adminCall`'s
 *  JSON-shaped envelope. The one caller that talks to `ADMIN_BASE` directly. */
export function adminAuditCsv(token: string, filter: AuditFilter): Promise<Response> {
  return fetch(`${ADMIN_BASE}/admin/audit.csv${auditQueryString(filter)}`, {
    headers: { authorization: `Bearer ${token}` },
    cache: "no-store",
  });
}

/** One row of `GET /admin/settings/schema` — `crates/workspaces/src/api/admin/schema.rs::Row`.
 *  `range` is `null` for a bool/string field, where a min/max means nothing. */
export type SettingsSchemaRow = {
  name: string;
  description: string;
  unit: string;
  range: { min: number; max: number } | null;
  mark: "live" | "boot";
  readers: string[];
  default: unknown;
  env: string | null;
};

export type SettingsSchema = { central: SettingsSchemaRow[]; cluster: SettingsSchemaRow[] };

export function adminSettingsSchema(token: string) {
  return adminCall<SettingsSchema>("/admin/settings/schema", { method: "GET", token });
}

/** `crates/core/settings::StoredCentralSettings` — every field `Option`, `null`/absent meaning
 *  "never set" (falls back to env, then the built-in default). Read-only here: keyed dynamically
 *  against `SettingsSchemaRow.name` rather than re-typing every field, same as `adminClusterSettings`. */
export function adminCentralSettings(token: string) {
  return adminCall<Record<string, unknown>>("/admin/settings/central", { method: "GET", token });
}

/** `GET /admin/settings/clusters/{region}` returns the whole `ClusterSettings` CR; only `.spec`
 *  (same `stored ?? env ?? default` fields as the central document) matters for display. */
export function adminClusterSettings(region: string, token: string) {
  return adminCall<{ spec: Record<string, unknown> }>(`/admin/settings/clusters/${encodeURIComponent(region)}`, {
    method: "GET",
    token,
  });
}

export function createRegion(body: { id: string; name: string; note: string }, token: string) {
  return adminCall<{ id: string; name: string; status: string }>("/admin/regions", {
    method: "POST",
    token,
    body: JSON.stringify(body),
  });
}

// ── clusters (admin host — crates/workspaces/src/api/admin/clusters.rs) ──

/** `GET /admin/clusters` — one row per region, everything the Clusters list card needs without a
 *  second click. `settingsStatus` is an open string (`"present"`, or `"stale (lag N)"` when the
 *  agents have not caught up) — render via `lib/clusters.ts::settingsStatusTone` rather than
 *  matching it here. */
export type AdminClusterRow = {
  region: string;
  status: string;
  agentsReady: number;
  agentsDesired: number;
  nodesReady: number;
  nodesTotal: number;
  draining: number;
  workingCopies: number;
  settingsStatus: string;
};

export function adminClusters(token: string) {
  return adminCall<AdminClusterRow[]>("/admin/clusters", { method: "GET", token });
}

/** `GET /admin/clusters/{region}` node row — `NodeDoc`'s four fields flattened, plus what a drain
 *  is waiting for: live working copies and replicas held on this node. */
export type AdminClusterNode = {
  name: string;
  ready: boolean;
  decommission: boolean;
  decommissionStatus: string | null;
  workingCopies: number;
  replicasHeld: number;
};

export type AdminClusterDetail = {
  region: string;
  status: string;
  nodes: AdminClusterNode[];
  workloads: WorkloadDoc[];
  settings: Record<string, unknown>;
};

export function adminClusterDetail(region: string, token: string) {
  return adminCall<AdminClusterDetail>(`/admin/clusters/${encodeURIComponent(region)}`, { method: "GET", token });
}

/** Activate/deactivate — server-side apply of the same shape `createRegion` writes. `note` is
 *  required only for `"inactive"` (a required reason on the loud half, per the Global Constraint);
 *  the api 422s a missing one itself. */
export function adminSetRegionStatus(region: string, status: "active" | "inactive", note: string, token: string) {
  return adminCall<{ id: string; name: string; status: string }>(
    `/admin/clusters/${encodeURIComponent(region)}/status`,
    { method: "PUT", token, body: JSON.stringify({ status, note }) },
  );
}

function nodeVerb(verb: "drain" | "undrain" | "decommission", region: string, node: string, reason: string, token: string) {
  return adminCall<AdminNode>(
    `/admin/clusters/${encodeURIComponent(region)}/nodes/${encodeURIComponent(node)}/${verb}`,
    { method: "POST", token, body: JSON.stringify({ reason }) },
  );
}

/** Sets the label the agent already watches — the drain itself runs on the node's own beat
 *  (CLAUDE.md, "Workspaces and environments"). `reason` is required; the api 422s an empty one. */
export function adminDrainNode(region: string, node: string, reason: string, token: string) {
  return nodeVerb("drain", region, node, reason, token);
}

/** A real abort — clears both the label and any `decommission-status` stamp, so a drain that
 *  never finished cannot leave a stale gate open for decommission. */
export function adminUndrainNode(region: string, node: string, reason: string, token: string) {
  return nodeVerb("undrain", region, node, reason, token);
}

/** Cordons the node (`spec.unschedulable`) and nothing else — the console never deletes the VM.
 *  409 "not drained yet" when `decommissionStatus` hasn't reached `"drained …"`. */
export function adminDecommissionNode(region: string, node: string, reason: string, token: string) {
  return nodeVerb("decommission", region, node, reason, token);
}

// ── workloads (admin host — crates/workspaces/src/api/admin/settings.rs) ─

/** `crates/workspaces/src/api/workloads.rs::WorkloadDoc`. `scope` serializes as a plain string
 *  now (`Scope`'s hand-written `Serialize`) — `"central"` or the bare region id — the fix for the
 *  internally-tagged-enum panic Task 7 hit is in (`fd9e851a`). */
export type WorkloadDoc = {
  scope: string;
  name: string;
  kind: "statefulset" | "deployment" | "daemonset";
  image: string | null;
  ready: number;
  desired: number;
  rolloutState: "RollingOut" | "Stable";
  lastRoll: { by: string; at: string; reason: string } | null;
};

export function listWorkloads(token: string) {
  return adminCall<WorkloadDoc[]>("/admin/workloads", { method: "GET", token });
}

/** `POST /admin/workloads/{scope}/{name}/roll` — the one write the Workloads tab offers, a
 *  manual restart with a required reason (`crates/workspaces/src/api/admin.rs::roll_workload_route`
 *  400s an empty one). `scope` is `"central"` or a region id, same encoding as `WorkloadDoc.scope`. */
export function rollWorkload(scope: string, name: string, reason: string, token: string) {
  return adminCall<WorkloadDoc>(`/admin/workloads/${encodeURIComponent(scope)}/${encodeURIComponent(name)}/roll`, {
    method: "POST",
    token,
    body: JSON.stringify({ reason }),
  });
}

/** `crates/workspaces/src/api/admin/monitoring.rs::SignalRow` — one catalogue rule
 *  (`deploy/alerts.md`), evaluated by scraping every pod's `/metrics` on the request path rather
 *  than through Prometheus. `detail` is the observed numbers behind `state`, or why a rule that
 *  needs a window this process cannot see stayed `unknown` — never guessed as `ok`. */
export type SignalRow = {
  alert: string;
  state: "firing" | "ok" | "unknown";
  why: string;
  detail: string | null;
  /** Which region this evaluation scraped, or `null` for a fleet-wide (central) rule — lets the
   *  toolbar group a per-region catalogue without a second fetch. */
  region: string | null;
};

/** `Restarts` in the same handler — container restart count since each pod started (Kubernetes
 *  exposes no 1 h window), summed per KNOWN central workload. */
export type SignalRestarts = { workload: string; restarts: number };

export type SignalsResponse = {
  signals: SignalRow[];
  restarts: SignalRestarts[];
  // Field names are the wire ones verbatim — `SignalsResponse` has no `rename_all`, unlike
  // most admin responses.
  scrape_failures: [string, string][];
  pods_listed: number;
  /** Absent (not null — `skip_serializing_if`) unless `RUSTIC_GIT_HYPERDX_URL` is configured, so
   *  a monitoring page never renders a dead link. */
  hyperdx_url?: string;
};

export function adminMonitoringSignals(token: string) {
  return adminCall<SignalsResponse>("/admin/monitoring/signals", { method: "GET", token });
}

/** `crates/workspaces/src/api/admin/overview.rs::AttentionItem` — no `rename_all`, so its three
 *  fields are already the wire shape verbatim. */
export type AttentionItem = { kind: string; detail: string; href: string };

/** `RegionFleet`/`FleetNumbers`, both `rename_all = "camelCase"`. */
export type RegionFleet = { owners: number; workspaces: number; environments: number; snapshots: number; diskGb: number };
export type FleetNumbers = {
  owners: number;
  workspaces: number;
  environments: number;
  snapshots: number;
  diskGbTotal: number;
  perRegion: Record<string, RegionFleet>;
};

/** `Overview`, `rename_all = "camelCase"`. `errors` is `skip_serializing_if = "Vec::is_empty"`,
 *  so it is absent rather than `[]` when every sub-source read cleanly. */
export type Overview = {
  pendingRequests: QuotaRequestDoc[];
  attention: AttentionItem[];
  recentAudit: AuditEntry[];
  fleet: FleetNumbers;
  errors?: string[];
};

export function adminOverview(token: string) {
  return adminCall<Overview>("/admin/overview", { method: "GET", token });
}

// ── history (admin host — crates/workspaces/src/api/admin/history.rs, spec §A5) ─

/** `GET /admin/history/{series}`. Deliberately NOT an `ApiResult`: history is optional
 *  infrastructure (a `503 history unavailable` when the admin process has no ClickHouse URL), and
 *  a page that reads five series must not have five failure branches. Every non-ok answer is the
 *  same flat placeholder, which every tile already knows how to render. */
export async function adminSeries(
  name: SeriesName,
  opts: { range?: string; step?: string; region?: string; owner?: string; dimension?: string },
  token: string,
): Promise<HistorySeries> {
  const qs = new URLSearchParams();
  qs.set("range", opts.range ?? "7d");
  qs.set("step", opts.step ?? "1d");
  if (opts.region) qs.set("region", opts.region);
  if (opts.owner) qs.set("owner", opts.owner);
  if (opts.dimension) qs.set("dimension", opts.dimension);
  const r = await adminCall<Omit<HistorySeries, "available">>(
    `/admin/history/${encodeURIComponent(name)}?${qs}`,
    { method: "GET", token },
  );
  return r.ok ? { ...r.value, available: true } : FLAT;
}

/** `GET /admin/history/events` — the timeline and the activity feed. This one keeps its
 *  `ApiResult`: a section whose whole content is events says so in its own empty state. */
export function adminHistoryEvents(
  q: { kind?: string; owner?: string; region?: string; from?: string; to?: string; cursor?: string; limit?: number },
  token: string,
) {
  const qs = new URLSearchParams();
  for (const [k, v] of Object.entries(q)) if (v !== undefined && v !== "") qs.set(k, String(v));
  return adminCall<{ events: HistoryEvent[]; cursor: string | null }>(
    `/admin/history/events${qs.toString() ? `?${qs}` : ""}`,
    { method: "GET", token },
  );
}

/** Display-only slice of the central document — `crates/api/src/lib.rs::settings_central`, the
 *  UNAUTHENTICATED route on the ordinary api host (not `/admin`), so `lib/clone.ts` can call it
 *  without a signed-in caller's token. Blank fields mean "never set", the same fallback-to-env
 *  contract that route's own doc comment states. */
export type PublicCentralSettings = { cloneHost: string; sshHost: string; sshPort: number; registryHost: string };

export function getPublicCentralSettings() {
  return call<PublicCentralSettings>("/v1/settings/central", { method: "GET" });
}

// ── superadmins (server tier, not the admin host — crates/api/src/teams.rs) ─

export type SuperAdmin = { _id: string; addedAt: string; addedBy: string };

export function listSuperadmins(token: string) {
  return call<SuperAdmin[]>("/api/admin/superadmins", { method: "GET", token });
}

// A required note (Global Constraint: reason on every write except approve) — the api 422s an
// empty one, and that message surfaces to the form rather than being swallowed.
export function addSuperadmin(user: string, token: string, note: string) {
  return call<undefined>(`/api/admin/superadmins/${encodeURIComponent(user)}`, {
    method: "POST",
    token,
    body: JSON.stringify({ note }),
  });
}

export function removeSuperadmin(user: string, token: string, note: string) {
  return call<undefined>(`/api/admin/superadmins/${encodeURIComponent(user)}`, {
    method: "DELETE",
    token,
    body: JSON.stringify({ note }),
  });
}

