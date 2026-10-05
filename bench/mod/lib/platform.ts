//! Pure HTTP clients for the workspace's own tool server (`kl ide serve`, `crates/ide/src/api.rs`)
//! and the `/v1/workspaces/{id}/tools` address lookup (`crates/workspaces/src/api/workspaces/mod.rs`,
//! `ws_tools`). Every function takes `fetch` as a parameter (never the global) so `register.tsx`
//! can hand it an adapter over `$.http.fetch`, and the tests here can hand it a fake — this file
//! never touches the mods API directly, which is what makes it `node --test`-able standalone.
//!
//! Timeouts are a parameter (`AbortSignal.timeout`) on every call, not left to the caller's hook
//! budget: a stalled remote tool must surface as a tool error naming the workspace, never a hang
//! (see `.superpowers/sdd/2026-10-05-claude-code-bench/spike.md`, "10s hook limit vs tool handler
//! time limit").

export type At = { address: string; token?: string };

export type Tool = { name: string; description: string; input_schema: Record<string, unknown> };

export type CallResult = { ok: true; text: string } | { ok: false; error: string };

// Same list as Global Constraints: built-ins the bench disallows because the workspace's own
// tools (over the remote `kl ide serve`) replace them.
export const DISABLED = [
  "Read",
  "Write",
  "Edit",
  "MultiEdit",
  "Bash",
  "Grep",
  "Glob",
  "NotebookEdit",
  "WebFetch",
];

// `tool.call`'s event is `{ tool, tool_use_id, agentId?, ...toolArguments }` (no `e.input`): the
// envelope fields (plus the reserved `consent`) sit alongside the tool's own arguments, so the
// only way to get the arguments is to strip the envelope's own keys back off.
export function toolArgsOf(e: Record<string, unknown>): Record<string, unknown> {
  const { tool, tool_use_id, agentId, consent, ...args } = e;
  return args;
}

const V1_TIMEOUT_MS = 10_000;
const TOOLS_LIST_TIMEOUT_MS = 10_000;
const TOOL_CALL_TIMEOUT_MS = 600_000;

type FetchLike = (url: string, init?: Record<string, unknown>) => Promise<Response>;

async function errorOf(res: Response, fallback: string): Promise<string> {
  try {
    const body = await res.json();
    if (body && typeof body.error === "string") return body.error;
  } catch {
    // body wasn't JSON (or wasn't readable) — fall through to the generic message
  }
  return fallback;
}

// `crates/ide/src/auth.rs` `require`: every tool-server route except `/healthz` 401s without
// `authorization: Bearer <token>`. Never send one of these requests unauthenticated — missing a
// token is a known, named state (before the keys beat has written `workspace-token` into the pod,
// `ws_tools` answers with no `token` field at all), not a reason to probe the server anyway.
function tokenHeader(at: At, ws: string): Record<string, string> {
  if (!at.token) throw new Error(`workspace ${ws} no tool token yet; retry shortly`);
  return { authorization: `Bearer ${at.token}` };
}

/// `GET {api}/v1/workspaces/{ws}/tools`: the owner's tool server address. A non-2xx throws
/// `Error(body.error)` so a stopped/between-pods workspace surfaces its own message verbatim.
export async function toolsAt(fetch: FetchLike, api: string, token: string, ws: string): Promise<At> {
  const res = await fetch(`${api}/v1/workspaces/${encodeURIComponent(ws)}/tools`, {
    headers: { authorization: `Bearer ${token}` },
    signal: AbortSignal.timeout(V1_TIMEOUT_MS),
  });
  if (!res.ok) throw new Error(await errorOf(res, `request failed with status ${res.status}`));
  return (await res.json()) as At;
}

/// `GET http://{address}/tools`: `{"tools": [{name, description, schema}]}` per `crates/ide/src/api.rs`.
export async function listTools(fetch: FetchLike, at: At, ws: string): Promise<Tool[]> {
  const res = await fetch(`http://${at.address}/tools`, {
    headers: tokenHeader(at, ws),
    signal: AbortSignal.timeout(TOOLS_LIST_TIMEOUT_MS),
  });
  if (!res.ok) throw new Error(await errorOf(res, `request failed with status ${res.status}`));
  const body = (await res.json()) as { tools?: { name: string; description: string; schema: Record<string, unknown> }[] };
  return (body.tools ?? []).map((t) => ({ name: t.name, description: t.description, input_schema: t.schema }));
}

/// `POST http://{address}/tools/{name}`. A 401 means the address's token rotated under us
/// (`KL_TOOL_TOKEN_FILE` is re-read per call, but the cached `at` handed in here may be stale) —
/// refetch the address once via `getAt(true)` and retry once. Any thrown error (DNS, connection
/// refused, timeout) means the pod isn't reachable at all: never bubble the raw error, name the
/// workspace and say to start it.
export async function callTool(
  fetch: FetchLike,
  getAt: (fresh?: boolean) => Promise<At>,
  ws: string,
  name: string,
  input: unknown,
): Promise<CallResult> {
  const noToken = (): CallResult => ({ ok: false, error: `workspace ${ws} no tool token yet; retry shortly` });
  const post = (at: At) =>
    fetch(`http://${at.address}/tools/${encodeURIComponent(name)}`, {
      method: "POST",
      headers: { "content-type": "application/json", ...tokenHeader(at, ws) },
      body: JSON.stringify(input ?? {}),
      signal: AbortSignal.timeout(TOOL_CALL_TIMEOUT_MS),
    });
  try {
    let at = await getAt();
    if (!at.token) return noToken();
    let res = await post(at);
    if (res.status === 401) {
      at = await getAt(true);
      if (!at.token) return noToken();
      res = await post(at);
    }
    if (!res.ok) return { ok: false, error: await errorOf(res, `request failed with status ${res.status}`) };
    const body = await res.json();
    return { ok: true, text: typeof body === "string" ? body : JSON.stringify(body, null, 2) };
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    return { ok: false, error: `workspace ${ws} is unreachable: ${msg}; start it` };
  }
}

/// Everything the first prompt gets for free: the workspace's own `CLAUDE.md` plus `GET /fs/git`
/// (`crates/ide/src/fs/mod.rs`, `git_state`). Each section is best-effort — a missing CLAUDE.md or
/// a not-yet-a-repo workspace must not make the whole context call fail.
export async function workspaceContext(fetch: FetchLike, getAt: (fresh?: boolean) => Promise<At>, ws: string): Promise<string> {
  const at = await getAt();
  const sections: string[] = [];
  try {
    const res = await fetch(`http://${at.address}/fs/file?path=CLAUDE.md`, {
      headers: tokenHeader(at, ws),
      signal: AbortSignal.timeout(TOOLS_LIST_TIMEOUT_MS),
    });
    if (res.ok) sections.push(await res.text());
  } catch {
    // no token yet, no CLAUDE.md, or the pod isn't reachable — skip this section, not the whole context
  }
  try {
    const res = await fetch(`http://${at.address}/fs/git`, {
      headers: tokenHeader(at, ws),
      signal: AbortSignal.timeout(TOOLS_LIST_TIMEOUT_MS),
    });
    if (res.ok) sections.push(JSON.stringify(await res.json(), null, 2));
  } catch {
    // no token yet, not a repo yet, or unreachable — skip
  }
  return sections.join("\n\n");
}
