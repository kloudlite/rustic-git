import { app, BrowserWindow, Menu, WebContentsView, clipboard, ipcMain, nativeTheme, type WebContents } from "electron";
import path from "node:path";
import fs from "node:fs/promises";
import { BenchClient } from "./bench-client";
import { batchImport, isLaptopRow, safeJsonlName, toItem, type ImportRow } from "./import-payload";

let mainWin: BrowserWindow | undefined;
// The bench is remote: HARNESS_BENCH is the local end of the tunnel to this
// person's harness-bench. Without it there is no bench, and the harness says so.
const BENCH = process.env.HARNESS_BENCH;
let bench: BenchClient | undefined;
const toRenderer = (ev: Record<string, unknown>) => {
  if (mainWin && !mainWin.isDestroyed()) mainWin.webContents.send("pi:event", ev);
};

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
    backgroundColor: nativeTheme.shouldUseDarkColors ? "#1f1f1f" : "#ffffff",
    webPreferences: {
      preload: path.join(__dirname, "preload.js"),
      contextIsolation: true,
      nodeIntegration: false,
    },
  });

  mainWin = win;
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

/** Runs inside a preview page. Plain script, no bundler: it must survive any page. */
const ANNOTATE = String.raw`(() => {
  if (window.__hzSet) return;
  const CUR = "url(\"data:image/svg+xml;utf8," + encodeURIComponent('<svg xmlns="http://www.w3.org/2000/svg" width="20" height="22" viewBox="0 0 20 22"><path d="M3 2l13 9.5-5.6 1.1 3.4 6.3-2.4 1.3-3.4-6.3L3 18.5z" fill="#fff" stroke="#000" stroke-width="1.2" stroke-linejoin="round"/></svg>') + "\") 3 2, crosshair";
  const S = document.createElement("style");
  S.textContent = "#__hzo{position:fixed;pointer-events:none;z-index:2147483646;border:1.5px solid #74ade8;background:#74ade814}.__hzm{outline:1.5px solid #74ade8!important;outline-offset:1px}html.__hzp,html.__hzp *{cursor:" + CUR + "!important}";
  document.documentElement.appendChild(S);
  const ol = document.createElement("div"); ol.id = "__hzo"; ol.style.display = "none"; document.documentElement.appendChild(ol);
  let on = false, cur = null;
  window.__hzSet = (v) => { on = v; document.documentElement.classList.toggle("__hzp", v); if (!v) ol.style.display = "none"; };
  window.__hzClear = () => document.querySelectorAll(".__hzm").forEach((e) => e.classList.remove("__hzm"));
  const sel = (el) => {
    const parts = [];
    for (let e = el; e && e !== document.body && parts.length < 3; e = e.parentElement) {
      if (e.id && !e.id.startsWith("__hz")) { parts.unshift("#" + e.id); break; }
      let p = e.tagName.toLowerCase();
      const cls = [...e.classList].filter((c) => !c.startsWith("__hz")).slice(0, 1);
      if (cls.length) p += "." + cls[0];
      const sib = e.parentElement ? [...e.parentElement.children].filter((x) => x.tagName === e.tagName) : [];
      if (sib.length > 1) p += ":nth-of-type(" + (sib.indexOf(e) + 1) + ")";
      parts.unshift(p);
    }
    return parts.join(">");
  };
  const block = (t) => { let e = t; while (e && e !== document.body && getComputedStyle(e).display.startsWith("inline")) e = e.parentElement; return e; };
  document.addEventListener("mousemove", (ev) => {
    if (!on) return; const el = block(ev.target); if (!el) return; cur = el;
    const r = el.getBoundingClientRect(); Object.assign(ol.style, { display: "block", left: r.left + "px", top: r.top + "px", width: r.width + "px", height: r.height + "px" });
  }, true);
  document.addEventListener("click", (ev) => {
    if (!on || !cur) return; ev.preventDefault(); ev.stopPropagation();
    cur.classList.add("__hzm");
    console.log("harness:ref " + location.pathname + " " + sel(cur));
  }, true);
  document.addEventListener("keydown", (ev) => { if (ev.key === "Escape") window.__hzSet(false); }, true);
})();`;

