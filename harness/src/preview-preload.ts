import { contextBridge, ipcRenderer } from "electron";

/** The toolbar of a preview window: what it can ask, and what it is told. */
const preview = {
  pick: (on: boolean): Promise<void> => ipcRenderer.invoke("preview:pick", on),
  clear: (): Promise<void> => ipcRenderer.invoke("preview:clear"),
  nav: (verb: "back" | "forward" | "reload"): Promise<void> => ipcRenderer.invoke("preview:nav", verb),
  onState: (fn: (s: PreviewState) => void): void => void ipcRenderer.on("state", (_e, s: PreviewState) => fn(s)),
};

export type PreviewState = { title: string; picking: boolean; marks: number; copied?: string; canBack: boolean; canForward: boolean };
export type Preview = typeof preview;

contextBridge.exposeInMainWorld("preview", preview);
