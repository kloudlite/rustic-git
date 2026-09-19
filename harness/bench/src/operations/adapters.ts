import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import type { ArgumentStates } from "./arguments.ts";
import type { JsonValue, ValidationIssue } from "./shape.ts";
import type { OperationErrorCode } from "./contracts.ts";

export type PlatformCall = (method: string, route: string, body?: unknown) => Promise<{ status: number; data: unknown }>;
export type BenchCall = (method: string, route: string, body?: unknown) => Promise<{ ok: boolean; data: any }>;
export type AdapterInput = { args: Record<string, JsonValue>; states: ArgumentStates; signal?: AbortSignal };
export type AdapterError = { code: OperationErrorCode; message: string; retryable: boolean };
export type AdapterResult = { ok: true; value: JsonValue } | { ok: false; error: AdapterError };
export type Adapter = (input: AdapterInput) => Promise<AdapterResult>;

const err = (code: OperationErrorCode, message: string, retryable = false): AdapterResult => ({ ok: false, error: { code, message, retryable } });
const ok = (value: unknown): AdapterResult => ({ ok: true, value: (value ?? null) as JsonValue });

export function classifyHttp(status: number, message: string, mutation: boolean): AdapterResult {
  if (status === 401 || status === 403) return err("permission_denied", message);
  if (status === 404) return err("no_match", message);
  if (status === 409) return err("revision_conflict", message);
  if (status === 400 || status === 422) return err("validation_failure", message);
  return err(status >= 500 ? "provider_failure" : "execution_failure", message, status >= 500);
}

