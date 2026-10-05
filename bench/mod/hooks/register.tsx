// Wires the bench session to a workspace's own tool server. Thin on purpose: all the HTTP and
// retry logic lives in `lib/platform.ts` (pure, `node --test`-able); this file only adapts the
// mods API's `$.http.fetch` ({status, ok, headers, text}) to a fetch-shaped function so `lib/`
// never has to know it's running inside a mod, and wires the handful of events spike.md confirmed.
//
// No `process`, no `node:*` imports: a hooks module runs inside the mods sandbox, which only
// gives it "claude-code" and its own relative files (`claude plugin validate` enforces this) — env
// vars go through `$.env.get`, file reads through `$.fs.read`.
import type { Register } from "claude-code";
import { callTool, DISABLED, listTools, toolsAt, workspaceContext, type At } from "../lib/platform.ts";

const TOOL_PREFIX = "mcp__kloudlite__";

type Env = { env: { get: (name: string) => Promise<string | undefined> } };
type Fs = { fs: { read: (path: string) => Promise<string> } };
type Http = { http: { fetch: (url: string, init?: Record<string, unknown>) => Promise<{ status: number; ok: boolean; headers: Record<string, string>; text: string }> } };

// Read fresh every call, never cached — the beat rotates it (same reason as the header
// `crates/ide/src/auth.rs` reads per request, not once at startup).
async function readToken($: Env & Fs): Promise<string> {
  const path = (await $.env.get("KL_TOOL_TOKEN_FILE")) || "/etc/kloudlite/bench-tool/token";
  return (await $.fs.read(path)).trim();
}

// `$.http.fetch` has no `signal`: the mods API doesn't bound the request itself, so a timeout is
// enforced here by racing the host's promise against the caller's `AbortSignal.timeout(ms)`. A
// race that loses still leaves the underlying `$.http.fetch` call running until the host's own
// ceiling — ponytail: no cancellation of the in-flight host call, acceptable because every caller
// in lib/platform.ts treats the race's rejection the same as a connection error (names the
// workspace, says start it), upgrade path is a `$.http.fetch` signal option if one ships.
function adaptFetch($: Http) {
  return async (url: string, init: Record<string, unknown> = {}): Promise<Response> => {
    const signal = init.signal as AbortSignal | undefined;
    const call = $.http.fetch(url, { method: init.method, headers: init.headers, body: init.body });
    const raced = signal
      ? await new Promise<Awaited<typeof call>>((resolve, reject) => {
          if (signal.aborted) return reject(new DOMException("timed out", "TimeoutError"));
          const onAbort = () => reject(new DOMException("timed out", "TimeoutError"));
          signal.addEventListener("abort", onAbort, { once: true });
          call.then(
            (v) => (signal.removeEventListener("abort", onAbort), resolve(v)),
            (e) => (signal.removeEventListener("abort", onAbort), reject(e)),
          );
        })
      : await call;
    return new Response(raced.text, { status: raced.status });
  };
}

export function register(on: Register) {
  on("session.start", async ($, e, next) => {
    const ws = await $.env.get("KL_WORKSPACE");
    if (!ws) return next(e); // main session: no workspace, no tools to proxy
    const api = (await $.env.get("KL_API_URL")) ?? "";
    const fetch = adaptFetch($);
    try {
      const at = await toolsAt(fetch, api, await readToken($), ws);
      const tools = await listTools(fetch, at);
      for (const t of tools) {
        await $.tool.register({ name: t.name, description: t.description, inputSchema: t.input_schema });
      }
    } catch (err) {
      $.ui.log("kloudlite: no workspace tools: " + err);
    }
    return next(e);
  });

  on("tool.check", async ($, e, next) => {
    if (DISABLED.includes(e.tool)) {
      return { decision: "deny", reason: "this bench runs no local tools; use the workspace's tools" };
    }
    return next(e);
  });

  // Main session only (no KL_WORKSPACE): a bare bench process has no subagent session to hand a
  // spawned Agent to. Text verbatim from the reference mod's own `tool.call { tool: 'Agent' }` hook.
  on("tool.call", { tool: "Agent" }, async ($, e, next) => {
    if (!e.agentId && !(await $.env.get("KL_WORKSPACE"))) {
      return { deny: "Main session cannot spawn subagents. Send the task to a workspace session instead." };
    }
    return next(e);
  });

  // One handler for every `mcp__kloudlite__*` tool `session.start` registered: the name tells
  // `lib/platform.ts` which workspace-side tool to call, so there's nothing per-tool to wire.
  on("tool.call", async ($, e, next) => {
    const ws = await $.env.get("KL_WORKSPACE");
    if (!ws || !e.tool.startsWith(TOOL_PREFIX)) return next(e);
    const name = e.tool.slice(TOOL_PREFIX.length);
    const api = (await $.env.get("KL_API_URL")) ?? "";
    const fetch = adaptFetch($);
    let cached: At | undefined;
    const getAt = async (fresh = false) => {
      if (!fresh && cached) return cached;
      cached = await toolsAt(fetch, api, await readToken($), ws);
      return cached;
    };
    const r = await callTool(fetch, getAt, ws, name, e.input);
    return r.ok ? { result: r.text } : { result: r.error, isError: true };
  });

  on("prompt.context", async ($, e, next) => {
    const ws = await $.env.get("KL_WORKSPACE");
    if (!ws) return next(e);
    const api = (await $.env.get("KL_API_URL")) ?? "";
    const fetch = adaptFetch($);
    let cached: At | undefined;
    const getAt = async (fresh = false) => {
      if (!fresh && cached) return cached;
      cached = await toolsAt(fetch, api, await readToken($), ws);
      return cached;
    };
    try {
      return { blocks: [await workspaceContext(fetch, getAt)] };
    } catch {
      return next(e); // workspace unreachable at session start — no context, not a crash
    }
  });
}
