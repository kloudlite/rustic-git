import { app, BrowserWindow, ipcMain, nativeTheme } from "electron";
import path from "node:path";
import fs from "node:fs/promises";

function createWindow(): void {
  // HARNESS_SIZE=WxH sizes the window for a screenshot; no effect otherwise.
  const [w, h] = (process.env.HARNESS_SIZE ?? "1360x860").split("x").map(Number);
  const win = new BrowserWindow({
    width: w || 1360,
    height: h || 860,
    minWidth: 900,
    minHeight: 560,
    titleBarStyle: "hiddenInset",
    trafficLightPosition: { x: 12, y: 11 },
    backgroundColor: nativeTheme.shouldUseDarkColors ? "#282c33" : "#fafafa",
    webPreferences: {
      preload: path.join(__dirname, "preload.js"),
      contextIsolation: true,
      nodeIntegration: false,
    },
  });

  void win.loadFile(path.join(__dirname, "renderer", "index.html"), {
    hash: process.env.HARNESS_HASH ?? "",
    search: process.env.HARNESS_THEME ? `theme=${process.env.HARNESS_THEME}` : "",
  });

  // HARNESS_SHOT=<file.png>: write one screenshot after first paint and exit, so
  // the UI can be looked at from a terminal session. No effect otherwise.
  const shot = process.env.HARNESS_SHOT;
  if (shot) {
    win.webContents.once("did-finish-load", () => {
      // HARNESS_THEME forces the OS preference for the capture, so both themes can
      // be shot without touching the person's stored choice.
      setTimeout(async () => {
        await fs.writeFile(shot, (await win.webContents.capturePage()).toPNG());
        app.quit();
      }, 1000);
    });
  }
}

// A service opens in a second window of this app rather than the person's
// browser: the environment's pages belong inside the harness. The window is
// sandboxed and carries no preload, so the page gets nothing of the harness.
const previews = new Map<string, BrowserWindow>();

ipcMain.handle("open-preview", (_e, url: unknown, label: unknown) => {
  if (typeof url !== "string" || !/^https?:\/\//.test(url)) throw new Error("only http(s) urls open");
  const existing = previews.get(url);
  if (existing && !existing.isDestroyed()) return existing.focus();

  // The title is the address as the environment sees it — `env · api:8080/path` —
  // because that is the name a person types inside the environment, not the
  // public hostname it happens to be reachable on.
  const origin = new URL(url).origin;
  const service = typeof label === "string" ? label : new URL(url).host;
  const asSeen = (u: string) => (u.startsWith(origin) ? service + u.slice(origin.length) : u).replace(/\/$/, "");

  const win = new BrowserWindow({
    width: 1100,
    height: 760,
    title: asSeen(url),
    backgroundColor: nativeTheme.shouldUseDarkColors ? "#282c33" : "#fafafa",
    webPreferences: { contextIsolation: true, nodeIntegration: false, sandbox: true },
  });
  win.setMenuBarVisibility(false);
  win.webContents.setWindowOpenHandler(({ url: u }) => {
    void win.loadURL(u);
    return { action: "deny" };
  });
  win.on("page-title-updated", (e) => e.preventDefault());
  const retitle = () => win.setTitle(asSeen(win.webContents.getURL()));
  win.webContents.on("did-navigate", retitle);
  win.webContents.on("did-navigate-in-page", retitle);
  void win.loadURL(url);

  previews.set(url, win);
  win.on("closed", () => previews.delete(url));
});

ipcMain.handle("set-theme", (_e, mode: unknown) => {
  if (mode !== "system" && mode !== "light" && mode !== "dark") throw new Error("unknown theme");
  nativeTheme.themeSource = mode;
});

void app.whenReady().then(() => {
  createWindow();
  app.on("activate", () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});
