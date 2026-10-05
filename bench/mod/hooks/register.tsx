// Wires the bench session to a workspace's own tool server. Thin on purpose: all the HTTP and
// retry logic lives in `lib/platform.ts` (pure, `node --test`-able); this file only adapts the
// mods API's `$.http.fetch` ({status, ok, headers, text}) to a fetch-shaped function so `lib/`
// never has to know it's running inside a mod, and wires the handful of events spike.md confirmed.
//
// No `process`, no `node:*` imports: a hooks module runs inside the mods sandbox, which only
// gives it "claude-code" and its own relative files (`claude plugin validate` enforces this) — env
// vars go through `$.env.get`, file reads through `$.fs.read`.
import { atom, read, update } from "claude-code";
import type { Register, StateDollar } from "claude-code";
import { callTool, isDisabledBuiltin, listTools, toolArgsOf, toolsAt, workspaceContext, type At } from "../lib/platform.ts";
import { buildView, type EnvRow, type Row, type SessionsState, type V1Environment, type V1Workspace } from "../lib/view.ts";

const TOOL_PREFIX = "mcp__kloudlite__";
const PANE = "kloudlite";
const SESSIONS = "http://127.0.0.1:8917";
const V1_TIMEOUT_MS = 10_000;

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

// ---- pane state: view atoms only (the brief's ruling 6) ----
const scroll = atom({ plugin: "kloudlite", key: "scroll" } as const, 0);
const selected = atom({ plugin: "kloudlite", key: "selected" } as const, null as string | null);
const envId = atom({ plugin: "kloudlite", key: "envId" } as const, null as string | null);
const viewAtom = atom(
  { plugin: "kloudlite", key: "view" } as const,
  { workspaces: [] as Row[], environment: null as EnvRow | null },
);
const rawAgents = atom({ plugin: "kloudlite", key: "rawAgents" } as const, {} as SessionsState);
const pollError = atom({ plugin: "kloudlite", key: "pollError" } as const, null as string | null);
const tickAtom = atom({ plugin: "kloudlite", key: "tick" } as const, 0);
const tasksAtom = atom({ plugin: "kloudlite", key: "tasks" } as const, [] as { id: string; cmd: string; state: string }[]);
const FRAMES = ["·", "✢", "✳", "✻", "✽", "✻", "✳", "✢"];

// Fetches process_list for `ws` and writes it into tasksAtom. Shared by the 2s poll (gated to the
// open workspace) and by the process_kill action handler's one-shot refresh. Own try/catch so a
// tasks fetch failure never wipes the rest of the poll or the pane's main view (ruling 5).
async function refreshTasks(
  $: Env & Fs & StateDollar,
  fetch: (url: string, init?: Record<string, unknown>) => Promise<Response>,
  api: string,
  ws: string,
): Promise<void> {
  try {
    const getAt = async () => toolsAt(fetch, api, await readToken($), ws);
    const r = await callTool(fetch, getAt, ws, "process_list", {});
    if (r.ok) await update($, tasksAtom, () => (JSON.parse(r.text).processes ?? []) as { id: string; cmd: string; state: string }[]);
  } catch (err) {
    await update($, pollError, () => (err instanceof Error ? err.message : String(err)));
  }
}

const STATUS_STYLE = {
  idle: { color: "cyan", glyph: "◆" },
  running: { color: "yellow", glyph: "spin" },
  errored: { color: "red", glyph: "✖" },
  stopped: { color: "red", glyph: "○" },
} as const;

