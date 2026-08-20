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

// `manifests` is an object-store manifest count, not a tag count: the images list is owner-scoped
// and cannot route to any one image's database, where tags and visibility actually live. See
// `browse_api::images` server-side.
export type ImageSummary = { name: string; manifests: number; updated_ms: number | null };
export type ImageTag = {
  tag: string;
  digest: string;
  /** The manifest document's own bytes — small, and not what a pull costs. */
  size: number;
  /** Config + all layers: what `docker pull` actually transfers. This is "the image size". */
  bytes: number;
  pushed_ms: number | null;
};

/** The team's pushed images — owner-scoped, not repo-scoped, since an image is not a git repo. */
export function images(token: string | undefined, owner: string) {
  return get<ImageSummary[]>(`/api/${seg(owner)}/images`, token);
}

export function imageTags(token: string | undefined, owner: string, image: string) {
  return get<ImageTag[]>(`/api/${seg(owner)}/${seg(image)}/imagetags`, token);
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

export type WalkedFile = { name: string; path: string; size: number | null };

/**
 * Every file under a commit, in ONE request.
 *
 * This used to be a walk from here — a request per directory, up to forty of them
 * in sequence, to answer a question that is a pure function of the commit id. The
 * api serves it directly now: same cacheability as a tree (immutable, keyed by
 * oid), one round trip instead of forty. The rule this follows: resolve refs
 * here, because refs move; ask the server for anything derived from an object id,
 * because the answer never changes and the walk belongs where the objects are.
 */
export async function files(
  token: string | undefined,
  owner: string,
  repo: string,
  oid: string,
  path = "",
): Promise<WalkedFile[]> {
  const q = path ? `?path=${encodeURIComponent(path)}` : "";
  const r = await get<Entry[]>(`/api/${seg(owner)}/${seg(repo)}/files/${seg(oid)}${q}`, token);
  // A repo whose shape cannot be read still lists and still opens; only the
  // derived views (languages, go-to-file) go quiet.
  if (!r.ok) return [];
  return r.value.map((e) => ({ name: e.name.split("/").pop() ?? e.name, path: e.name, size: e.size }));
}

/** What last touched each entry of a directory: one walk of history, server-side,
 *  keyed by commit id and therefore cacheable forever. Entries older than the
 *  server's budget come back absent rather than wrong. */
export async function lastChanges(
  token: string | undefined,
  owner: string,
  repo: string,
  oid: string,
  path = "",
): Promise<Map<string, Commit>> {
  const q = path ? `?path=${encodeURIComponent(path)}` : "";
  const r = await get<(Commit & { name: string })[]>(
    `/api/${seg(owner)}/${seg(repo)}/lastmod/${seg(oid)}${q}`,
    token,
  );
  if (!r.ok) return new Map();
  return new Map(r.value.map((c) => [c.name, c]));
}
