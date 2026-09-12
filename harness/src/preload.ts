import { contextBridge, ipcRenderer } from "electron";

/**
 * The only surface the renderer sees. Every call is a request to the main
 * process, which validates it; nothing of Node or Electron crosses this line.
 */
const harness = {
  version: process.versions.electron,

  /** Opens a URL in a preview window of this app, titled `label` plus the path. */
  openPreview: (url: string, label?: string): Promise<void> => ipcRenderer.invoke("open-preview", url, label),

  /** Keeps the OS chrome (native title bars, dialogs) on the app's own theme. */
  setTheme: (mode: "system" | "light" | "dark"): Promise<void> => ipcRenderer.invoke("set-theme", mode),
};

export type Harness = typeof harness;

contextBridge.exposeInMainWorld("harness", harness);
