import "server-only";
import type { ApiResult } from "@/lib/api";

/**
 * The read side of a repo: refs, trees, blobs, log, commits.
 *
 * Separate from `lib/api` because it is a different tier with a different shape.
 * `/v1/*` is the api server's own data (people, teams, the repo index) and answers
 * JSON objects; `/api/{owner}/{name}/*` is a cached view of what the git fleet
 * holds, keyed by object id, and every answer is immutable except `refs`.
 */

const BASE = (process.env.RUSTIC_GIT_API_URL ?? "http://rustic-git-api").replace(/\/$/, "");

export type Ref = { name: string; oid: string; kind: "branch" | "tag" };
export type Entry = {
  name: string;
  mode: number;
  kind: "blob" | "tree" | string;
  oid: string;
  /** Blobs only — a tree has no meaningful size. */
  size: number | null;
};
export type Commit = {
  oid: string;
  parents: string[];
  author: string;
  /** Unix seconds. */
  time: number;
  message: string;
};
export type Blob = { oid: string; bytes_base64: string; truncated: boolean };
export type CommitDetail = Commit & { diff: string };

async function get<T>(path: string, token?: string): Promise<ApiResult<T>> {
  const headers = new Headers();
  // The session token. The api tier resolves it to a membership and presents the
  // caller upstream; an anonymous request still works for a public repo.
  if (token) headers.set("authorization", `Bearer ${token}`);

  let res: Response;
  try {
    res = await fetch(`${BASE}${path}`, { headers, cache: "no-store" });
  } catch {
    return { ok: false, kind: "unavailable", message: "The service is unavailable. Try again." };
  }
  if (res.ok) return { ok: true, value: (await res.json()) as T };
  // A private repo and a missing repo are deliberately indistinguishable here,
  // and the page must keep them that way rather than reporting which it was.
  if (res.status === 404) return { ok: false, kind: "notFound", message: "not found" };
  if (res.status === 401) return { ok: false, kind: "unauthorized", message: "sign in" };
  return { ok: false, kind: "unavailable", message: "The service is unavailable. Try again." };
}

const seg = (s: string) => encodeURIComponent(s);
/** A path keeps its slashes — it is many segments — but every segment is escaped. */
const filePath = (p: string) => p.split("/").filter(Boolean).map(seg).join("/");

export function refs(token: string | undefined, owner: string, repo: string) {
  return get<Ref[]>(`/api/${seg(owner)}/${seg(repo)}/refs`, token);
}

export function tree(token: string | undefined, owner: string, repo: string, oid: string, path = "") {
  const tail = path ? `/${filePath(path)}` : "";
  return get<Entry[]>(`/api/${seg(owner)}/${seg(repo)}/tree/${seg(oid)}${tail}`, token);
}

export function blob(token: string | undefined, owner: string, repo: string, oid: string, path: string) {
  return get<Blob>(`/api/${seg(owner)}/${seg(repo)}/blob/${seg(oid)}/${filePath(path)}`, token);
}

export function log(token: string | undefined, owner: string, repo: string, oid: string, page = 1) {
  return get<Commit[]>(`/api/${seg(owner)}/${seg(repo)}/log/${seg(oid)}?page=${page}`, token);
}

export function commit(token: string | undefined, owner: string, repo: string, oid: string) {
  return get<CommitDetail>(`/api/${seg(owner)}/${seg(repo)}/commit/${seg(oid)}`, token);
}

/** The ref a repo opens on: `main`, else `master`, else whatever branch exists. */
export function defaultBranch(list: Ref[]): Ref | undefined {
  const branches = list.filter((r) => r.kind === "branch");
  return (
    branches.find((b) => b.name === "refs/heads/main") ??
    branches.find((b) => b.name === "refs/heads/master") ??
    branches[0]
  );
}

export const shortRef = (name: string) => name.replace(/^refs\/(heads|tags)\//, "");
export const shortOid = (oid: string) => oid.slice(0, 7);

/** Blobs travel as base64 because a blob is arbitrary binary. Text is decoded
 *  here; anything with a NUL byte is treated as binary and never rendered. */
export function decodeBlob(b: Blob): { text: string; binary: false } | { binary: true } {
  const bytes = Buffer.from(b.bytes_base64, "base64");
  if (bytes.includes(0)) return { binary: true };
  return { text: bytes.toString("utf8"), binary: false };
}
