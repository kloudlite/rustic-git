import { contextBridge, ipcRenderer } from "electron";
import type { AuthState } from "./auth/controller";

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
  /** Configured, connected, and the last list and exchanges seen, for a cold offline start. */
  benchState: (): Promise<{ configured: boolean; connected: boolean; sessions: unknown[]; exchanges: unknown[] }> => ipcRenderer.invoke("bench:state"),
  /** Copies this laptop's sessions onto the bench; safe to run again. */
  benchImport: (sessions: { id: string; name: string; seq: number; lastActive?: number; archived?: boolean }[]): Promise<{ added: string[]; files: number }> =>
    ipcRenderer.invoke("bench:import", sessions),

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
    switchTeam: (): Promise<void> => ipcRenderer.invoke("auth:switchTeam"),
    api: (): Promise<string> => ipcRenderer.invoke("auth:api"),
    setApi: (url: string): Promise<void> => ipcRenderer.invoke("auth:setApi", url),
    onState: (fn: (s: AuthState) => void): void => void ipcRenderer.on("auth:state", (_e, s: AuthState) => fn(s)),
  },
  setTheme: (mode: "system" | "light" | "dark"): Promise<void> => ipcRenderer.invoke("set-theme", mode),
};

export type Harness = typeof harness;

contextBridge.exposeInMainWorld("harness", harness);
