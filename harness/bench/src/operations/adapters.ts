import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import type { ArgumentStates } from "./arguments.ts";
import type { JsonValue, ValidationIssue } from "./shape.ts";
import type { OperationErrorCode } from "./contracts.ts";

export type PlatformCall = (method: string, route: string, body?: unknown, signal?: AbortSignal) => Promise<{ status: number; data: unknown }>;
export type BenchCall = (method: string, route: string, body?: unknown) => Promise<{ ok: boolean; data: any }>;
export type AdapterInput = { args: Record<string, JsonValue>; states: ArgumentStates; signal?: AbortSignal };
export type AdapterError = { code: OperationErrorCode; message: string; retryable: boolean };
export type AdapterResult = { ok: true; value: JsonValue } | { ok: false; error: AdapterError };
export type Adapter = (input: AdapterInput) => Promise<AdapterResult>;

// One platform round trip (a single GET/POST/PATCH/DELETE) is expected to answer in seconds; the
// minutes-long waits (workspace start, restore) are a separate polling loop in kloudlite.ts's
// `settle` that reissues short calls like this one against its own cap. 30s is generous slack
// above ordinary API latency without being mistaken for that readiness wait.
export const PLATFORM_CALL_TIMEOUT_MS = 30_000;

const err = (code: OperationErrorCode, message: string, retryable = false): AdapterResult => ({ ok: false, error: { code, message, retryable } });
const ok = (value: unknown): AdapterResult => ({ ok: true, value: (value ?? null) as JsonValue });

export function classifyHttp(status: number, message: string, mutation: boolean): AdapterResult {
  if (status === 401 || status === 403) return err("permission_denied", message);
  if (status === 404) return err("no_match", message);
  if (status === 409) return err("revision_conflict", message);
  if (status === 400 || status === 422) return err("validation_failure", message);
  // On a mutation: 500 is the origin's own answer — it ran and refused, so it is a definitive
  // provider_failure. 502/503/504 mean an edge or gateway answered, not necessarily the origin,
  // for a write that may already have committed there — unknown, never failed. Every write is
  // reconcile-first, so a false "unknown" only costs one reconcile pass; a false "failed" can be
  // reported to the person as an error or retried against a change that already landed. On a
  // READ there is nothing to reconcile and nothing was committed, so every 5xx (including 503)
  // stays a retryable provider_failure, never unknown.
  if (mutation && (status === 502 || status === 503 || status === 504)) return err("unknown_outcome", message, true);
  return err(status >= 500 ? "provider_failure" : "execution_failure", message, status >= 500);
}

export async function platformResult(
  call: PlatformCall,
  method: string,
  route: string,
  body?: unknown,
  mutation = false,
  signal?: AbortSignal,
  timeoutMs = PLATFORM_CALL_TIMEOUT_MS,
): Promise<AdapterResult> {
  const bounded = AbortSignal.any([...(signal ? [signal] : []), AbortSignal.timeout(timeoutMs)]);
  try {
    const response = await call(method, route, body, bounded);
    return response.status >= 400 ? classifyHttp(response.status, typeof response.data === "string" ? response.data : JSON.stringify(response.data), mutation) : ok(response.data);
  } catch (error) {
    // ponytail: once PlatformCall exposes request write/response phases, distinguish failures
    // before dispatch. Today a thrown mutation transport (including an abort or timeout) is
    // conservatively unknown because the provider may have committed it; a definitive HTTP
    // response, including most 5xx, is classified above instead.
    return err(mutation ? "unknown_outcome" : "provider_failure", String((error as Error)?.message ?? error), !mutation);
  }
}

export function visibleWorkspaces(data: unknown, ownBench?: string): JsonValue[] {
  return (Array.isArray(data) ? data : []).filter((row) => {
    const value = row as { id?: string; bench?: unknown; kind?: string };
    return !(value.bench || value.kind === "bench" || value.id === ownBench || /^bench-[0-9a-f]{8,}$/.test(String(value.id ?? "")));
  }) as JsonValue[];
}

export function resolveUnique(rows: JsonValue[], idOrName: string, kind = "workspace"): { ok: true; id: string } | { ok: false; code: OperationErrorCode; message: string } {
  const records = rows as Array<{ id?: string; name?: string }>;
  if (records.some((row) => row.id === idOrName)) return { ok: true, id: idOrName };
  const hits = records.filter((row) => row.name === idOrName);
  if (hits.length === 1 && hits[0].id) return { ok: true, id: hits[0].id };
  if (hits.length > 1) return { ok: false, code: "ambiguous_match", message: `${hits.length} ${kind}s are called ${idOrName}; name it by id (${hits.map((row) => row.id).join(", ")})` };
  return { ok: false, code: "no_match", message: `no ${kind} ${idOrName}` };
}

