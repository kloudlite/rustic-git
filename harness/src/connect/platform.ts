// Matched by name, as main and the controller already do; no runtime import, so node --test loads this alone.
const expired = () => Object.assign(new Error("your login has expired or was revoked"), { name: "Expired" });

/**
 * The `/v1` reads the desktop shows, run in MAIN with the stored token: the renderer asks for one
 * of these by name over IPC and gets back only the plain, validated objects below — never the
 * token, never a raw response, never a path of its own choosing.
 *
 * Shapes are the handlers' own (`crates/workspaces/src/model.rs` `Workspace`/`Environment`,
 * snake_case; `crd::ServiceStatus`/`Intercept` camelCase; `api/volumes.rs` `snapshot_rows`), cut
 * down to the fields the app renders. Anything that does not match is refused whole rather than
 * half-rendered.
 */
export type ApiWorkspace = { id: string; name: string; state: string; repo?: string; branch?: string; packages: string[] };
export type ApiService = { name: string; image: string; ports: number[] };
export type ApiServiceStatus = { name: string; ready: boolean; message?: string; interceptedBy?: string };
export type ApiIntercept = { service: string; workspace: string; ports: { service: number; workspace: number }[] };
export type ApiEnvironment = {
  id: string;
  owner: string;
  name: string;
  region: string;
  state: string;
  volume?: string;
  services: ApiService[];
  serviceStatus: ApiServiceStatus[];
  intercepts: ApiIntercept[];
};
/**
 * `GET /v1/repos` (`crates/api/src/repos.rs`, `RepoOut`). The listing comes from the object-store
 * markers, so it carries what a marker knows and NOTHING more: there is no default branch and no
 * last-updated here — reading a repo's HEAD would mean opening its database on its owning node.
 */
export type ApiRepo = { id: string; owner: string; name: string; public: boolean; description?: string; createdAt?: number };
export type ApiSnapshot = { id: string; message?: string; createdAt?: string; phase: string; services?: number };

type Obj = Record<string, unknown>;
const isObj = (v: unknown): v is Obj => typeof v === "object" && v !== null && !Array.isArray(v);
const bad = (what: string) => new Error(`Kloudlite answered an unreadable ${what}`);
function str(o: Obj, k: string, what: string): string {
  if (typeof o[k] !== "string") throw bad(what);
  return o[k] as string;
}
function optStr(o: Obj, k: string, what: string): string | undefined {
  if (o[k] == null) return undefined;
  return str(o, k, what);
}
function list<T>(v: unknown, what: string, f: (o: Obj) => T): T[] {
  if (v == null) return [];
  if (!Array.isArray(v)) throw bad(what);
  return v.map((x) => {
    if (!isObj(x)) throw bad(what);
    return f(x);
  });
}
function strs(v: unknown, what: string): string[] {
  if (v == null) return [];
  if (!Array.isArray(v) || v.some((x) => typeof x !== "string")) throw bad(what);
  return v as string[];
}
const port = (v: unknown, what: string) => {
  if (!Number.isInteger(v) || (v as number) < 0 || (v as number) > 65535) throw bad(what);
  return v as number;
};

/** A path or query segment: the server's own `valid_segment` charset, then encoded anyway. */
export function segment(s: unknown): string {
  if (typeof s !== "string" || !/^[A-Za-z0-9._-]{1,128}$/.test(s) || s === "." || s === "..") throw new Error("not a valid name");
  return encodeURIComponent(s);
}

async function getJson(api: string, token: string, path: string, missing?: unknown): Promise<unknown> {
  const r = await fetch(api + path, { headers: { authorization: `Bearer ${token}` }, redirect: "error", signal: AbortSignal.timeout(10_000) });
  if (r.status === 401) throw (await r.body?.cancel(), expired());
  if (r.status === 404 && missing !== undefined) return (await r.body?.cancel(), missing);
  if (!r.ok) throw (await r.body?.cancel(), new Error(`Kloudlite answered ${r.status}`));
  try {
    return await r.json();
  } catch {
    throw bad("answer");
  }
}

const toWorkspace = (o: Obj): ApiWorkspace => ({
  id: str(o, "id", "workspace"),
  name: str(o, "name", "workspace"),
  state: str(o, "state", "workspace"),
  repo: optStr(o, "repo", "workspace"),
  branch: optStr(o, "branch", "workspace"),
  packages: strs(o.packages, "workspace"),
});

function toEnvironment(o: Obj): ApiEnvironment {
  const w = "environment";
  return {
    id: str(o, "id", w),
    owner: str(o, "owner", w),
    name: str(o, "name", w),
    region: str(o, "region", w),
    state: str(o, "state", w),
    volume: optStr(o, "volume", w),
    services: list(o.services, w, (s) => {
      if (s.ports != null && !Array.isArray(s.ports)) throw bad(w);
      return { name: str(s, "name", w), image: str(s, "image", w), ports: ((s.ports as unknown[]) ?? []).map((p) => port(p, w)) };
    }),
    serviceStatus: list(o.service_status, w, (s) => {
      if (typeof s.ready !== "boolean") throw bad(w);
      return { name: str(s, "name", w), ready: s.ready, message: optStr(s, "message", w), interceptedBy: optStr(s, "interceptedBy", w) };
    }),
    intercepts: list(o.intercepts, w, (i) => ({
      service: str(i, "service", w),
      workspace: str(i, "workspace", w),
      ports: list(i.ports, w, (p) => ({ service: port(p.service, w), workspace: port(p.workspace, w) })),
    })),
  };
}

