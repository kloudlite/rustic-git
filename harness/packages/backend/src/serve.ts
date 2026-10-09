//! The bench side of a TUI: one `serve` per client connection to the bench daemon (./daemon),
//! speaking ./wire over that connection's socket. The agents live in the daemon's one backend, so
//! the connection closing disposes this client's VIEWS (`dispose` below), never the agents: a
//! running turn survives a quit or an ssh drop and a reconnecting client finds it still going
//! (`session.open` reports `busy`).
import type { Backend, SessionHandle, ToolSpec } from "./index.ts";
import { Peer } from "./wire.ts";

const METHODS = new Set([
  "prompt", "steer", "followUp", "clearQueue", "btw", "abort",
  "setModel", "setThinkingLevel", "setAutoCompactionEnabled", "setCodemode",
]);

export function serve(backend: Backend, peer: Peer) {
  const open = new Map<string, SessionHandle>();

  peer.handle("hello", () => backend.hello());

  peer.handle("session.open", async ({ key, tools, ...o }: { key: string; tools: ToolSpec[] } & any) => {
    // Claude Code drops the WHOLE tool server when one tool has no object schema: the session then
    // runs with no tools and says nothing (a drill client sent `parameters` and lost all 53)
    const bad = tools.find((t: ToolSpec) => (t?.inputSchema as any)?.type !== "object");
    if (bad) throw new Error(`tool ${bad?.name} needs an inputSchema of type "object"`);
    await open.get(key)?.dispose();
    open.delete(key);
    const h = await backend.session(key, {
      ...o,
      tools: tools.map((t: ToolSpec) => ({ ...t, run: (input: unknown) => peer.request<string>("tool", { key, name: t.name, input }) })),
      permission: (req, signal) => peer.request("permission", { key, req }, signal),
      client: true,
    });
    open.set(key, h);
    h.subscribe((event) => peer.emit("session", key, event));
    return { messages: h.messages, isClaude: h.isClaude, busy: h.busy, state: h.state };
  });

  peer.handle("session.call", async ({ key, method, args }: { key: string; method: string; args: unknown[] }) => {
    const h = open.get(key);
    if (!h) throw new Error(`no session ${key}`);
    if (method === "dispose") {
      open.delete(key);
      return h.dispose();
    }
    if (!METHODS.has(method)) throw new Error(`method not allowed: ${method}`);
    return (h as any)[method](...args);
  });

  for (const [group, ops] of [
    ["sessions", backend.sessions],
    ["fs", backend.fs],
    ["podfs", backend.podfs],
  ] as const)
    for (const [name, fn] of Object.entries(ops)) {
      if (group === "sessions" && name === "watch") continue; // a stream, not a call: below
      peer.handle(`${group}.${name}`, (args: unknown[]) => (fn as any)(...args));
    }

  // One watch per connection, however often the client asks; the list goes out as an event.
  let unwatch: (() => void) | undefined;
  peer.handle("sessions.watch", async () => {
    unwatch?.();
    unwatch = await backend.sessions.watch((list) => peer.emit("sessions", "*", list));
    return true;
  });

  peer.handle("settings.write", (patch) => backend.settings.write(patch));
  peer.handle("space", () => backend.space());
  peer.handle("models.refresh", () => backend.models.refresh());
  peer.handle("auth.providers", () => backend.auth.providers());
  peer.handle("auth.claudeSignedIn", (fresh) => backend.auth.claudeSignedIn(fresh));
  peer.handle("auth.login", ({ lid, provider, type }, signal) =>
    backend.auth.login(provider, type, {
      signal,
      prompt: (prompt) => peer.request<string>("auth.prompt", { lid, prompt }, signal),
      notify: (event) => peer.emit("auth", String(lid), event),
    }),
  );

  return {
    async dispose() {
      unwatch?.();
      await Promise.allSettled([...open.values()].map((h) => h.dispose()));
      open.clear();
    },
  };
}