function samplePage(service: string, url: string): string {
  const dark = nativeTheme.shouldUseDarkColors;
  const c = dark
    ? { bg: "#282c33", panel: "#2f343c", line: "#3b414b", fg: "#dce0e5", muted: "#9aa2ad", accent: "#74ade8", ok: "#a1c181" }
    : { bg: "#fafafa", panel: "#ffffff", line: "#e4e4e8", fg: "#383a42", muted: "#74767e", accent: "#3f5bd8", ok: "#50a14f" };
  const [name, port] = service.split(":");
  const rows = [
    ["GET", "/healthz", "200", "1 ms"], ["GET", "/v1/workspaces", "200", "14 ms"], ["POST", "/v1/workspaces/ws-51480ba5/push", "202", "38 ms"],
    ["GET", "/v1/environments/env-2f9a11", "200", "9 ms"], ["GET", "/metrics", "200", "2 ms"],
  ].map(([m, p, s, t]) => `<tr><td class="m">${m}</td><td>${p}</td><td class="s">${s}</td><td class="t">${t}</td></tr>`).join("");
  const html = `<!doctype html><meta charset="utf-8"><title>${service}</title><style>
    body{margin:0;background:${c.bg};color:${c.fg};font:14px/1.5 -apple-system,"IBM Plex Sans",system-ui,sans-serif}
    header{display:flex;align-items:center;gap:12px;padding:14px 24px;border-bottom:1px solid ${c.line};background:${c.panel}}
    .dot{width:8px;height:8px;border-radius:50%;background:${c.ok}} h1{font-size:15px;font-weight:500;margin:0}
    .url{margin-left:auto;font:12px ui-monospace,Menlo,monospace;color:${c.muted}}
    main{padding:24px;max-width:860px} h2{font-size:11px;letter-spacing:.08em;text-transform:uppercase;color:${c.muted};margin:24px 0 8px}
    .grid{display:grid;grid-template-columns:repeat(4,1fr);gap:1px;background:${c.line};border:1px solid ${c.line};border-radius:6px;overflow:hidden}
    .grid div{background:${c.panel};padding:12px 14px} .grid b{display:block;font-size:20px;font-weight:500} .grid span{font-size:12px;color:${c.muted}}
    table{width:100%;border-collapse:collapse;font:13px ui-monospace,Menlo,monospace} td{padding:7px 10px;border-top:1px solid ${c.line}}
    td.m{color:${c.accent};width:60px} td.s{color:${c.ok};width:50px;text-align:right} td.t{color:${c.muted};width:70px;text-align:right}
    p{color:${c.muted};font-size:13px}
  </style>
  <header><span class="dot"></span><h1>${name}</h1><span style="color:${c.muted}">listening on :${port}</span><span class="url">${url}</span></header>
  <main><p>Sample page — the harness is not connected to an environment yet. This is what opens when a port is clicked.</p>
  <h2>Now</h2><div class="grid"><div><b>up</b><span>2h 14m</span></div><div><b>412</b><span>requests / min</span></div><div><b>11 ms</b><span>p50</span></div><div><b>0</b><span>5xx</span></div></div>
  <h2>Recent requests</h2><table>${rows}</table></main>`;
  return "data:text/html;charset=utf-8," + encodeURIComponent(html);
}

const BAR = 38;