export async function platformResult(call: PlatformCall, method: string, route: string, body?: unknown, mutation = false): Promise<AdapterResult> {
  try {
    const response = await call(method, route, body);
    return response.status >= 400 ? classifyHttp(response.status, typeof response.data === "string" ? response.data : JSON.stringify(response.data), mutation) : ok(response.data);
  } catch (error) {
    // ponytail: once PlatformCall exposes request write/response phases, distinguish failures
    // before dispatch. Today a thrown mutation transport is conservatively unknown because the
    // provider may have committed it; an HTTP response, including 5xx, is definitive failure.
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

export function createSharedAdapters(input: { platform: PlatformCall; bench: BenchCall; ownBench?: string }): Record<string, Adapter> {
  return { ...createPlatformAdapters(input.platform, input.ownBench), "workspace.progress": progressAdapter(input.bench), "skill.read": skillAdapter() };
}

export function createPlatformAdapters(call: PlatformCall, ownBench = process.env.KL_WORKSPACE_ID): Record<string, Adapter> {
  const workspaceList: Adapter = async ({ args }) => {
    const result = await platformResult(call, "GET", `/v1/workspaces${args.team ? `?team=${encodeURIComponent(String(args.team))}` : ""}`);
    return result.ok ? ok(visibleWorkspaces(result.value, ownBench)) : result;
  };
  const workspaceInspect: Adapter = async ({ args }) => {
    const requested = String(args.id);
    if (requested === ownBench || /^bench-[0-9a-f]{8,}$/.test(requested)) return err("scope_denied", "that is you, not a workspace; name a workspace");
    const listed = await workspaceList({ args: {}, states: {} });
    if (!listed.ok) return listed;
    const resolved = resolveUnique(listed.value as JsonValue[], requested);
    if (!resolved.ok) return err(resolved.code, resolved.message);
    return platformResult(call, "GET", `/v1/workspaces/${encodeURIComponent(resolved.id)}`);
  };
  const services = async (id: string): Promise<AdapterResult> => {
    const environment = await platformResult(call, "GET", `/v1/environments/${encodeURIComponent(id)}`);
    return environment.ok ? ok(((environment.value as any)?.services ?? []) as JsonValue) : environment;
  };
  const packages = async (id: string): Promise<AdapterResult> => {
    const workspace = await platformResult(call, "GET", `/v1/workspaces/${encodeURIComponent(id)}`);
    return workspace.ok ? ok(((workspace.value as any)?.packages ?? []) as JsonValue) : workspace;
  };
  return {
    "workspace.list": workspaceList,
    "workspace.inspect": workspaceInspect,
    "workspace.create": ({ args }) => platformResult(call, "POST", args.from_snapshot ? "/v1/workspaces/restore" : "/v1/workspaces", args.from_snapshot ? { name: args.name, snapshot_id: args.from_snapshot, packages: args.packages } : args, true),
    "environment.create": ({ args }) => platformResult(call, "POST", args.from_snapshot ? "/v1/environments/restore" : "/v1/environments", args.from_snapshot ? { name: args.name, snapshot_id: args.from_snapshot, services: args.services } : args, true),
    "environment.restore": ({ args }) => platformResult(call, "POST", `/v1/environments/${encodeURIComponent(String(args.id))}/restore-in-place`, { snapshot_id: args.snapshot }, true),
    "environment.intercept": ({ args, states }) => states.workspace?.kind === "explicitly_clear"
      ? platformResult(call, "DELETE", `/v1/environments/${encodeURIComponent(String(args.id))}/intercepts/${encodeURIComponent(String(args.service))}`, undefined, true)
      : platformResult(call, "POST", `/v1/environments/${encodeURIComponent(String(args.id))}/intercepts`, { service: args.service, workspace: args.workspace, ...(args.ports === undefined ? {} : { ports: args.ports }) }, true),
    "environment.service.put": async ({ args }) => {
      const current = await services(String(args.id));
      if (!current.ok) return current;
      const have = current.value as JsonValue[];
      const one = args.service as Record<string, JsonValue>;
      const next = [...have.filter((row: any) => row.name !== one.name), one];
      return platformResult(call, "PATCH", `/v1/environments/${encodeURIComponent(String(args.id))}`, { services: next }, true);
    },
    "environment.service.rm": async ({ args }) => {
      const current = await services(String(args.id));
      if (!current.ok) return current;
      const have = current.value as JsonValue[];
      const next = have.filter((row: any) => row.name !== args.name);
      if (next.length === have.length) return err("no_match", `${args.id} has no service ${args.name}`);
      return platformResult(call, "PATCH", `/v1/environments/${encodeURIComponent(String(args.id))}`, { services: next }, true);
    },
    "workspace.packages.add": async ({ args }) => {
      const current = await packages(String(args.workspace));
      if (!current.ok) return current;
      const have = current.value as string[];
      const attr = (entry: string) => entry.split("@")[0];
      const added = args.packages as string[];
      const next = [...have.filter((entry) => !added.some((item) => attr(item) === attr(entry))), ...added];
      return platformResult(call, "PATCH", `/v1/workspaces/${encodeURIComponent(String(args.workspace))}`, { packages: next }, true);
    },
    "workspace.packages.rm": async ({ args }) => {
      const current = await packages(String(args.workspace));
      if (!current.ok) return current;
      const have = current.value as string[];
      const remove = new Set((args.packages as string[]).map((entry) => entry.split("@")[0]));
      const next = have.filter((entry) => !remove.has(entry.split("@")[0]));
      if (next.length === have.length) return err("no_match", "none of those packages is installed");
      return platformResult(call, "PATCH", `/v1/workspaces/${encodeURIComponent(String(args.workspace))}`, { packages: next }, true);
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

export function resolveWorkspaceProgress(platform: PlatformCall, bench: BenchCall, ownBench?: string): Adapter {
  const list = createPlatformAdapters(platform, ownBench)["workspace.list"];
  const progress = progressAdapter(bench);
  return async (input) => {
    const requested = String(input.args.id);
    if (requested === ownBench || /^bench-[0-9a-f]{8,}$/.test(requested)) return err("scope_denied", "that is you, not a workspace; name a workspace");
    const listed = await list({ args: {}, states: {} });
    if (!listed.ok) return listed;
    const resolved = resolveUnique(listed.value as JsonValue[], requested);
    if (!resolved.ok) return err(resolved.code, resolved.message);
    return progress({ ...input, args: { id: resolved.id } });
  };
}

export const validationError = (issues: ValidationIssue[]): AdapterResult => err("validation_failure", issues.map((issue) => `${issue.path}: ${issue.message}`).join("; "));
