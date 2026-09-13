import { contextBridge, ipcRenderer } from "electron";

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
  /** Start (or resume) a pi: a session `s-N` on its own, a `btw-N` forked from a session file; `stopPi` ends one. */
  spawnPi: (id: string, sessionFile?: string): Promise<void> => ipcRenderer.invoke("pi:spawn", id, sessionFile ? { fork: sessionFile } : {}),
  stopPi: (id: string, forget = false): Promise<void> => ipcRenderer.invoke("pi:stop", id, forget),

  /** Keeps the OS chrome (native title bars, dialogs) on the app's own theme. */
  setTheme: (mode: "system" | "light" | "dark"): Promise<void> => ipcRenderer.invoke("set-theme", mode),
};

export type Harness = typeof harness;

contextBridge.exposeInMainWorld("harness", harness);
