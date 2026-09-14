import type { Credential } from "./device";

/**
 * What the window shows, decided here and nowhere else. Every effect (keychain, network,
 * browser, tunnel) is injected so the transitions run under node --test; main.ts supplies the
 * real ones. The state never carries the token — it is what crosses to the renderer.
 */
export type AuthState =
  | { phase: "starting" }
  | { phase: "signed-out"; reason?: string }
  | { phase: "waiting"; code: string; url: string }
  | { phase: "connecting"; step: string }
  | { phase: "ready"; username: string }
  | { phase: "error"; message: string; retry: "launch" | "connect" | "none" };

export type Deps = {
  api(): string;
  store: { load(): Credential | undefined; save(c: Credential): void; clear(): void };
  startLogin(api: string, signal: AbortSignal): Promise<{ code: string; url: string; done: Promise<Credential> }>;
  openExternal(url: string): Promise<void>;
  validate(c: Credential): Promise<"ok" | "expired">;
  connect(c: Credential, step: (s: string) => void): Promise<() => void>;
  revoke(c: Credential): Promise<void>;
  emit(s: AuthState): void;
};

const EXPIRED = "signed out: expired or revoked";
const msg = (e: unknown) => (e instanceof Error ? e.message : String(e));
const named = (e: unknown, name: string) => e instanceof Error && e.name === name;

export function createAuth(d: Deps) {
  let state: AuthState = { phase: "starting" };
  let pending: AbortController | undefined;
  let disconnect: (() => void) | undefined;
  const set = (s: AuthState) => {
    state = s;
    d.emit(s);
  };
  const drop = () => {
    disconnect?.();
    disconnect = undefined;
  };

  const connect = async (c: Credential) => {
    set({ phase: "connecting", step: "connecting to your bench" });
    try {
      disconnect = await d.connect(c, (step) => set({ phase: "connecting", step }));
      set({ phase: "ready", username: c.username });
    } catch (e) {
      if (named(e, "Expired")) return expired();
      set({ phase: "error", message: msg(e), retry: "connect" });
    }
  };

  const launch = async () => {
    let c: Credential | undefined;
    try {
      c = d.store.load();
    } catch (e) {
      return set({ phase: "error", message: msg(e), retry: "none" });
    }
    if (!c) return set({ phase: "signed-out" });
    try {
      if ((await d.validate(c)) === "expired") {
        d.store.clear();
        return set({ phase: "signed-out", reason: EXPIRED });
      }
    } catch (e) {
      // unreachable is not revoked: the login stays for the retry
      return set({ phase: "error", message: msg(e), retry: "launch" });
    }
    await connect(c);
  };

  function expired() {
    drop();
    d.store.clear();
    set({ phase: "signed-out", reason: EXPIRED });
  }

  return {
    state: () => state,
    launch,
    async retry() {
      if (state.phase !== "error") return;
      if (state.retry === "launch") return launch();
      const c = state.retry === "connect" ? d.store.load() : undefined;
      if (c) return connect(c);
    },
    async signIn() {
      if (state.phase === "waiting" || state.phase === "connecting" || state.phase === "ready") return;
      pending = new AbortController();
      const ac = pending;
      try {
        const l = await d.startLogin(d.api(), ac.signal);
        set({ phase: "waiting", code: l.code, url: l.url });
        await d.openExternal(l.url).catch(() => undefined); // the code and URL are on screen either way
        const c = await l.done;
        d.store.save(c);
        await connect(c);
      } catch (e) {
        if (ac.signal.aborted) return set({ phase: "signed-out" });
        if (named(e, "NoKeychain")) return set({ phase: "error", message: msg(e), retry: "none" });
        set({ phase: "signed-out", reason: msg(e) });
      } finally {
        if (pending === ac) pending = undefined;
      }
    },
    cancel() {
      pending?.abort();
    },
    async signOut() {
      const c = (() => {
        try {
          return d.store.load();
        } catch {
          return undefined;
        }
      })();
      if (c) await d.revoke(c);
      d.store.clear();
      drop();
      set({ phase: "signed-out" });
    },
    expired,
  };
}