export function toolResult(result: AdapterResult): { content: { type: "text"; text: string }[]; isError?: boolean } {
  return result.ok
    ? { content: [{ type: "text", text: typeof result.value === "string" ? result.value : JSON.stringify(result.value, null, 2) }] }
    : { content: [{ type: "text", text: result.error.message }], isError: true };
}

export function createSharedAdapters(input: { platform: PlatformCall; bench: BenchCall; ownBench?: string; team?: string }): Record<string, Adapter> {
  return { ...createPlatformAdapters(input.platform, input.ownBench, input.team), "workspace.progress": progressAdapter(input.bench), "skill.read": skillAdapter() };
}

// `team` is the bench's own scope (KL_TEAM): a team bench must list its team's workspaces, not
// just its own personal ones, or every id lookup below silently sees an empty listing.
export function createPlatformAdapters(call: PlatformCall, ownBench = process.env.KL_WORKSPACE_ID, team = process.env.KL_TEAM): Record<string, Adapter> {
  const workspaceList: Adapter = async ({ args, signal }) => {
    const scopedTeam = args.team ?? team;
    const result = await platformResult(call, "GET", `/v1/workspaces${scopedTeam ? `?team=${encodeURIComponent(String(scopedTeam))}` : ""}`, undefined, false, signal);
    return result.ok ? ok(visibleWorkspaces(result.value, ownBench)) : result;
  };
  // The listing exists to disambiguate a NAME; an id that is not in it is not necessarily wrong —
  // it may be a team workspace the listing didn't need to resolve. Pass it through as given so
  // /v1's own 404 (or success) is the answer, rather than a lookup failure of ours.
  const resolveId = async (idOrName: string, signal?: AbortSignal): Promise<{ ok: true; id: string } | { ok: false; code: OperationErrorCode; message: string }> => {
    const listed = await workspaceList({ args: {}, states: {}, signal });
    if (!listed.ok) return { ok: false, code: listed.error.code, message: listed.error.message };
    const resolved = resolveUnique(listed.value as JsonValue[], idOrName);
    return resolved.ok || resolved.code !== "no_match" ? resolved : { ok: true, id: idOrName };
  };
  const workspaceInspect: Adapter = async ({ args, signal }) => {
    const requested = String(args.id);
    if (requested === ownBench || /^bench-[0-9a-f]{8,}$/.test(requested)) return err("scope_denied", "that is you, not a workspace; name a workspace");
    const resolved = await resolveId(requested, signal);
    if (!resolved.ok) return err(resolved.code, resolved.message);
    return platformResult(call, "GET", `/v1/workspaces/${encodeURIComponent(resolved.id)}`, undefined, false, signal);
  };
  const services = async (id: string, signal?: AbortSignal): Promise<AdapterResult> => {
    const environment = await platformResult(call, "GET", `/v1/environments/${encodeURIComponent(id)}`, undefined, false, signal);
    return environment.ok ? ok(((environment.value as any)?.services ?? []) as JsonValue) : environment;
  };
  const packages = async (id: string, signal?: AbortSignal): Promise<AdapterResult> => {
    const workspace = await platformResult(call, "GET", `/v1/workspaces/${encodeURIComponent(id)}`, undefined, false, signal);
    return workspace.ok ? ok(((workspace.value as any)?.packages ?? []) as JsonValue) : workspace;
  };
  return {
    "workspace.list": workspaceList,
    "workspace.inspect": workspaceInspect,
    "workspace.create": ({ args, signal }) => platformResult(call, "POST", args.from_snapshot ? "/v1/workspaces/restore" : "/v1/workspaces", args.from_snapshot ? { name: args.name, snapshot_id: args.from_snapshot, packages: args.packages } : args, true, signal),
    "environment.create": ({ args, signal }) => platformResult(call, "POST", args.from_snapshot ? "/v1/environments/restore" : "/v1/environments", args.from_snapshot ? { name: args.name, snapshot_id: args.from_snapshot, services: args.services } : args, true, signal),
    "environment.restore": ({ args, signal }) => platformResult(call, "POST", `/v1/environments/${encodeURIComponent(String(args.id))}/restore-in-place`, { snapshot_id: args.snapshot }, true, signal),
    "environment.intercept": ({ args, states, signal }) => states.workspace?.kind === "explicitly_clear"
      ? platformResult(call, "DELETE", `/v1/environments/${encodeURIComponent(String(args.id))}/intercepts/${encodeURIComponent(String(args.service))}`, undefined, true, signal)
      : platformResult(call, "POST", `/v1/environments/${encodeURIComponent(String(args.id))}/intercepts`, { service: args.service, workspace: args.workspace, ...(args.ports === undefined ? {} : { ports: args.ports }) }, true, signal),
    // ponytail: this is GET-then-PATCH of the whole list, not an add/remove verb — two edits racing
    // (another scheduler lane, or the web UI) can lose one's write under the other's PATCH. Ceiling:
    // a lost update is silent until someone notices a service/package missing. Upgrade path: a
    // server-side add/remove verb, or an ETag/If-Match precondition on the PATCH.
    "environment.service.put": async ({ args, signal }) => {
      const current = await services(String(args.id), signal);
      if (!current.ok) return current;
      const have = current.value as JsonValue[];
      const one = args.service as Record<string, JsonValue>;
      const next = [...have.filter((row: any) => row.name !== one.name), one];
      return platformResult(call, "PATCH", `/v1/environments/${encodeURIComponent(String(args.id))}`, { services: next }, true, signal);
    },
    "environment.service.rm": async ({ args, signal }) => {
      const current = await services(String(args.id), signal);
      if (!current.ok) return current;
      const have = current.value as JsonValue[];
      const next = have.filter((row: any) => row.name !== args.name);
      if (next.length === have.length) return err("no_match", `${args.id} has no service ${args.name}`);
      return platformResult(call, "PATCH", `/v1/environments/${encodeURIComponent(String(args.id))}`, { services: next }, true, signal);
    },
    "workspace.packages.add": async ({ args, signal }) => {
      const current = await packages(String(args.workspace), signal);
      if (!current.ok) return current;
      const have = current.value as string[];
      const attr = (entry: string) => entry.split("@")[0];
      const added = args.packages as string[];
      const next = [...have.filter((entry) => !added.some((item) => attr(item) === attr(entry))), ...added];
      return platformResult(call, "PATCH", `/v1/workspaces/${encodeURIComponent(String(args.workspace))}`, { packages: next }, true, signal);
    },
    "workspace.packages.rm": async ({ args, signal }) => {
      const current = await packages(String(args.workspace), signal);
      if (!current.ok) return current;
      const have = current.value as string[];
      const remove = new Set((args.packages as string[]).map((entry) => entry.split("@")[0]));
      const next = have.filter((entry) => !remove.has(entry.split("@")[0]));
      if (next.length === have.length) return err("no_match", "none of those packages is installed");
      return platformResult(call, "PATCH", `/v1/workspaces/${encodeURIComponent(String(args.workspace))}`, { packages: next }, true, signal);
    },
  };
}

