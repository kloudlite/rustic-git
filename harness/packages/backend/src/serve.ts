//! The bench side of the laptop TUI: sshd runs this as `kl-host` (bench/term/login-shell) and the
//! laptop's kl-tui speaks ./wire over its stdio. stdout carries frames ONLY: anything else that
//! writes there (pi, a library's console.log) is rerouted to stderr, which ssh shows the laptop
//! after the TUI exits. stdin EOF = the laptop went away: dispose every session and exit.
import type { Backend, SessionHandle, ToolSpec } from "./index.ts";
import { Peer } from "./wire.ts";

const METHODS = new Set([
  "prompt", "steer", "followUp", "clearQueue", "abort",
  "setModel", "setThinkingLevel", "setAutoCompactionEnabled",
]);

export function serve(backend: Backend, peer: Peer) {
  const open = new Map<string, SessionHandle>();

  peer.handle("hello", () => backend.hello());

  peer.handle("session.open", async ({ key, tools, ...o }: { key: string; tools: ToolSpec[] } & any) => {
    await open.get(key)?.dispose();
    open.delete(key);
    const h = await backend.session(key, {
      ...o,
      tools: tools.map((t: ToolSpec) => ({ ...t, run: (input: unknown) => peer.request<string>("tool", { key, name: t.name, input }) })),
      permission: (req, signal) => peer.request("permission", { key, req }, signal),
    });
    open.set(key, h);
    h.subscribe((event) => peer.emit("session", key, event));
    return { messages: h.messages, isClaude: h.isClaude };
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
  ] as const)
    for (const [name, fn] of Object.entries(ops)) peer.handle(`${group}.${name}`, (args: unknown[]) => (fn as any)(...args));

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
      await Promise.allSettled([...open.values()].map((h) => h.dispose()));
      open.clear();
    },
  };
}

if (import.meta.main) {
  const out = process.stdout.write.bind(process.stdout);
  const toErr = (...a: unknown[]) => process.stderr.write(a.map(String).join(" ") + "\n");
  console.log = console.info = console.warn = console.debug = toErr;
  process.stdout.write = ((chunk: any, ...rest: any[]) => process.stderr.write(chunk, ...rest)) as any;
  if (process.env.KL_SERVE_TEST_NOISE) console.log("noise");

  const { LocalBackend } = await import("./local.ts");
  const peer = new Peer((line) => void out(line));
  const server = serve(new LocalBackend(), peer);
  for await (const chunk of Bun.stdin.stream()) peer.feed(chunk);
  // EOF: let in-flight handlers reply (bounded 2 s), then tear down.
  for (let t = 0; t < 200 && !peer.idle; t++) await Bun.sleep(10);
  peer.close();
  await server.dispose();
  process.exit(0);
}
