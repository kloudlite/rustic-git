//! The workspace pod's code tools (crates/ide `kl ide serve`, port 7788), reached through the
//! platform: GET /v1/workspaces/{id}/tools gives {address, token?}, then plain HTTP to the pod.
//! The names are a FIXED list so the tools exist even while the pod is not Ready; the pod's own
//! description/schema replace the permissive fallback when it answers at construction.
import type { ToolDef } from "./index.ts";
import { api, apiJson } from "./platform.ts";

export const POD_TOOLS = [
  "read", "write", "edit", "patch", "glob", "grep", "exec",
  "process_list", "process_output", "process_write", "process_kill",
  "watch", "watch_poll", "watch_stop",
  "graft_find_code", "graft_find_all", "graft_trace_calls", "graft_file_api", "graft_repo_map", "graft_build", "graft_blast",
];

type Addr = { address: string; token?: string };
type Listed = { name: string; description?: string; schema?: Record<string, unknown> };

class NotReady extends Error {}

async function lookup(ws: string): Promise<Addr> {
  // api throws on a non-2xx answer; for these callers that is "not ready", not a crash
  const out = await api("GET", `/v1/workspaces/${encodeURIComponent(ws)}/tools`).catch((e) => {
    throw new NotReady(String(e?.message ?? e).replace(/^\d+: /, ""));
  });
  const a = JSON.parse(out);
  if (!a?.address) throw new NotReady("no address");
  return a;
}

/** A ready workspace's tool server, for reads that are not model tool calls (sidebar). One
 * lookup per call: the address and token rotate, and a sidebar beat is 5 s apart. */
async function podFetch<T>(ws: string, path: string, init: RequestInit, ms: number, text = false): Promise<T> {
  const a = await apiJson<Addr>("GET", `/v1/workspaces/${encodeURIComponent(ws)}/tools`);
  if (!a?.address) throw new Error("no address");
  const res = await fetch(`http://${a.address}${path}`, { ...init, headers: { ...(init.headers as object), ...headers(a) }, signal: AbortSignal.timeout(ms) });
  if (!res.ok) throw new Error(`error ${res.status}`); // body withheld: it may echo the token
  return (text ? await res.text() : await res.json()) as T;
}
export const podGet = <T>(ws: string, path: string, ms = 5000) => podFetch<T>(ws, path, {}, ms);
/** A pod file's bytes as text (`/fs/file`). */
export const podText = (ws: string, path: string, ms = 10_000) => podFetch<string>(ws, path, {}, ms, true);
export const podPost = <T>(ws: string, tool: string, args: unknown, ms = 5000) =>
  podFetch<T>(ws, `/tools/${tool}`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(args ?? {}) }, ms);

const headers = (a: Addr): Record<string, string> => (a.token ? { authorization: `Bearer ${a.token}` } : {});

export async function podTools(ws: string): Promise<ToolDef[]> {
  let cache: Addr | undefined;
  const addr = async () => (cache ??= await lookup(ws));

  let listed = new Map<string, Listed>();
  try {
    const a = await addr();
    const res = await fetch(`http://${a.address}/tools`, { headers: headers(a), signal: AbortSignal.timeout(5000) });
    if (res.ok) for (const t of ((await res.json()) as { tools: Listed[] }).tools) listed.set(t.name, t);
  } catch {
    cache = undefined;
  }

  async function post(name: string, args: any, timeout: number): Promise<Response> {
    const a = await addr();
    return fetch(`http://${a.address}/tools/${name}`, {
      method: "POST",
      headers: { "content-type": "application/json", ...headers(a) },
      body: JSON.stringify(args ?? {}),
      signal: AbortSignal.timeout(timeout),
    });
  }

  async function call(name: string, args: any): Promise<string> {
    const timeout = name === "exec" ? (args?.timeout_ms ?? 120_000) + 15_000 : 60_000;
    try {
      let res: Response | undefined;
      // a stale address (pod restarted, token rotated) shows as a network error or 401: re-resolve once
      for (let attempt = 0; attempt < 2; attempt++) {
        try {
          res = await post(name, args, timeout);
          if (res.status !== 401 || attempt) break;
        } catch (e) {
          if (e instanceof NotReady || attempt) throw e;
        }
        cache = undefined;
      }
      const text = await res!.text();
      if (res!.ok) {
        try {
          return JSON.stringify(JSON.parse(text), null, 2);
        } catch {
          return text;
        }
      }
      try {
        return `error: ${JSON.parse(text).error ?? text}`;
      } catch {
        return `error: ${text}`;
      }
    } catch (e: any) {
      return e instanceof NotReady ? `workspace not ready: ${e.message}` : `error: ${e?.message ?? e}`;
    }
  }

  return POD_TOOLS.filter((n) => listed.size === 0 || listed.has(n)).map((name) => {
    const t = listed.get(name);
    return {
      name,
      description: t?.description ? `${t.description} (Runs in workspace ${ws}.)` : `Runs in workspace ${ws}.`,
      inputSchema: t?.schema ?? { type: "object", additionalProperties: true },
      run: (args: any) => call(name, args),
    };
  });
}

export type ExecResult = { code: number; stdout: string; stderr: string };

/** One shell job in workspace `ws` through its tool server (same lookup as `podTools`, fresh each
 * call: callers are the delegate's few git steps, not a hot path). A transport or non-2xx failure
 * comes back as code -1 with the reason in stderr, never a throw. The token stays in the header. */
export async function podExec(ws: string, cmd: string, timeoutMs = 120_000): Promise<ExecResult> {
  try {
    const a = await lookup(ws);
    const res = await fetch(`http://${a.address}/tools/exec`, {
      method: "POST",
      headers: { "content-type": "application/json", ...headers(a) },
      body: JSON.stringify({ cmd, timeout_ms: timeoutMs }),
      signal: AbortSignal.timeout(timeoutMs + 15_000),
    });
    const text = await res.text();
    if (!res.ok) return { code: -1, stdout: "", stderr: `exec ${res.status}: ${text}` };
    const j = JSON.parse(text);
    return { code: j.exit_code ?? -1, stdout: j.stdout ?? "", stderr: j.stderr ?? "" };
  } catch (e: any) {
    return { code: -1, stdout: "", stderr: e instanceof NotReady ? `workspace not ready: ${e.message}` : `${e?.message ?? e}` };
  }
}
