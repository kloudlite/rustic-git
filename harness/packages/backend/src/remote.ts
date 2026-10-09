//! The client side: a `Backend` whose every call is a ./wire request to serve.ts in the bench daemon.
//! Tools the TUI owns and the permission gate stay here — the host calls back for them. ssh's
//! stderr (the bench-proxy's waking progress) is shown until `hello`, then buffered: after that
//! the renderer owns the screen, so the tail is printed once the TUI exits.
import type { Backend, Hello, LoginUi, SessionHandle, SessionOpts } from "./index.ts";
import { Peer, PROTOCOL } from "./wire.ts";

export class RemoteBackend implements Backend {
  #opts = new Map<string, SessionOpts>();
  #subs = new Map<string, Set<(e: any) => void>>();
  #logins = new Map<string, LoginUi>();
  #lid = 0;

  constructor(private readonly peer: Peer) {
    peer.handle("tool", async ({ key, name, input }) => {
      const def = this.#opts.get(key)?.tools.find((t) => t.name === name);
      if (!def) throw new Error(`unknown tool: ${name}`);
      return def.run(input);
    });
    peer.handle("permission", async ({ key, req }, signal) => {
      const o = this.#opts.get(key);
      if (!o) return { block: true, reason: "session closed" };
      return o.permission!(req, signal);
    });
    peer.handle("auth.prompt", async ({ lid, prompt }) => {
      const ui = this.#logins.get(String(lid));
      if (!ui) throw new Error("login closed");
      return ui.prompt(prompt);
    });
    peer.onEvent((ev, key, event) => {
      if (ev === "session") for (const cb of this.#subs.get(key) ?? []) cb(event);
      else if (ev === "auth") this.#logins.get(key)?.notify(event as any);
    });
  }

  hello(): Promise<Hello> {
    return this.peer.request("hello", null);
  }

  async session(key: string, opts: SessionOpts): Promise<SessionHandle> {
    const { tools, permission: _p, ...rest } = opts;
    this.#opts.set(key, opts);
    const subs = new Set<(e: any) => void>();
    this.#subs.set(key, subs);
    const { messages, isClaude, busy } = await this.peer.request<{ messages: any; isClaude: boolean; busy: boolean }>("session.open", {
      key,
      ...rest,
      tools: tools.map(({ name, description, inputSchema }) => ({ name, description, inputSchema })),
    });
    // kept current from the stream: a client that reconnects mid-turn starts out busy
    let running = !!busy;
    subs.add((e) => {
      if (e.type === "agent_start") running = true;
      else if (e.type === "agent_end" || e.type === "session_closed") running = false;
    });
    const call = (method: string) => (...args: unknown[]) =>
      this.peer.request<void>("session.call", { key, method, args });
    return {
      messages,
      isClaude,
      get busy() {
        return running;
      },
      prompt: call("prompt"),
      steer: call("steer"),
      followUp: call("followUp"),
      clearQueue: call("clearQueue"),
      btw: (q: string) => this.peer.request<string>("session.call", { key, method: "btw", args: [q] }),
      abort: call("abort"),
      setModel: call("setModel"),
      setThinkingLevel: call("setThinkingLevel"),
      setAutoCompactionEnabled: call("setAutoCompactionEnabled"),
      dispose: async () => {
        this.#opts.delete(key);
        this.#subs.delete(key);
        await call("dispose")();
      },
      subscribe: (cb) => (subs.add(cb), () => void subs.delete(cb)),
    } as SessionHandle;
  }

  #ops = <T extends Record<string, any>>(group: string, names: (keyof T)[]): T =>
    Object.fromEntries(names.map((n) => [n, (...args: unknown[]) => this.peer.request(`${group}.${String(n)}`, args)])) as T;

  sessions = this.#ops<Backend["sessions"]>("sessions", ["list", "name", "describe", "clear"]);
  fs = this.#ops<Backend["fs"]>("fs", ["isGitRepo", "changes", "fileDiff", "fullFile", "listDir", "grep"]);
  podfs = this.#ops<Backend["podfs"]>("podfs", ["isGitRepo", "changes", "fileDiff", "fullFile", "listDir"]);
  settings = { write: (patch: any) => this.peer.request<void>("settings.write", patch) };
  space = () => this.peer.request<any>("space", null);
  models = { refresh: () => this.peer.request<any>("models.refresh", null) };
  auth = {
    providers: () => this.peer.request<any>("auth.providers", null),
    claudeSignedIn: (fresh?: boolean) => this.peer.request<boolean>("auth.claudeSignedIn", fresh ?? false),
    login: async (provider: string, type: any, ui: LoginUi) => {
      const lid = String(++this.#lid);
      this.#logins.set(lid, ui);
      try {
        await this.peer.request("auth.login", { lid: Number(lid), provider, type }, ui.signal);
      } finally {
        this.#logins.delete(lid);
      }
    },
  };
}

const mismatch = (msg: string) => Object.assign(new Error(msg), { code: 3 });

export async function connect(cmd: string[]) {
  const child = Bun.spawn(cmd, { stdin: "pipe", stdout: "pipe", stderr: "pipe" });
  const peer = new Peer((line) => {
    child.stdin.write(line);
    child.stdin.flush();
  });
  let booted = false;
  let tail = "";
  const exited = child.exited;
  (async () => {
    for await (const chunk of child.stdout) peer.feed(chunk);
    peer.close();
  })();
  (async () => {
    const dec = new TextDecoder();
    for await (const chunk of child.stderr) {
      if (booted) tail = (tail + dec.decode(chunk, { stream: true })).slice(-16384);
      else process.stderr.write(chunk);
    }
  })();
  const backend = new RemoteBackend(peer);
  let hello: Hello;
  try {
    hello = await backend.hello();
  } catch {
    child.kill();
    throw mismatch("the bench does not serve the laptop TUI");
  }
  if (hello.protocol !== PROTOCOL) {
    child.kill();
    throw mismatch(`protocol ${hello.protocol} on the bench, ${PROTOCOL} here`);
  }
  booted = true;
  return { backend, hello, exited, stderr: () => tail };
}