function toSnapshot(o: Obj): ApiSnapshot {
  const w = "snapshot";
  const st = o.state;
  return {
    id: str(o, "id", w),
    message: optStr(o, "message", w),
    createdAt: optStr(o, "createdAt", w),
    phase: str(o, "phase", w),
    services: isObj(st) && st.kind === "environment" && Array.isArray(st.services) ? st.services.length : undefined,
  };
}

const toRepo = (o: Obj): ApiRepo => {
  const w = "repo";
  const at = o.created_at;
  return {
    // `_id` is `{owner}/{name}`; the api renames it in serde, so it is read under that name.
    id: typeof o._id === "string" && o._id ? o._id : `${str(o, "owner", w)}/${str(o, "name", w)}`,
    owner: str(o, "owner", w),
    name: str(o, "name", w),
    public: o.public === true,
    description: optStr(o, "description", w),
    createdAt: typeof at === "number" ? at : undefined,
  };
};

/**
 * `GET /v1/repos?owner=` — the repos under one owner, a team slug or a person's handle. The api
 * requires `owner` (400 without it) and answers 404 for an owner the caller may not act under.
 */
export async function listRepos(api: string, token: string, owner: string): Promise<ApiRepo[]> {
  const v = await getJson(api, token, `/v1/repos?owner=${segment(owner)}`);
  if (!Array.isArray(v)) throw bad("repo list");
  return list(v, "repo", toRepo);
}

/** `GET /v1/workspaces?team=` — the caller's own workspaces in that team. */
export async function listWorkspaces(api: string, token: string, team: string): Promise<ApiWorkspace[]> {
  const v = await getJson(api, token, `/v1/workspaces?team=${segment(team)}`);
  if (!Array.isArray(v)) throw bad("workspace list");
  return list(v, "workspace", toWorkspace);
}

/** `GET /v1/environments?owner=` — that team's environments (the builder is never listed). */
export async function listEnvironments(api: string, token: string, owner: string): Promise<ApiEnvironment[]> {
  const v = await getJson(api, token, `/v1/environments?owner=${segment(owner)}`);
  if (!Array.isArray(v)) throw bad("environment list");
  return list(v, "environment", toEnvironment);
}

/** `GET /v1/environments/{id}`. */
export async function getEnvironment(api: string, token: string, id: string): Promise<ApiEnvironment> {
  const v = await getJson(api, token, `/v1/environments/${segment(id)}`);
  if (!isObj(v)) throw bad("environment");
  return toEnvironment(v);
}

/** `GET /v1/volumes/{name}/history`, newest first; a volume with no snapshots answers 404, which is an empty list. */
export async function volumeHistory(api: string, token: string, volume: string): Promise<ApiSnapshot[]> {
  const v = await getJson(api, token, `/v1/volumes/${segment(volume)}/history`, []);
  if (!Array.isArray(v)) throw bad("snapshot list");
  return list(v, "snapshot", toSnapshot);
}

async function sendJson(api: string, token: string, method: "PUT" | "DELETE", path: string, body?: unknown): Promise<unknown> {
  const r = await fetch(api + path, {
    method,
    headers: { authorization: `Bearer ${token}`, ...(body === undefined ? {} : { "content-type": "application/json" }) },
    body: body === undefined ? undefined : JSON.stringify(body),
    redirect: "error",
    signal: AbortSignal.timeout(10_000),
  });
  if (r.status === 401) throw (await r.body?.cancel(), expired());
  if (!r.ok) throw (await r.body?.cancel(), new Error(`Kloudlite answered ${r.status}`));
  if (r.status === 204) return (await r.body?.cancel(), undefined);
  try {
    return await r.json();
  } catch {
    throw bad("answer");
  }
}

/**
 * `GET /v1/me/environments` — one row per space the caller has chosen one for. The desktop shows
 * the connected team's row only, so a space with none is `undefined` rather than some other team's.
 */
export async function myEnvironment(api: string, token: string, team: string): Promise<string | undefined> {
  const v = await getJson(api, token, "/v1/me/environments");
  if (!Array.isArray(v)) throw bad("space list");
  const rows = list(v, "space", (o) => ({ team: str(o, "team", "space"), environment: str(o, "environment", "space") }));
  return rows.find((r) => r.team.toLowerCase() === team.toLowerCase())?.environment;
}

/** `PUT /v1/me/environments/{team}` — the space follows `id` from now on. */
export async function setMyEnvironment(api: string, token: string, team: string, id: string): Promise<void> {
  segment(id); // the id travels in the body, but it is ours to refuse before the call
  await sendJson(api, token, "PUT", `/v1/me/environments/${segment(team)}`, { environment: id });
}

/** `DELETE /v1/me/environments/{team}` — the space follows nothing; idempotent server-side. */
export async function clearMyEnvironment(api: string, token: string, team: string): Promise<void> {
  await sendJson(api, token, "DELETE", `/v1/me/environments/${segment(team)}`);
}
