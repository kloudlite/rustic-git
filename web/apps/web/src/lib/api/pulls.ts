import type { Commit } from "@/lib/browse";
import { SLOW_TIMEOUT_MS, call } from "./client";

/**
 * Pull requests: open, comment, merge, close, compare, delete a branch.
 */
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

export const repoPath = (owner: string, name: string) =>
  `/v1/repos/${encodeURIComponent(owner)}/${encodeURIComponent(name)}`;

export function listPulls(token: string, owner: string, name: string) {
  // ponytail: flat 100 cap, no paging; add ?page= when a repo outgrows it
  return call<ApiPull[]>(`${repoPath(owner, name)}/pulls?limit=100`, { method: "GET", token });
}

/** Delete a branch, compare-and-swap on the oid the page listed. A refusal (protected,
 *  default, an open pull request's head, a branch that moved) is a 409 whose sentence is
 *  the api's own and is shown as-is. */
export function deleteBranch(token: string, owner: string, name: string, branch: string, oid: string) {
  return call<void>(
    `${repoPath(owner, name)}/branches/${encodeURIComponent(branch)}?oid=${encodeURIComponent(oid)}`,
    { method: "DELETE", token },
  );
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
