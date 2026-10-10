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
  if (r.unavailable) throw new Error(UNAVAILABLE);
  if (r.status < 200 || r.status >= 300) throw new Error(`${r.status}: ${r.text}`);
  if (r.status === 204 || !r.text) return "ok";
  return r.data === undefined ? r.text : JSON.stringify(r.data, null, 2);
}

/** One call, rendered for the model. A non-2xx answer throws, so a direct call shows as failed and a codemode script stops. */
export async function api(method: string, path: string, body?: unknown): Promise<string> {
  return show(await raw(method, path, body));
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
// Models reach for the display name; every /v1 path takes the id, so say so on every param.
const WS = { type: "string", description: "The workspace id (`ws-…`, the `id` from workspace_list). Do not use the name." };
const ENV = { type: "string", description: "The environment id (`env-…`, the `id` from env_list). Do not use the name." };
const N = { type: "number" };
const SL = { type: "array", items: S };
const OBJ = { type: "object" };
const SERVICE = {
  type: "object",
  properties: {
    name: S,
    image: { type: "string", description: "repo:tag. The platform pins it to the tag's current digest (repo:tag@sha256:…), so redeploying a rebuilt tag runs the new bytes." },
    command: SL,
    env: OBJ,
    mounts: { type: "array", items: { type: "object", properties: { folder: S, path: S }, required: ["folder", "path"] } },
    ports: { type: "array", items: N },
    resources: OBJ,
  },
  required: ["name", "image"],
  description: "The `command`, `env` and `mounts` fields default to empty.",
};

/** The API requires command, env and mounts on every service; the schema asks only for name and image. */
const withServiceDefaults = (s: any) => ({ command: [], env: {}, mounts: [], ...s });

/** Fold service_status into each services[] entry so a poll reads ready where the spec is. */
function withReady(out: string): string {
  try {
    const doc = JSON.parse(out);
    if (!Array.isArray(doc?.services)) return out;
    doc.services = doc.services.map((svc: any) => {
      const st = doc.service_status?.find((x: any) => x.name === svc.name);
      return { ...svc, ready: st?.ready ?? false, ...(st?.message ? { message: st.message } : {}) };
    });
    return JSON.stringify(doc, null, 2);
  } catch {
    return out;
  }
}

function def(name: string, description: string, properties: Props, required: string[], run: (a: any) => Promise<string>): ToolDef {
  // the tools' own refusals ("error: service exists") fail like an API error does, never pass as a result
  const checked = async (a: any) => {
    const r = await run(a ?? {});
    if (r.startsWith("error: ")) throw new Error(r.slice(7));
    return r;
  };
  return { name, description, inputSchema: { type: "object", properties, required }, run: checked };
}

const attrOf = (p: string) => p.split("@")[0]!;
const ASYNC = " It returns 202 immediately. The work continues after the return. Poll the matching `*_get` tool.";

/** GET a document and throw on a failure like `api`. */
async function load(path: string): Promise<{ doc: any } | { err: string }> {
  const r = await raw("GET", path);
  if (r.unavailable) throw new Error(UNAVAILABLE);
  if (r.status < 200 || r.status >= 300) throw new Error(`${r.status}: ${r.text}`);
  return { doc: r.data };
}

export function platformTools(kind: "main" | "workspace", wsId?: string): ToolDef[] {
  const ws = kind === "workspace";
  const W = ws ? `/v1/workspaces/${seg(wsId!)}` : "";
  const wsPath = (a: any) => (ws ? W : `/v1/workspaces/${seg(a.workspace)}`);
  /** `workspace` param only on main. */
  const wp = (extra: Props = {}): Props => (ws ? extra : { workspace: WS, ...extra });
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
    // `crd::space_slug`: a personal workspace's space is the owner's own handle, never "no team"
    const team = String(g.doc?.team || g.doc?.owner || "").toLowerCase();
    return team ? { team } : { err: "error: workspace has no owner" };
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
    def(name, description, { env: ENV, ...props }, ws ? req : ["env", ...req], async (a) => {
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
    def("packages_list", "List the packages that the workspace declares. Do this before you change them.", wp(), wr(), async (a) => {
      const g = await load(wsPath(a));
      return "err" in g ? g.err : JSON.stringify(g.doc?.packages ?? [], null, 2);
    }),
    packages("packages_add", "Declare packages (`attr` or `attr@version`) on the workspace. An entry with the same `attr` replaces the old entry.", { packages: SL }, ["packages"], (cur, a) => {
      const add: string[] = a.packages ?? [];
      const keep = cur.filter((p) => !add.some((n) => attrOf(n) === attrOf(p)));
      return [...keep, ...add];
    }),
    packages("packages_remove", "Remove declared packages from the workspace. Give the `attr` of each package.", { packages: SL }, ["packages"], (cur, a) => {
      const gone = new Set<string>((a.packages ?? []).map(attrOf));
      const next = cur.filter((p) => !gone.has(attrOf(p)));
      return next.length === cur.length ? `error: not declared: ${(a.packages ?? []).join(", ")}` : next;
    }),
    def("packages_update", "Resolve the package pins of the workspace again, to the newest matching versions.", wp(), wr(), async (a) => api("POST", `${wsPath(a)}/packages/update`)),
    def("workspace_push", "Make a snapshot of the workspace in its history." + ASYNC, wp({ message: S }), wr(), async (a) => api("POST", `${wsPath(a)}/push`, { message: a.message })),
    envTool(
      "service_logs",
      "Read the recent logs of one service of an environment that you own. The tool reads from inside the cluster, not from /v1. It is read-only. `tail` is the number of lines. The default is 200 and the maximum is 2000. `since` is in seconds. The maximum is 86400. `previous` reads the last container that crashed. If an intercept has taken the service, the answer has a note. The note names the workspace that runs the service.",
      { service: S, tail: N, since: N, previous: { type: "boolean" } },
      ["service"],
      async (env, a) => {
        const base = (process.env.KL_LOGS_URL ?? "http://builder-gate.kloudlite-system.svc:1235").replace(/\/$/, "");
        const q = qs({ tail: a.tail === undefined ? undefined : String(a.tail), since: a.since === undefined ? undefined : String(a.since), previous: a.previous ? "true" : undefined });
        // no auth header: the gate identifies this pod by its source IP
        const res = await fetch(`${base}/logs/${env}/${seg(a.service)}${q}`, { signal: AbortSignal.timeout(15_000) });
        const text = await res.text();
        if (res.status < 200 || res.status >= 300) throw new Error(`${res.status}: ${text}`);
        return text;
      },
    ),
    def("space_env_current", "Show the environment that each space (team) of the person follows.", {}, [], async () => api("GET", "/v1/me/environments")),
    def("space_env_switch", "Make every workspace of a team follow a different environment." + (ws ? " The team defaults to the team of this workspace." : ""), { ...teamProp, env: ENV }, ws ? ["env"] : ["team", "env"], async (a) => {
      const t = await teamOf(a);
      return "err" in t ? t.err : api("PUT", `/v1/me/environments/${seg(t.team)}`, { environment: a.env });
    }),
    def("space_env_clear", "Detach the space of a team from its environment." + (ws ? " The team defaults to the team of this workspace." : ""), teamProp, ws ? [] : ["team"], async (a) => {
      const t = await teamOf(a);
      return "err" in t ? t.err : api("DELETE", `/v1/me/environments/${seg(t.team)}`);
    }),
    envTool("env_get", "Read the services, state and intercepts of an environment. Each `services[]` entry has `ready` (and `message`) from service_status. Poll `ready`, not the spec. The `state` is `creating`, `running`, `stopped`, `error` or `deleted`. `running` means up. Stop the poll when the state is `error` or `deleted`." + (ws ? " It defaults to the environment that the space of this workspace follows." : ""), {}, [], async (env) => withReady(await api("GET", `/v1/environments/${env}`))),
    services("service_add", "Add one service to an environment. The call fails if a service with this name exists." + ASYNC, { service: SERVICE }, ["service"], (cur, a) =>
      cur.some((s) => s.name === a.service?.name) ? `error: service exists: ${a.service?.name}` : [...cur, withServiceDefaults(a.service)]),
    services("service_update", "Replace one existing service in an environment. The tool matches the service by name." + ASYNC, { service: SERVICE }, ["service"], (cur, a) =>
      cur.some((s) => s.name === a.service?.name) ? cur.map((s) => (s.name === a.service.name ? withServiceDefaults(a.service) : s)) : `error: no such service: ${a.service?.name}`),
    services("service_remove", "Remove a service from an environment. Give the name of the service." + ASYNC, { name: S }, ["name"], (cur, a) =>
      cur.some((s) => s.name === a.name) ? cur.filter((s) => s.name !== a.name) : `error: no such service: ${a.name}`),
    // a workspace session intercepts for itself only: no `workspace` param to aim it at another workspace
    envTool("intercept", (ws ? "Send the traffic of an environment service to this workspace." : "Send the traffic of an environment service to a workspace.") + ASYNC, { service: S, ...(ws ? {} : { workspace: WS }), ports: { type: "array", items: { type: "object", properties: { service: N, workspace: N } } } }, ["service", ...(ws ? [] : ["workspace"])], (env, a) =>
      api("POST", `/v1/environments/${env}/intercepts`, strip({ service: a.service, workspace: ws ? wsId : a.workspace, ports: a.ports }))),
    envTool("release", "Release an intercepted service back to its own pod.", { service: S }, ["service"], (env, a) => api("DELETE", `/v1/environments/${env}/intercepts/${seg(a.service)}`)),
  ];
  // main's own `workspace_stop` (below) takes a `workspace` param; this one stops the session's own
  // workspace, so it sits outside `shared` and `of()` never picks it up.
  if (ws) return [...shared, def("workspace_stop", "Stop this workspace after its task is done and main or the person said to stop. The tool makes a snapshot first. The next start resumes the workspace." + ASYNC, {}, [], async () => api("POST", `${W}/stop`))];

  const of = (n: string) => shared.find((t) => t.name === n)!;

  const lifecycle = (name: string, description: string, method: string, suffix: string, key = "workspace", base = "/v1/workspaces") =>
    def(name, description, { [key]: key === "env" ? ENV : WS }, [key], async (a) => api(method, `${base}/${seg(a[key])}${suffix}`));

  return [
    def("workspace_list", "List the workspaces of the person. You can give one team.", { team: S }, [], async (a) => api("GET", `/v1/workspaces${qs({ team: a.team })}`)),
    def("workspace_get", "Read the state of one workspace. Poll this tool after any lifecycle call. The `state` is `creating`, `ready`, `stopped`, `error` or `deleted`. `ready` means up. A workspace is not `running` at any time. Stop the poll when the state is `error` or `deleted`.", { workspace: WS }, ["workspace"], async (a) => api("GET", `/v1/workspaces/${seg(a.workspace)}`)),
    def("workspace_create", "Create a workspace in a region." + ASYNC, { name: S, region: S, quota_gb: N, image: S, repo: S, branch: S, packages: SL, team: S }, ["name", "region", "quota_gb"], async (a) =>
      api("POST", "/v1/workspaces", strip({ team: a.team, name: a.name, region: a.region, quota_gb: a.quota_gb, image: a.image, repo: a.repo, branch: a.branch, packages: a.packages }))),
    def("workspace_clone", "Clone the current state of a workspace into a new workspace." + ASYNC, { workspace: WS, name: S, task: { type: "string", description: "What the clone is for. Use at most 200 characters." } }, ["workspace", "name"], async (a) => api("POST", `/v1/workspaces/${seg(a.workspace)}/clone`, strip({ name: a.name, task: a.task }))),
    def("workspace_restore", "Create a workspace from a snapshot." + ASYNC, { name: S, snapshot_id: S, image: S, packages: SL, quota_gb: N }, ["name", "snapshot_id"], async (a) =>
      api("POST", "/v1/workspaces/restore", strip({ name: a.name, snapshot_id: a.snapshot_id, image: a.image, packages: a.packages, quota_gb: a.quota_gb }))),
    lifecycle("workspace_start", "Start a stopped workspace." + ASYNC, "POST", "/start"),
    lifecycle("workspace_stop", "Stop a workspace that is up. The tool makes a snapshot first." + ASYNC, "POST", "/stop"),
    lifecycle("workspace_delete", "Delete a workspace permanently." + ASYNC, "DELETE", ""),
    of("workspace_push"),
    def("worktree_add", "Make a new worktree in a workspace." + ASYNC, { workspace: WS, name: S }, ["workspace", "name"], async (a) => api("POST", `/v1/workspaces/${seg(a.workspace)}/trees`, { name: a.name })),
    def("worktree_drop", "Remove a worktree from a workspace." + ASYNC, { workspace: WS, name: S }, ["workspace", "name"], async (a) => api("DELETE", `/v1/workspaces/${seg(a.workspace)}/trees/${seg(a.name)}`)),
    of("packages_list"),
    def("env_list", "List the environments. You can give one team.", { team: S }, [], async (a) => api("GET", `/v1/environments${qs({ owner: a.team })}`)),
    def("env_get", "Read the services, state and intercepts of one environment. Poll this tool after any env call. Each `services[]` entry has `ready` (and `message`) from service_status. Poll `ready`, not the spec. The `state` is `creating`, `running`, `stopped`, `error` or `deleted`. `running` means up. Stop the poll when the state is `error` or `deleted`.", { env: ENV }, ["env"], async (a) => withReady(await api("GET", `/v1/environments/${seg(a.env)}`))),
    of("service_logs"),
    def("env_delete", "Delete an environment permanently.", { env: ENV }, ["env"], async (a) => api("DELETE", `/v1/environments/${seg(a.env)}`)),
    def("env_start", "Start a stopped environment." + ASYNC, { env: ENV }, ["env"], async (a) => api("POST", `/v1/environments/${seg(a.env)}/start`)),
    def("env_stop", "Stop an environment that is up." + ASYNC, { env: ENV }, ["env"], async (a) => api("POST", `/v1/environments/${seg(a.env)}/stop`)),
    def("env_create", "Create an environment of services in a region." + ASYNC, { name: S, region: S, services: { type: "array", items: SERVICE }, team: S, quota_gb: N }, ["name", "region"], async (a) =>
      api("POST", "/v1/environments", strip({ name: a.name, region: a.region, services: a.services?.map(withServiceDefaults), owner: a.team, quota_gb: a.quota_gb }))),
    def("env_clone", "Copy an environment that is up into a new environment." + ASYNC, { env: ENV, name: S }, ["env", "name"], async (a) => api("POST", `/v1/environments/${seg(a.env)}/clone`, { name: a.name })),
    def("env_push", "Make a snapshot of an environment in its history." + ASYNC, { env: ENV, message: S }, ["env"], async (a) => api("POST", `/v1/environments/${seg(a.env)}/push`, { message: a.message })),
    def("env_restore", "Create an environment from a snapshot." + ASYNC, { name: S, snapshot_id: S, team: S, services: { type: "array", items: SERVICE }, region: S, quota_gb: N }, ["name", "snapshot_id"], async (a) =>
      api("POST", "/v1/environments/restore", strip({ name: a.name, snapshot_id: a.snapshot_id, owner: a.team, services: a.services?.map(withServiceDefaults), region: a.region, quota_gb: a.quota_gb }))),
    def("env_restore_in_place", "Put an environment back to one of its own snapshots." + ASYNC, { env: ENV, snapshot_id: S }, ["env", "snapshot_id"], async (a) =>
      api("POST", `/v1/environments/${seg(a.env)}/restore-in-place`, { snapshot_id: a.snapshot_id })),
    ...["service_add", "service_update", "service_remove"].map(of),
    ...["space_env_current", "space_env_switch", "space_env_clear"].map(of),
    def("volume_list", "List the volumes. You can filter by kind (`workspace` or `environment`) and by owner.", { kind: S, owner: S }, [], async (a) => api("GET", `/v1/volumes${qs({ kind: a.kind, owner: a.owner })}`)),
    def("volume_history", "List the snapshots of a volume. These are the ids for restore.", { volume: S }, ["volume"], async (a) => api("GET", `/v1/volumes/${seg(a.volume)}/history`)),
    def("volume_refs", "Read the branch tips of a volume.", { volume: S }, ["volume"], async (a) => api("GET", `/v1/volumes/${seg(a.volume)}/refs`)),
    def("volume_delete", "Delete a detached volume permanently.", { volume: S }, ["volume"], async (a) => api("DELETE", `/v1/volumes/${seg(a.volume)}`)),
    def("snapshot_delete", "Delete one snapshot of a volume.", { volume: S, snapshot: S }, ["volume", "snapshot"], async (a) => api("DELETE", `/v1/volumes/${seg(a.volume)}/snapshots/${seg(a.snapshot)}`)),
    def("builder_status", "Check the state of the image builder for a team.", { team: S }, [], async (a) => api("GET", `/v1/builders/me${qs({ team: a.team })}`)),
    def("quota", "Show the limits and the current use of an owner (workspaces, environments, snapshots, diskGb, cpu, memoryGb). A call that creates more than a limit answers 409. To ask for more, use request_create with kind `quota`.", { owner: S }, [], async (a) => api("GET", `/v1/quota${qs({ owner: a.owner })}`)),
    def("requests_list", "List the requests of the person and of their teams (quota, access, region, other). Each request has a state (`pending`, `approved` or `denied`) and the decision note.", { owner: S }, [], async (a) => api("GET", `/v1/requests${qs({ owner: a.owner })}`)),
    def("request_get", "Show one request and its decision.", { id: S }, ["id"], async (a) => api("GET", `/v1/requests/${seg(a.id)}`)),
    def(
      "request_create",
      "Ask a superadmin for something that the person cannot do now. Examples are more quota, access to a team, a region, or anything else. Each owner can have only one pending request of each kind. Otherwise the answer is 409. The tool returns at once. The decision comes later. Use request_get to check it.",
      {
        owner: { type: "string", description: "The team slug. Omit it for the person's own." },
        kind: { type: "string", enum: ["quota", "access", "region", "other"] },
        reason: S,
        quota: {
          type: "object",
          description: "The new ceilings that you want. Give only the dimensions that change.",
          properties: { workspaces: { type: "integer" }, environments: { type: "integer" }, snapshots: { type: "integer" }, diskGb: { type: "integer" }, cpu: { type: "integer" }, memoryGb: { type: "integer" } },
        },
        access: { type: "object", properties: { team: S, role: { type: "string", enum: ["member", "admin", "owner"] } }, required: ["team", "role"] },
        region: { type: "string", description: "The region name." },
        title: S,
        body: S,
      },
      ["kind", "reason"],
      async (a) => api("POST", "/v1/requests", strip({ owner: a.owner, kind: a.kind, reason: a.reason, quota: a.quota, access: a.access, region: a.region ? { region: a.region } : undefined, other: a.kind === "other" ? { title: a.title, body: a.body } : undefined })),
    ),
    def("regions", "List the regions where workspaces and environments can be created.", {}, [], async () => api("GET", "/v1/regions")),
  ];
}