export const register: Register = (on) => {
  on("session.start", async ($, e, next) => {
    // Task 3 behaviour: a workspace session proxies the workspace's own tools. Unchanged.
    const ws = await $.env.get("KL_WORKSPACE");
    const api = (await $.env.get("KL_API_URL")) ?? "";
    const fetch = adaptFetch($);
    if (ws) {
      try {
        const at = await toolsAt(fetch, api, await readToken($), ws);
        const tools = await listTools(fetch, at, ws);
        for (const t of tools) {
          await $.tool.register({ name: t.name, description: t.description, inputSchema: t.input_schema });
        }
      } catch (err) {
        $.ui.log("kloudlite: no workspace tools: " + err);
      }
      return next(e);
    }

    // Main session only: the real pane (ruling 1 — a workspace session draws no pane).
    await $.command.register({ name: "kloudlite", description: "Open the workspaces side panel" });
    await $.command.register({ name: "env", description: "Switch the attached environment" });
    void $.ui.open({ id: PANE, title: "Kloudlite", columns: 48 });

    const v1 = async <T,>(path: string): Promise<T> => {
      const res = await fetch(`${api}${path}`, {
        headers: { authorization: `Bearer ${await readToken($)}` },
        signal: AbortSignal.timeout(V1_TIMEOUT_MS),
      });
      if (!res.ok) throw new Error(`${path} failed with status ${res.status}`);
      return (await res.json()) as T;
    };

    $.clock.every(2000, async () => {
      try {
        const workspaces = await v1<V1Workspace[]>("/v1/workspaces");
        let selectedEnv = await read($, envId);
        if (!selectedEnv) {
          const envs = await v1<{ id: string }[]>("/v1/environments");
          selectedEnv = envs[0]?.id ?? null;
          await update($, envId, () => selectedEnv);
        }
        const envs: V1Environment[] = selectedEnv ? [await v1<V1Environment>(`/v1/environments/${encodeURIComponent(selectedEnv)}`)] : [];
        const stateRes = await fetch(`${SESSIONS}/state`, { signal: AbortSignal.timeout(V1_TIMEOUT_MS) });
        if (!stateRes.ok) throw new Error(`sessions service returned ${stateRes.status}`);
        const state = (await stateRes.json()) as SessionsState;
        await update($, rawAgents, () => state);
        await update($, viewAtom, () => buildView(workspaces, envs, state));
        await update($, pollError, () => null);
      } catch (err) {
        // ruling 5: never throw out of the clock callback; keep the last good view, show one line
        await update($, pollError, () => (err instanceof Error ? err.message : String(err)));
      }

      // Poll tasks only for the workspace whose detail view is open, and only while it's open.
      const sel = await read($, selected);
      const view = await read($, viewAtom);
      const openWs = sel && view.workspaces.some((w) => w.id === sel) ? sel : null;
      if (openWs) await refreshTasks($, fetch, api, openWs);
      else await update($, tasksAtom, () => []);
    });

    $.clock.every(120, async () => {
      const anyRunning = (await read($, viewAtom)).workspaces.some((w) => w.status === "running");
      if (anyRunning) await update($, tickAtom, (t) => t + 1);
    });

    return next(e);
  });

  on("command.run", { command: "kloudlite" }, async ($) => {
    await $.ui.open({ id: PANE, title: "Kloudlite", columns: 48 });
    return { text: "Workspaces panel opened." };
  });

  on("command.run", { command: "env" }, async ($) => {
    const api = (await $.env.get("KL_API_URL")) ?? "";
    const fetch = adaptFetch($);
    try {
      const res = await fetch(`${api}/v1/environments`, {
        headers: { authorization: `Bearer ${await readToken($)}` },
        signal: AbortSignal.timeout(V1_TIMEOUT_MS),
      });
      if (!res.ok) throw new Error(`request failed with status ${res.status}`);
      const envs = (await res.json()) as { id: string; name: string }[];
      const current = await read($, envId);
      const pick = await $.ui.ask(`Switch environment (current: ${current ?? "none"})`, {
        options: envs.map((e) => e.name),
        header: "Environment",
      });
      const env = envs.find((e) => e.name === pick);
      if (!env || env.id === current) return { text: `Environment unchanged.` };
      await update($, envId, () => env.id);
      return { text: `Switched environment to ${env.name}.` };
    } catch (err) {
      return { text: `Could not switch environment: ${err instanceof Error ? err.message : String(err)}` };
    }
  });

  on("ui.scroll", { requestId: PANE }, async ($, e) => {
    await update($, scroll, (off) => Math.max(0, off + Math.sign(e.by)));
    return {};
  });

  on("tool.call", { tool: "Agent" }, async ($, e, next) => {
    if (!e.agentId && !(await $.env.get("KL_WORKSPACE"))) {
      return { deny: "Main session cannot spawn subagents. Send the task to a workspace session instead." };
    }
    return next(e);
  });

  // Backstop: `kl-sessions` already passes these as `disallowedTools`, but a plugin or a resumed
  // session must never see them answered by the bench's own filesystem (spec §1, "Fails closed").
  on("tool.call", async ($, e, next) => {
    if (isDisabledBuiltin(e.tool)) {
      return { deny: "This bench runs no local tools; use the workspace's tools (mcp__kloudlite__*) instead." };
    }
    return next(e);
  });

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
    const r = await callTool(fetch, getAt, ws, name, toolArgsOf(e));
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
      const text = await workspaceContext(fetch, getAt, ws);
      return { blocks: [...e.blocks, { name: "kloudlite", text }] };
    } catch {
      return next(e); // workspace unreachable at session start — no context, not a crash
    }
  });

  on("ui.render", { component: "Pane", requestId: PANE }, async ($, e) => {
    const table = $.ui.resolve(e);
    const { Box, Text, Button } = table;
    const Input = "Input" in table ? table.Input : undefined;
    const view = await read($, viewAtom);
    const err = await read($, pollError);
    const sel = await read($, selected);
    const rows = e.props.scroll.bodyRows;
    const spin = FRAMES[(await read($, tickAtom)) % FRAMES.length]!;
    const api = (await $.env.get("KL_API_URL")) ?? "";
    const fetch = adaptFetch($);

    const sendTo = async (ws: string, text: string, agentId?: string) => {
      await fetch(`${SESSIONS}/send`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(agentId ? { ws, agentId, text } : { ws, text }),
        signal: AbortSignal.timeout(V1_TIMEOUT_MS),
      }).catch(() => update($, pollError, () => `could not reach session service`));
    };

    // ---- detail view ----
    const wsRow = sel ? view.workspaces.find((w) => w.id === sel) : undefined;
    const agentRow = !wsRow && sel ? view.workspaces.flatMap((w) => w.agents.map((a) => ({ ...a, ws: w.id }))).find((a) => a.id === sel) : undefined;
    if (wsRow || agentRow) {
      const ws = wsRow ? wsRow.id : agentRow!.ws;
      const lines = wsRow ? (await read($, rawAgents))[ws]?.lines ?? [] : (await read($, rawAgents))[ws]?.agents.find((a) => a.id === sel)?.lines ?? [];
      const slice = lines.slice(-(rows - 8));

      // TASKS: polled into tasksAtom by the 2s clock while this workspace's detail is open; render
      // only reads it, never fetches (the 120ms spinner clock re-runs render, so a fetch here would
      // hit the tool server far faster than this screen needs).
      const tasks = wsRow ? await read($, tasksAtom) : [];

      return (
        <Box flexDirection="column" paddingX={1} height={rows} overflow="hidden">
          <Box justifyContent="space-between">
            <Box gap={1}>
              <Button key="back" label="‹" hotkey="b" variant="secondary" onPress={() => void update($, selected, () => null)} />
              <Text bold>{wsRow ? wsRow.name : agentRow!.label}</Text>
            </Box>
            {(wsRow?.status === "errored") && <Text color="red">✖ error</Text>}
          </Box>

          <Box flexDirection="column" flexGrow={1} overflow="hidden" marginTop={1}>
            {slice.length === 0 && (
              <Box flexDirection="column">
                <Text>
                  <Text color="cyan">✳ </Text>Welcome to <Text bold>{wsRow ? wsRow.name : agentRow!.label}</Text>
                </Text>
                <Text dimColor>  warm session · context persists across restarts</Text>
              </Box>
            )}
            {slice.map((line) => {
              const body = line.slice(2);
              if (line.startsWith("t:"))
                return (
                  <Box gap={1}>
                    <Text color="green">⏺</Text>
                    <Text wrap="truncate-end">{body}</Text>
                  </Box>
                );
              if (line.startsWith("a:"))
                return (
                  <Box gap={1} marginTop={1}>
                    <Text>⏺</Text>
                    <Text wrap="wrap">{body.slice(0, 10000)}</Text>
                  </Box>
                );
              if (line.startsWith("u:"))
                return (
                  <Box gap={1} marginTop={1}>
                    <Text dimColor>{">"}</Text>
                    <Text dimColor wrap="wrap">{body}</Text>
                  </Box>
                );
              if (line.startsWith("r:")) return <Text dimColor wrap="truncate-end">{"  ⎿  "}{body}</Text>;
              if (line.startsWith("s:")) return <Text dimColor wrap="truncate-end">· {body}</Text>;
              return <Text dimColor wrap="truncate-end">{line}</Text>;
            })}
            {wsRow?.status === "running" && (
              <Box marginTop={1}>
                <Text color="yellow">✳ working…</Text>
              </Box>
            )}
          </Box>

          {wsRow && tasks.length > 0 && (
            <Box flexDirection="column" marginTop={1}>
              <Text bold dimColor>TASKS</Text>
              {tasks.map((t) => (
                <Box gap={1}>
                  <Text color={t.state === "running" ? "green" : "blue"}>{t.state === "running" ? spin : "✔"}</Text>
                  <Text wrap="truncate-end">{t.cmd.slice(0, 32)}</Text>
                  {t.state === "running" && (
                    <Button
                      key={`kill:${t.id}`}
                      label="✕ kill"
                      variant="secondary"
                      onPress={() =>
                        void callTool(fetch, async () => toolsAt(fetch, api, await readToken($), ws), ws, "process_kill", { id: t.id }).then(() =>
                          refreshTasks($, fetch, api, ws),
                        )
                      }
                    />
                  )}
                </Box>
              ))}
            </Box>
          )}

          {wsRow && (
            <Box flexDirection="column" marginTop={1}>
              {wsRow.agents.map((a) => (
                <Box gap={1}>
                  <Button key={`open:${a.id}`} label={a.label.slice(0, 32)} variant="secondary" onPress={() => void update($, selected, () => a.id)} />
                  <Text dimColor>{a.status}</Text>
                </Box>
              ))}
            </Box>
          )}

          {Input && (
            <Box marginTop={1} paddingX={1} borderStyle="round" borderDimColor>
              <Text>{"> "}</Text>
              <Input
                key={`chat:${sel}`}
                autoFocus
                submitLabel=" "
                onSubmit={(text: string) => {
                  if (!text.trim()) return;
                  void sendTo(ws, text, wsRow ? undefined : agentRow!.id);
                }}
              />
            </Box>
          )}
        </Box>
      );
    }

    // ---- list view ----
    const off = await read($, scroll);
    const visible = view.workspaces.slice(off);

    return (
      <Box flexDirection="column" paddingX={1} height={rows} overflow="hidden">
        <Box gap={1}>
          <Text bold color="cyan">◆ KLOUDLITE</Text>
          {view.environment && <Text dimColor>· {view.environment.name}</Text>}
        </Box>
        {err && <Text color="red">⚠ {err.slice(0, 60)}</Text>}
        <Box marginTop={1}>
          <Text bold dimColor>WORKSPACES</Text>
        </Box>

        <Box flexDirection="column" flexGrow={1} flexShrink={1} overflow="hidden">
          {off > 0 && <Text dimColor>↑ {off} more</Text>}
          {visible.map((row, i) => {
            const style = STATUS_STYLE[row.status];
            const num = off + i + 1;
            return (
              <Box flexDirection="column" marginTop={1}>
                <Box gap={1}>
                  <Text color={style.color}>{row.status === "running" ? spin : style.glyph}</Text>
                  <Button
                    key={`ws:${row.id}`}
                    label={row.name}
                    {...(num <= 9 ? { hotkey: String(num) } : {})}
                    variant="secondary"
                    onPress={() => void update($, selected, () => row.id)}
                  />
                  <Text dimColor={row.status !== "errored"} color={row.status === "errored" ? "red" : undefined}>{row.status}</Text>
                  {row.intercepts.map((i) => (
                    <Text color="magenta">⇄ {i}</Text>
                  ))}
                </Box>
                {(row.status === "running" || row.queued > 0) && (
                  <Box gap={1} marginLeft={2}>
                    {row.status === "running" && <Text dimColor>{row.doing}</Text>}
                    {row.queued > 0 && <Text color="yellow">⧗ {row.queued} queued</Text>}
                  </Box>
                )}
                {row.agents.map((a, idx, arr) => (
                  <Box gap={1} marginLeft={2}>
                    <Text dimColor>{idx === arr.length - 1 ? "└" : "├"}</Text>
                    <Text color={a.status === "running" ? "green" : a.status === "error" ? "red" : "blue"}>
                      {a.status === "running" ? spin : a.status === "error" ? "✖" : "✔"}
                    </Text>
                    <Button key={`open:${a.id}`} label={a.label.slice(0, 32)} variant="secondary" onPress={() => void update($, selected, () => a.id)} />
                  </Box>
                ))}
              </Box>
            );
          })}
        </Box>

        {view.environment && (
          <Box flexDirection="column" marginTop={1}>
            <Box gap={1}>
              <Text bold dimColor>ENVIRONMENT</Text>
              <Text>{view.environment.name}</Text>
            </Box>
            {view.environment.services.map((svc) => (
              <Box gap={1} justifyContent="space-between">
                <Box gap={1}>
                  <Text>{svc.name}</Text>
                  <Text dimColor>:{svc.ports.join(",")}</Text>
                  {svc.interceptedBy && <Text color="magenta">⇄ {svc.interceptedBy}</Text>}
                </Box>
              </Box>
            ))}
          </Box>
        )}
      </Box>
    );
  });
};