/** Which preview window a toolbar or a page belongs to, for the IPC below. */
type PreviewWin = { win: BrowserWindow; bar: WebContentsView; page: WebContentsView; asSeen: (u: string) => string; picking: boolean; marks: number };
const byContents = new Map<number, PreviewWin>();

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

  // Two views in one window: the harness's own toolbar in the title bar, and
  // the page under it. The page is sandboxed with no preload — nothing of the
  // harness reaches it — so the annotate tool is injected as a plain script
  // and reports back over the one channel every page has, a console line.
  const win = new BrowserWindow({
    width: 1100,
    height: 760,
    title: asSeen(url),
    titleBarStyle: "hiddenInset",
    trafficLightPosition: { x: 12, y: 12 },
    backgroundColor: nativeTheme.shouldUseDarkColors ? "#1f1f1f" : "#ffffff",
    webPreferences: { sandbox: true },
  });
  win.setMenuBarVisibility(false);
  const bar = new WebContentsView({ webPreferences: { preload: path.join(__dirname, "preview-preload.js"), contextIsolation: true, nodeIntegration: false } });
  const page = new WebContentsView({ webPreferences: { contextIsolation: true, nodeIntegration: false, sandbox: true } });
  win.contentView.addChildView(bar);
  win.contentView.addChildView(page);
  const layout = () => {
    const [w, h] = win.getContentSize();
    bar.setBounds({ x: 0, y: 0, width: w, height: BAR });
    page.setBounds({ x: 0, y: BAR, width: w, height: h - BAR });
  };
  layout();
  win.on("resize", layout);

  const pw: PreviewWin = { win, bar, page, asSeen, picking: false, marks: 0 };
  byContents.set(bar.webContents.id, pw);
  byContents.set(page.webContents.id, pw);

  page.webContents.setWindowOpenHandler(({ url: u }) => {
    void page.webContents.loadURL(u);
    return { action: "deny" };
  });
  const retitle = () => {
    win.setTitle(asSeen(page.webContents.getURL()));
    tell(pw);
  };
  page.webContents.on("did-navigate", retitle);
  page.webContents.on("did-navigate-in-page", retitle);
  page.webContents.on("did-finish-load", () => {
    pw.picking = false;
    pw.marks = 0;
    void page.webContents.executeJavaScript(ANNOTATE, true);
    tell(pw);
  });
  page.webContents.on("console-message", (_e, _level, line) => {
    if (!line.startsWith("harness:ref ")) return;
    const ref = `@${asSeen(page.webContents.getURL())} ${line.slice("harness:ref ".length)}`;
    clipboard.writeText(ref);
    pw.marks++;
    tell(pw, ref);
  });

  void bar.webContents.loadFile(path.join(__dirname, "renderer", "preview.html"));
  // Until the harness is wired to a real environment, a fixture host answers
  // with a sample page of its own, so opening a port shows something rather
  // than a resolver error: the service, the port, and a request log.
  const sample = /\.khost\.dev$/.test(new URL(url).host);
  void page.webContents.loadURL(sample ? samplePage(service, url) : url);

  previews.set(url, win);
  win.on("closed", () => {
    previews.delete(url);
    byContents.delete(bar.webContents.id);
    byContents.delete(page.webContents.id);
  });
});

/** The toolbar is told everything it shows; it keeps no state of its own. */
function tell(pw: PreviewWin, copied?: string) {
  const h = pw.page.webContents.navigationHistory;
  pw.bar.webContents.send("state", {
    title: pw.asSeen(pw.page.webContents.getURL()),
    picking: pw.picking,
    marks: pw.marks,
    copied,
    canBack: h.canGoBack(),
    canForward: h.canGoForward(),
  });
}

const owner = (wc: WebContents) => byContents.get(wc.id);
ipcMain.handle("preview:pick", (e, on: unknown) => {
  const pw = owner(e.sender);
  if (!pw) return;
  pw.picking = on === true;
  void pw.page.webContents.executeJavaScript(`window.__hzSet && window.__hzSet(${pw.picking})`, true);
  tell(pw);
});
ipcMain.handle("preview:clear", (e) => {
  const pw = owner(e.sender);
  if (!pw) return;
  pw.marks = 0;
  void pw.page.webContents.executeJavaScript("window.__hzClear && window.__hzClear()", true);
  tell(pw);
});
ipcMain.handle("preview:nav", (e, verb: unknown) => {
  const pw = owner(e.sender);
  if (!pw) return;
  const h = pw.page.webContents.navigationHistory;
  if (verb === "back" && h.canGoBack()) h.goBack();
  else if (verb === "forward" && h.canGoForward()) h.goForward();
  else if (verb === "reload") pw.page.webContents.reload();
});

