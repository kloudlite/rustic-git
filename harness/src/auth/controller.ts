import type { Credential } from "./device";
import type { Team } from "../connect/bench";

/**
 * What the window shows, decided here and nowhere else. Every effect (keychain, network,
 * browser, tunnel) is injected so the transitions run under node --test; main.ts supplies the
 * real ones. The state never carries the token — it is what crosses to the renderer.
 *
 * A bench belongs to a TEAM and the team owns the region (owner decision 2026-09-13), so after
 * sign-in the person picks a team; there is no personal bench here. The choice is remembered
 * (a plain setting) and re-checked against the live team list on every connect.
 */
export type AuthState =
  | { phase: "starting" }
  | { phase: "signed-out"; reason?: string }
  | { phase: "waiting"; code: string; url: string }
  | { phase: "choose-team"; teams: Team[]; reason?: string }
  | { phase: "connecting"; step: string }
  | { phase: "ready"; username: string; team: string }
  | { phase: "error"; message: string; retry: "launch" | "connect" | "none" };

export type Deps = {
  api(): string;
  store: { load(): Credential | undefined; save(c: Credential): void; clear(): void };
  team: { load(): string | undefined; save(slug: string): void; clear(): void };
  startLogin(api: string, signal: AbortSignal): Promise<{ code: string; url: string; done: Promise<Credential> }>;
  openExternal(url: string): Promise<void>;
  validate(c: Credential): Promise<"ok" | "expired">;
  teams(c: Credential): Promise<Team[]>;
  connect(c: Credential, team: string, step: (s: string) => void): Promise<() => void>;
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
  // The last team list read; the title bar's switcher offers exactly these.
  let teams: Team[] = [];
  // Bumped by every connect, sign-out and expiry: a connect whose attempt moved on while it was
  // awaiting closes what it just built instead of flipping the state back to ready.
  let attempt = 0;
  const set = (s: AuthState) => {
    state = s;
    d.emit(s);
  };
  const drop = () => {
    disconnect?.();
    disconnect = undefined;
  };

  const connect = async (c: Credential, team: string) => {
    const mine = ++attempt;
    set({ phase: "connecting", step: `connecting to your bench in ${team}` });
    try {
      const close = await d.connect(c, team, (step) => {
        if (mine === attempt) set({ phase: "connecting", step });
      });
      if (mine !== attempt) return close();
      disconnect = close;
      set({ phase: "ready", username: c.username, team });
    } catch (e) {
      if (mine !== attempt) return;
      if (named(e, "Expired")) return expired();
      set({ phase: "error", message: msg(e), retry: "connect" });
    }
  };

  /** The live team list decides: the remembered team if still usable, a lone usable team, else the picker. */
  const pick = async (c: Credential) => {
    const mine = ++attempt;
    set({ phase: "connecting", step: "finding your teams" });
    try {
      teams = await d.teams(c);
    } catch (e) {
      if (mine !== attempt) return;
      if (named(e, "Expired")) return expired();
      return set({ phase: "error", message: msg(e), retry: "connect" });
    }
    if (mine !== attempt) return;
    const remembered = d.team.load();
    if (remembered) {
      const t = teams.find((x) => x.slug === remembered);
      if (t?.region) return connect(c, t.slug);
      d.team.clear();
      const reason = t ? `${remembered} has no region yet` : `you are no longer in ${remembered}`;
      return set({ phase: "choose-team", teams, reason });
    }
    if (teams.length === 1 && teams[0].region) {
      d.team.save(teams[0].slug);
      return connect(c, teams[0].slug);
    }
    set({ phase: "choose-team", teams });
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
    await pick(c);
  };

  function expired(reason = EXPIRED) {
    attempt++;
    drop();
    d.store.clear();
    set({ phase: "signed-out", reason });
  }

  return {
    state: () => state,
    launch,
    async retry() {
      if (state.phase !== "error") return;
      if (state.retry === "launch") return launch();
      if (state.retry !== "connect") return;
      const c = d.store.load();
      // The stored login is gone (a corrupt file was dropped): nothing left to reconnect with.
      if (!c) return set({ phase: "signed-out" });
      return pick(c);
    },
    teams: () => teams,
    /** Only a team from the last list, and only one with a region: no bench is created elsewhere.
        From the picker, or while ready (the title bar's switcher): the current bench closes first. */
    async chooseTeam(slug: string) {
      if (state.phase !== "choose-team" && state.phase !== "ready") return;
      if (state.phase === "ready" && state.team === slug) return;
      const t = teams.find((x) => x.slug === slug);
      if (!t?.region) return;
      const c = d.store.load();
      if (!c) return set({ phase: "signed-out" });
      drop();
      d.team.save(slug);
      return connect(c, slug);
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
        await pick(c);
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
    /** `reason` stays in the state, so a reloaded window's `status()` still shows it. */
    async signOut(reason?: string) {
      attempt++;
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
      set(reason ? { phase: "signed-out", reason } : { phase: "signed-out" });
    },
    expired,
  };
}
