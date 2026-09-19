import { contextBridge, ipcRenderer } from "electron";
import type { AuthState } from "./auth/controller";
import type { Team } from "./connect/bench";
import type { ApiEnvironment, ApiRepo, ApiSnapshot, ApiWorkspace } from "./connect/platform";

type OperationSnapshot = import("../bench/src/operations/contracts").OperationSnapshot;
type OperationEvent = import("../bench/src/operations/contracts").OperationEvent;
type CancelOperationPayload = { operationId: string; expectedRevision: number };
type DecisionOperationPayload = CancelOperationPayload & { stepId: string; decisionId: string; outcome: "granted" | "denied" };
type InputOperationPayload = CancelOperationPayload & { stepId: string; decisionId: string; inputs: { answer: string } };

/**
 * The only surface the renderer sees. Every call is a request to the main
 * process, which validates it; nothing of Node or Electron crosses this line.
 */
const harness = {
  version: process.versions.electron,

  /** Opens a URL in a preview window of this app, titled `label` plus the path. */
  openPreview: (url: string, label?: string): Promise<void> => ipcRenderer.invoke("open-preview", url, label),

  /** Native find-in-page: the page's own highlights, nothing to render. Empty text clears. */
  find: (text: string, next?: boolean): Promise<void> => ipcRenderer.invoke("find", text, next),

  /** A pi process's RPC: send a command, get its response; events stream separately,
      each stamped with the `pi` it came from. The bench is the default id. */
  pi: (cmd: Record<string, unknown>, id = "bench"): Promise<Record<string, unknown>> => ipcRenderer.invoke("pi", cmd, id),
  onPi: (fn: (ev: Record<string, unknown> & { type: string; pi?: string }) => void): void =>
    void ipcRenderer.on("pi:event", (_e, ev: Record<string, unknown> & { type: string; pi?: string }) => fn(ev)),
  /** The remote bench's own routes (sessions list, archive, delete, btw, exchanges, import). */
  bench: <T = unknown>(method: "GET" | "POST" | "DELETE", path: string, body?: unknown): Promise<T> => ipcRenderer.invoke("bench", method, path, body),
  /** A session's history: the cache first, then whatever the bench has beyond it. */
  benchMessages: (id: string): Promise<unknown[]> => ipcRenderer.invoke("bench:messages", id),
  /** Everything a window needs to open, in one request over the tunnel. */
  benchBootstrap: (session?: string): Promise<Record<string, unknown>> => ipcRenderer.invoke("bench:bootstrap", session),
  /** Configured, connected, and the last list and exchanges seen, for a cold offline start. */
  benchState: (): Promise<{ configured: boolean; connected: boolean; sessions: unknown[]; exchanges: unknown[] }> => ipcRenderer.invoke("bench:state"),
  /** Copies this laptop's sessions onto the bench; safe to run again. */
  benchImport: (sessions: { id: string; name: string; seq: number; lastActive?: number; archived?: boolean }[]): Promise<{ added: string[]; files: number }> =>
    ipcRenderer.invoke("bench:import", sessions),

  /** Model provider keys, as pi stores them in the bench. A key only ever goes
      in: `list` says whether one is configured, never what it is. */
  providers: {
    list: (): Promise<{ id: string; label: string; configured: boolean }[]> => ipcRenderer.invoke("bench:providers", "list"),
    save: (id: string, apiKey: string): Promise<void> => ipcRenderer.invoke("bench:providers", "save", id, apiKey),
    remove: (id: string): Promise<void> => ipcRenderer.invoke("bench:providers", "remove", id),
  },

  operations: {
    snapshot: (operationId: string): Promise<OperationSnapshot> => ipcRenderer.invoke("operations:snapshot", operationId),
    events: (operationId: string, after?: string, limit?: number): Promise<{ events: OperationEvent[]; nextCursor?: string; hasMore: boolean }> =>
      ipcRenderer.invoke("operations:events", operationId, after, limit),
    cancel: (payload: CancelOperationPayload): Promise<OperationSnapshot> => ipcRenderer.invoke("operations:cancel", payload),
    decision: (payload: DecisionOperationPayload): Promise<void> => ipcRenderer.invoke("operations:decision", payload),
    input: (payload: InputOperationPayload): Promise<OperationSnapshot> => ipcRenderer.invoke("operations:input", payload),
  },

  /** Keeps the OS chrome (native title bars, dialogs) on the app's own theme. */
  /** The desktop login. The renderer sees only the state — the token never leaves main. */
  auth: {
    status: (): Promise<AuthState> => ipcRenderer.invoke("auth:status"),
    signIn: (): Promise<void> => ipcRenderer.invoke("auth:signIn"),
    cancel: (): Promise<void> => ipcRenderer.invoke("auth:cancel"),
    openBrowser: (): Promise<void> => ipcRenderer.invoke("auth:openBrowser"),
    retry: (): Promise<void> => ipcRenderer.invoke("auth:retry"),
    signOut: (): Promise<void> => ipcRenderer.invoke("auth:signOut"),
    chooseTeam: (slug: string): Promise<void> => ipcRenderer.invoke("auth:chooseTeam", slug),
    teams: (): Promise<Team[]> => ipcRenderer.invoke("auth:teams"),
    api: (): Promise<string> => ipcRenderer.invoke("auth:api"),
    setApi: (url: string): Promise<void> => ipcRenderer.invoke("auth:setApi", url),
    onState: (fn: (s: AuthState) => void): void => void ipcRenderer.on("auth:state", (_e, s: AuthState) => fn(s)),
  },
  /** Kloudlite's /v1, read by main for the connected team: one call per read, plain validated JSON back. */
  platform: {
    workspaces: (): Promise<ApiWorkspace[]> => ipcRenderer.invoke("platform:workspaces"),
    environments: (): Promise<ApiEnvironment[]> => ipcRenderer.invoke("platform:environments"),
    repos: (): Promise<ApiRepo[]> => ipcRenderer.invoke("platform:repos"),
    environment: (id: string): Promise<ApiEnvironment> => ipcRenderer.invoke("platform:environment", id),
    snapshots: (volume: string): Promise<ApiSnapshot[]> => ipcRenderer.invoke("platform:snapshots", volume),
    /** The connected team's space: which environment its pods follow, chosen on the platform. */
    myEnvironment: (): Promise<string | undefined> => ipcRenderer.invoke("platform:myEnvironment"),
    setMyEnvironment: (id: string): Promise<void> => ipcRenderer.invoke("platform:setMyEnvironment", id),
    clearMyEnvironment: (): Promise<void> => ipcRenderer.invoke("platform:clearMyEnvironment"),
  },
  /**
   * One shell per id, over the bench tunnel: main owns the socket, the renderer only names it.
   * The far end is the pod's `shell` sidecar running ttyd (spec §2.3) — a socket IS the shell, so
   * there is no session list and no kill: closing it ends it, and a new tab is a new shell.
   * `onData`/`onExit`/`onTitle` return an unsubscribe.
   */
  pty: {
    open: (id: string, scope: string, cols: number, rows: number): Promise<void> => ipcRenderer.invoke("pty:open", id, scope, cols, rows),
    write: (id: string, data: Uint8Array): void => ipcRenderer.send("pty:write", id, data),
    resize: (id: string, cols: number, rows: number): void => ipcRenderer.send("pty:resize", id, cols, rows),
    close: (id: string): void => ipcRenderer.send("pty:close", id),
    onData: (cb: (id: string, data: Uint8Array) => void): (() => void) => {
      const fn = (_e: unknown, id: string, data: Uint8Array) => cb(id, data);
      ipcRenderer.on("pty:data", fn);
      return () => void ipcRenderer.removeListener("pty:data", fn);
    },
    onExit: (cb: (id: string, code: number | undefined, error?: string) => void): (() => void) => {
      const fn = (_e: unknown, id: string, code: number | undefined, error?: string) => cb(id, code, error);
      ipcRenderer.on("pty:exit", fn);
      return () => void ipcRenderer.removeListener("pty:exit", fn);
    },
    /** ttyd's `1` frame: what the shell calls itself, which the tab shows. */
    onTitle: (cb: (id: string, title: string) => void): (() => void) => {
      const fn = (_e: unknown, id: string, title: string) => cb(id, title);
      ipcRenderer.on("pty:title", fn);
      return () => void ipcRenderer.removeListener("pty:title", fn);
    },
  },
  /**
   * A workspace's files, as they change. One stream per workspace, owned by main; the renderer says
   * which workspace it is showing and hears `{path, kind}` (or `{resync:true}` when the far end
   * lost events and everything must be read again).
   */
  watch: {
    open: (scope: string): Promise<void> => ipcRenderer.invoke("watch:open", scope),
    close: (scope: string): void => ipcRenderer.send("watch:close", scope),
    onEvent: (cb: (scope: string, ev: { path?: string; kind?: string; resync?: true }) => void): (() => void) => {
      const fn = (_e: unknown, scope: string, ev: { path?: string; kind?: string; resync?: true }) => cb(scope, ev);
      ipcRenderer.on("watch:event", fn);
      return () => void ipcRenderer.removeListener("watch:event", fn);
    },
  },
  setTheme: (mode: "system" | "light" | "dark"): Promise<void> => ipcRenderer.invoke("set-theme", mode),
  /** Whether the SYSTEM asks for less motion — Chromium's own media query lies about this here. */
  reducedMotion: (): Promise<boolean> => ipcRenderer.invoke("app:reduced-motion"),
};

export type Harness = typeof harness;

contextBridge.exposeInMainWorld("harness", harness);