// Session ids as the bench checks them; only the bench's own ids are forwarded.
// A DNS label after the prefix, as workspace and ephemeral ids are.
const SESSION = /^(bench|btw-\d+|[swe]-[a-z0-9]([a-z0-9-]*[a-z0-9])?)$/;
const needBench = () => {
  if (!bench) throw new Error("no bench: set HARNESS_BENCH to the bench's address");
  return bench;
};
ipcMain.handle("pi", async (_e, cmd: unknown, id: unknown) => {
  if (!cmd || typeof cmd !== "object" || typeof (cmd as { type?: unknown }).type !== "string") throw new Error("a pi command has a type");
  const sid = id === undefined ? "bench" : id;
  if (typeof sid !== "string" || !SESSION.test(sid)) throw new Error("not a session id");
  return needBench().rpc(sid, cmd as Record<string, unknown>);
});
// The bench's own surface, method + path allow-listed: the renderer never
// reaches anything else through this.
const SID = "(bench|btw-\\d+|[swe]-[a-z0-9]([a-z0-9-]*[a-z0-9])?)";
const BENCH_ROUTES = new RegExp(
  `^(GET|POST) /sessions$|^POST /sessions/${SID}/(archive|restore|btw)$|^DELETE /sessions/${SID}$|^GET /sessions/${SID}/btw$|^GET /(tasks|procs|healthz)$|^GET /exchanges\\?(session|workspace)=[\\w.-]+$|^POST /import$`,
);
ipcMain.handle("bench", async (_e, method: unknown, p: unknown, body: unknown) => {
  if (typeof method !== "string" || typeof p !== "string" || !BENCH_ROUTES.test(`${method} ${p}`)) throw new Error(`not a bench route: ${String(method)} ${String(p)}`);
  return needBench().rest(method, p, body);
});
ipcMain.handle("bench:messages", (_e, id: unknown) => {
  if (typeof id !== "string" || !SESSION.test(id)) throw new Error("not a session id");
  return needBench().messages(id);
});
ipcMain.handle("bench:state", () => ({ configured: !!bench, connected: bench?.connected() ?? false, ...(bench?.cached() ?? { sessions: [], exchanges: [] }) }));
// `bench import`: the laptop's sessions onto the bench, once. The list is the
// renderer's localStorage (only it can read it); the files are what the old
// memos point at plus every other session file beside them. The bench merges
// by id and skips file names it has, so running it again changes nothing.
ipcMain.handle("bench:import", async (_e, rows: unknown) => {
  if (!Array.isArray(rows)) throw new Error("import takes the session list");
  const ud = app.getPath("userData");
  const memoFile = (id: string) => (id === "bench" ? path.join(ud, "last-session") : path.join(ud, "sessions", id));
  const readMemo = async (id: string) => (await fs.readFile(memoFile(id), "utf8").catch(() => "")).trim();
  const items = [];
  const dirs = new Set<string>();
  for (const r of rows as ImportRow[]) {
    if (!r || typeof r.id !== "string" || !isLaptopRow(r.id)) continue;
    const file = await readMemo(r.id);
    const base = file && safeJsonlName(path.basename(file));
    const content = base ? await fs.readFile(file, "utf8").catch(() => undefined) : undefined;
    if (file) dirs.add(path.dirname(file));
    items.push(toItem(r, base || `${r.id}.jsonl`, content));
  }
  const named = new Set(items.map((i) => i.name));
  const loose = [];
  for (const d of dirs) {
    for (const f of await fs.readdir(d).catch(() => [] as string[])) {
      const base = safeJsonlName(f);
      if (!base || named.has(base)) continue;
      loose.push({ name: base, content: await fs.readFile(path.join(d, f), "utf8") });
    }
  }
  let added: string[] = [];
  let files = 0;
  for (const batch of batchImport(items, loose)) {
    const r = await needBench().rest<{ added: string[]; files: number }>("POST", "/import", batch);
    added = added.concat(r.added);
    files += r.files;
  }
  return { added, files };
});
app.on("before-quit", () => bench?.close());

ipcMain.handle("set-theme", (_e, mode: unknown) => {
  if (mode !== "system" && mode !== "light" && mode !== "dark") throw new Error("unknown theme");
  nativeTheme.themeSource = mode;
});

void app.whenReady().then(() => {
  // The standard menus, explicitly: on macOS ⌘C/⌘V/⌘X/⌘A reach a web page
  // only through Edit-menu roles, and a pasted image is a paste event first.
  Menu.setApplicationMenu(Menu.buildFromTemplate([{ role: "appMenu" }, { role: "editMenu" }, { role: "viewMenu" }, { role: "windowMenu" }]));
  if (BENCH) {
    bench = new BenchClient(BENCH, toRenderer, path.join(app.getPath("userData"), "bench-cache.json"));
    bench.start();
  }
  createWindow();
  app.on("activate", () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});
