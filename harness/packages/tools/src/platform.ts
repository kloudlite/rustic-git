//! Platform API tools: the model's hands on /v1 (workspaces, environments, volumes, space).
//! The bearer is a short-lived tool token (rotates every 900 s) in a file, so it is read on
//! EVERY call, never cached. Non-2xx comes back as text for the model, never a throw.
//! Main sessions get the whole table; a workspace session gets the subset its own token's
//! route list allows (WORKSPACE_TOOL_ROUTES), with its workspace fixed and `env` defaulted.
import { readFileSync } from "node:fs";
import type { ToolDef } from "./index.ts";

const UNAVAILABLE = "platform tools unavailable: KL_API_URL / KL_TOOL_TOKEN_FILE not set";

type Raw = { unavailable: true } | { unavailable?: false; status: number; text: string; data: any };

async function raw(method: string, path: string, body?: unknown): Promise<Raw> {
  const base = process.env.KL_API_URL;
  const file = process.env.KL_TOOL_TOKEN_FILE;
  if (!base || !file) return { unavailable: true };
  const token = readFileSync(file, "utf8").trim();
  const res = await fetch(base.replace(/\/$/, "") + path, {
    method,
    headers: { authorization: `Bearer ${token}`, ...(body === undefined ? {} : { "content-type": "application/json" }) },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    signal: AbortSignal.timeout(30_000),
  });
  const text = await res.text();
  let data: any;
  try {
    data = text ? JSON.parse(text) : undefined;
  } catch {}
  return { status: res.status, text, data };
}

function show(r: Raw): string {
  if (r.unavailable) return UNAVAILABLE;
  if (r.status < 200 || r.status >= 300) return `error ${r.status}: ${r.text}`;
  if (r.status === 204 || !r.text) return "ok";
  return r.data === undefined ? r.text : JSON.stringify(r.data, null, 2);
}

/** One call, rendered for the model. */
export async function api(method: string, path: string, body?: unknown): Promise<string> {
  try {
    return show(await raw(method, path, body));
  } catch (e: any) {
    return `error: ${e?.message ?? e}`;
  }
}

/** One call, parsed. For the TUI's own reads (sidebar): the same text a failed `api` shows, as an Error. */
export async function apiJson<T>(method: string, path: string, body?: unknown): Promise<T> {
  let r: Raw;
  try {
    r = await raw(method, path, body);
  } catch (e: any) {
    throw new Error(`error: ${e?.message ?? e}`);
  }
  if (r.unavailable) throw new Error(UNAVAILABLE);
  if (r.status < 200 || r.status >= 300) throw new Error(`error ${r.status}: ${r.text}`);
  return r.data as T;
}

const seg = encodeURIComponent;
const qs = (p: Record<string, string | undefined>) => {
  const u = new URLSearchParams();
  for (const [k, v] of Object.entries(p)) if (v) u.set(k, v);
  const s = u.toString();
  return s ? `?${s}` : "";
};
const strip = (o: Record<string, unknown>) => Object.fromEntries(Object.entries(o).filter(([, v]) => v !== undefined));

type Props = Record<string, unknown>;
const S = { type: "string" };
const N = { type: "number" };
const SL = { type: "array", items: S };
const OBJ = { type: "object" };
const SERVICE = {
  type: "object",
  properties: {
    name: S,
    image: S,
    command: SL,
    env: OBJ,
    mounts: { type: "array", items: { type: "object", properties: { folder: S, path: S }, required: ["folder", "path"] } },
    ports: { type: "array", items: N },
    resources: OBJ,
  },
  required: ["name", "image"],
};

function def(name: string, description: string, properties: Props, required: string[], run: (a: any) => Promise<string>): ToolDef {
  return { name, description, inputSchema: { type: "object", properties, required }, run: async (a) => run(a ?? {}) };
}

const attrOf = (p: string) => p.split("@")[0]!;
const ASYNC = " Returns 202: the change converges asynchronously, so poll with the matching *_get.";

/** GET a document and surface a failure as the error text the tool returns. */
async function load(path: string): Promise<{ doc: any } | { err: string }> {
  try {
    const r = await raw("GET", path);
    if (r.unavailable) return { err: UNAVAILABLE };
    if (r.status < 200 || r.status >= 300) return { err: `error ${r.status}: ${r.text}` };
    return { doc: r.data };
  } catch (e: any) {
    return { err: `error: ${e?.message ?? e}` };
  }
}