export function skillAdapter(): Adapter {
  return async ({ args }) => {
    const root = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..", "skills");
    if (!args.name) return ok(["workspaces", "environments", "snapshots", "repos", "images", "agents"].map((name) => {
      const body = fs.readFileSync(path.join(root, `${name}.md`), "utf8");
      return { name, description: /^description:\s*(.*)$/m.exec(body)?.[1] ?? "" };
    }));
    const name = String(args.name).trim().toLowerCase().replace(/\.md$/, "");
    try { return ok(fs.readFileSync(path.join(root, `${name}.md`), "utf8")); }
    catch { return err("no_match", `no skill ${args.name}; there are workspaces, environments, snapshots, repos, images, agents`); }
  };
}

export function progressAdapter(call: BenchCall): Adapter {
  return async ({ args }) => {
    const id = encodeURIComponent(String(args.id));
    const [exchanges, messages, procs] = await Promise.all([call("GET", `/exchanges?workspace=${id}`), call("GET", `/workspaces/${id}/messages?limit=10`), call("GET", "/procs")]);
    if (!exchanges.ok || !messages.ok) return err("provider_failure", String((exchanges.ok ? messages.data : exchanges.data)?.error ?? "the bench could not be read"), true);
    return ok({ asks: exchanges.data, messages: messages.data?.messages ?? [], processes: Array.isArray(procs.data) ? procs.data.filter((row: any) => row.workspace === String(args.id)) : [] });
  };
}

export function resolveWorkspaceProgress(platform: PlatformCall, bench: BenchCall, ownBench?: string, team = process.env.KL_TEAM): Adapter {
  const list = createPlatformAdapters(platform, ownBench, team)["workspace.list"];
  const progress = progressAdapter(bench);
  return async (input) => {
    const requested = String(input.args.id);
    if (requested === ownBench || /^bench-[0-9a-f]{8,}$/.test(requested)) return err("scope_denied", "that is you, not a workspace; name a workspace");
    const listed = await list({ args: {}, states: {}, signal: input.signal });
    if (!listed.ok) return listed;
    const resolved = resolveUnique(listed.value as JsonValue[], requested);
    // Not in the listing: hand it on as given, so the bench's own not-found answer is the
    // response, rather than a lookup failure of ours (a team workspace addressed by id).
    if (!resolved.ok && resolved.code !== "no_match") return err(resolved.code, resolved.message);
    return progress({ ...input, args: { id: resolved.ok ? resolved.id : requested } });
  };
}

export const validationError = (issues: ValidationIssue[]): AdapterResult => err("validation_failure", issues.map((issue) => `${issue.path}: ${issue.message}`).join("; "));