export function platformTools(kind: "main" | "workspace", wsId?: string): ToolDef[] {
  const ws = kind === "workspace";
  const W = ws ? `/v1/workspaces/${seg(wsId!)}` : "";
  const wsPath = (a: any) => (ws ? W : `/v1/workspaces/${seg(a.workspace)}`);
  /** `workspace` param only on main. */
  const wp = (extra: Props = {}): Props => (ws ? extra : { workspace: S, ...extra });
  const wr = (req: string[] = []) => (ws ? req : ["workspace", ...req]);

  const packages = (name: string, description: string, props: Props, req: string[], edit: (cur: string[], a: any) => string[] | string) =>
    def(name, description, wp(props), wr(req), async (a) => {
      const g = await load(wsPath(a));
      if ("err" in g) return g.err;
      const next = edit(g.doc?.packages ?? [], a);
      if (typeof next === "string") return next;
      return api("PATCH", wsPath(a), { packages: next });
    });

  // space tools: in a workspace session `team` defaults to the workspace's own team
  const teamOf = async (a: any): Promise<{ team: string } | { err: string }> => {
    if (a.team) return { team: a.team };
    if (!ws) return { err: "error: team is required" };
    const g = await load(W);
    if ("err" in g) return g;
    return g.doc?.team ? { team: g.doc.team } : { err: "error: workspace has no team" };
  };
  const teamProp: Props = { team: S };

  /** `env` for a workspace session: the environment its space follows. */
  const envOf = async (a: any): Promise<{ env: string } | { err: string }> => {
    if (a.env) return { env: a.env };
    if (!ws) return { err: "error: env is required" };
    const t = await teamOf({});
    if ("err" in t) return t;
    const m = await load("/v1/me/environments");
    if ("err" in m) return m;
    const hit = (m.doc as any[]).find((e) => e.team === t.team);
    return hit?.environment ? { env: hit.environment } : { err: "error: this workspace's space follows no environment" };
  };
  const envTool = (name: string, description: string, props: Props, req: string[], run: (env: string, a: any) => Promise<string>) =>
    def(name, description, { env: S, ...props }, ws ? req : ["env", ...req], async (a) => {
      const e = await envOf(a);
      return "err" in e ? e.err : run(seg(e.env), a);
    });
  const services = (name: string, description: string, props: Props, req: string[], edit: (cur: any[], a: any) => any[] | string) =>
    envTool(name, description, props, req, async (env, a) => {
      const g = await load(`/v1/environments/${env}`);
      if ("err" in g) return g.err;
      const next = edit(g.doc?.services ?? [], a);
      return typeof next === "string" ? next : api("PATCH", `/v1/environments/${env}`, { services: next });
    });

  // tools both kinds have
  const shared: ToolDef[] = [
    def("packages_list", "List the packages declared for the workspace before changing them.", wp(), wr(), async (a) => {
      const g = await load(wsPath(a));
      return "err" in g ? g.err : JSON.stringify(g.doc?.packages ?? [], null, 2);
    }),
    packages("packages_add", "Declare packages (attr or attr@version) on the workspace; an entry with the same attr replaces the old one.", { packages: SL }, ["packages"], (cur, a) => {
      const add: string[] = a.packages ?? [];
      const keep = cur.filter((p) => !add.some((n) => attrOf(n) === attrOf(p)));
      return [...keep, ...add];
    }),
    packages("packages_remove", "Remove declared packages by attr from the workspace.", { packages: SL }, ["packages"], (cur, a) => {
      const gone = new Set<string>((a.packages ?? []).map(attrOf));
      const next = cur.filter((p) => !gone.has(attrOf(p)));
      return next.length === cur.length ? `error: not declared: ${(a.packages ?? []).join(", ")}` : next;
    }),
    def("packages_update", "Re-resolve the workspace's package pins to their newest matching versions.", wp(), wr(), async (a) => api("POST", `${wsPath(a)}/packages/update`)),
    def("workspace_push", "Snapshot the workspace to its history." + ASYNC, wp({ message: S }), wr(), async (a) => api("POST", `${wsPath(a)}/push`, { message: a.message })),
    def("space_env_current", "Show which environment each of the person's spaces (teams) follows.", {}, [], async () => api("GET", "/v1/me/environments")),
    def("space_env_switch", "Make every workspace of a team follow another environment." + (ws ? " Team defaults to this workspace's." : ""), { ...teamProp, env: S }, ws ? ["env"] : ["team", "env"], async (a) => {
      const t = await teamOf(a);
      return "err" in t ? t.err : api("PUT", `/v1/me/environments/${seg(t.team)}`, { environment: a.env });
    }),
    def("space_env_clear", "Detach a team's space from its environment." + (ws ? " Team defaults to this workspace's." : ""), teamProp, ws ? [] : ["team"], async (a) => {
      const t = await teamOf(a);
      return "err" in t ? t.err : api("DELETE", `/v1/me/environments/${seg(t.team)}`);
    }),
    envTool("env_get", "Read an environment's services, state and intercepts." + (ws ? " Defaults to the environment this workspace's space follows." : ""), {}, [], (env) => api("GET", `/v1/environments/${env}`)),
    services("service_add", "Add one service to an environment; fails if the name exists." + ASYNC, { service: SERVICE }, ["service"], (cur, a) =>
      cur.some((s) => s.name === a.service?.name) ? `error: service exists: ${a.service?.name}` : [...cur, a.service]),
    services("service_update", "Replace one existing service (matched by name) in an environment." + ASYNC, { service: SERVICE }, ["service"], (cur, a) =>
      cur.some((s) => s.name === a.service?.name) ? cur.map((s) => (s.name === a.service.name ? a.service : s)) : `error: no such service: ${a.service?.name}`),
    services("service_remove", "Remove a service by name from an environment." + ASYNC, { name: S }, ["name"], (cur, a) =>
      cur.some((s) => s.name === a.name) ? cur.filter((s) => s.name !== a.name) : `error: no such service: ${a.name}`),
    envTool("intercept", "Route an environment service's traffic to a workspace." + ASYNC, { service: S, workspace: S, ports: { type: "array", items: { type: "object", properties: { service: N, workspace: N } } } }, ["service", ...(ws ? [] : ["workspace"])], (env, a) =>
      api("POST", `/v1/environments/${env}/intercepts`, strip({ service: a.service, workspace: a.workspace ?? wsId, ports: a.ports }))),
    envTool("release", "Release an intercepted service back to its own pod.", { service: S }, ["service"], (env, a) => api("DELETE", `/v1/environments/${env}/intercepts/${seg(a.service)}`)),
  ];
  if (ws) return shared;

  const of = (n: string) => shared.find((t) => t.name === n)!;

  const lifecycle = (name: string, description: string, method: string, suffix: string, key = "workspace", base = "/v1/workspaces") =>
    def(name, description, { [key]: S }, [key], async (a) => api(method, `${base}/${seg(a[key])}${suffix}`));

  return [
    def("workspace_list", "List the person's workspaces, optionally for one team.", { team: S }, [], async (a) => api("GET", `/v1/workspaces${qs({ team: a.team })}`)),
    def("workspace_get", "Read one workspace's state; poll this after any lifecycle call.", { workspace: S }, ["workspace"], async (a) => api("GET", `/v1/workspaces/${seg(a.workspace)}`)),
    def("workspace_create", "Create a workspace in a region." + ASYNC, { name: S, region: S, quota_gb: N, image: S, repo: S, branch: S, packages: SL, team: S }, ["name", "region", "quota_gb"], async (a) =>
      api("POST", "/v1/workspaces", strip({ team: a.team, name: a.name, region: a.region, quota_gb: a.quota_gb, image: a.image, repo: a.repo, branch: a.branch, packages: a.packages }))),
    def("workspace_clone", "Clone a workspace's current state into a new workspace." + ASYNC, { workspace: S, name: S }, ["workspace", "name"], async (a) => api("POST", `/v1/workspaces/${seg(a.workspace)}/clone`, { name: a.name })),
    def("workspace_restore", "Create a workspace from a snapshot." + ASYNC, { name: S, snapshot_id: S, image: S, packages: SL, quota_gb: N }, ["name", "snapshot_id"], async (a) =>
      api("POST", "/v1/workspaces/restore", strip({ name: a.name, snapshot_id: a.snapshot_id, image: a.image, packages: a.packages, quota_gb: a.quota_gb }))),
    lifecycle("workspace_start", "Start a stopped workspace." + ASYNC, "POST", "/start"),
    lifecycle("workspace_stop", "Stop a running workspace (it snapshots first)." + ASYNC, "POST", "/stop"),
    lifecycle("workspace_delete", "Delete a workspace for good." + ASYNC, "DELETE", ""),
    of("workspace_push"),
    def("worktree_add", "Cut a new worktree in a workspace." + ASYNC, { workspace: S, name: S }, ["workspace", "name"], async (a) => api("POST", `/v1/workspaces/${seg(a.workspace)}/trees`, { name: a.name })),
    def("worktree_drop", "Drop a worktree from a workspace." + ASYNC, { workspace: S, name: S }, ["workspace", "name"], async (a) => api("DELETE", `/v1/workspaces/${seg(a.workspace)}/trees/${seg(a.name)}`)),
    ...["packages_list", "packages_add", "packages_remove", "packages_update"].map(of),
    def("env_list", "List environments, optionally for one team.", { team: S }, [], async (a) => api("GET", `/v1/environments${qs({ owner: a.team })}`)),
    def("env_get", "Read one environment's services, state and intercepts; poll this after any env call.", { env: S }, ["env"], async (a) => api("GET", `/v1/environments/${seg(a.env)}`)),
    def("env_delete", "Delete an environment for good.", { env: S }, ["env"], async (a) => api("DELETE", `/v1/environments/${seg(a.env)}`)),
    def("env_start", "Start a stopped environment." + ASYNC, { env: S }, ["env"], async (a) => api("POST", `/v1/environments/${seg(a.env)}/start`)),
    def("env_stop", "Stop a running environment." + ASYNC, { env: S }, ["env"], async (a) => api("POST", `/v1/environments/${seg(a.env)}/stop`)),
    def("env_create", "Create an environment of services in a region." + ASYNC, { name: S, region: S, services: { type: "array", items: SERVICE }, team: S, quota_gb: N }, ["name", "region"], async (a) =>
      api("POST", "/v1/environments", strip({ name: a.name, region: a.region, services: a.services, owner: a.team, quota_gb: a.quota_gb }))),
    def("env_clone", "Copy a live environment into a new one." + ASYNC, { env: S, name: S }, ["env", "name"], async (a) => api("POST", `/v1/environments/${seg(a.env)}/clone`, { name: a.name })),
    def("env_push", "Snapshot an environment to its history." + ASYNC, { env: S, message: S }, ["env"], async (a) => api("POST", `/v1/environments/${seg(a.env)}/push`, { message: a.message })),
    def("env_restore", "Create an environment from a snapshot." + ASYNC, { name: S, snapshot_id: S, team: S, services: { type: "array", items: SERVICE }, region: S, quota_gb: N }, ["name", "snapshot_id"], async (a) =>
      api("POST", "/v1/environments/restore", strip({ name: a.name, snapshot_id: a.snapshot_id, owner: a.team, services: a.services, region: a.region, quota_gb: a.quota_gb }))),
    def("env_restore_in_place", "Roll an environment back to one of its own snapshots." + ASYNC, { env: S, snapshot_id: S }, ["env", "snapshot_id"], async (a) =>
      api("POST", `/v1/environments/${seg(a.env)}/restore-in-place`, { snapshot_id: a.snapshot_id })),
    ...["service_add", "service_update", "service_remove", "intercept", "release"].map(of),
    ...["space_env_current", "space_env_switch", "space_env_clear"].map(of),
    def("volume_list", "List volumes, optionally by kind (workspace|environment) and owner.", { kind: S, owner: S }, [], async (a) => api("GET", `/v1/volumes${qs({ kind: a.kind, owner: a.owner })}`)),
    def("volume_history", "List a volume's snapshots (ids for restore).", { volume: S }, ["volume"], async (a) => api("GET", `/v1/volumes/${seg(a.volume)}/history`)),
    def("volume_refs", "Read a volume's branch tips.", { volume: S }, ["volume"], async (a) => api("GET", `/v1/volumes/${seg(a.volume)}/refs`)),
    def("volume_delete", "Delete a detached volume for good.", { volume: S }, ["volume"], async (a) => api("DELETE", `/v1/volumes/${seg(a.volume)}`)),
    def("snapshot_delete", "Delete one snapshot of a volume.", { volume: S, snapshot: S }, ["volume", "snapshot"], async (a) => api("DELETE", `/v1/volumes/${seg(a.volume)}/snapshots/${seg(a.snapshot)}`)),
    def("builder_status", "Check the image builder's state for a team.", { team: S }, [], async (a) => api("GET", `/v1/builders/me${qs({ team: a.team })}`)),
    def("quota", "Show an owner's quota and current usage.", { owner: S }, [], async (a) => api("GET", `/v1/quota${qs({ owner: a.owner })}`)),
    def("regions", "List the regions workspaces and environments can be created in.", {}, [], async () => api("GET", "/v1/regions")),
  ];
}
