import { BrowserWindow, Menu, WebContentsView, app, clipboard, ipcMain, nativeTheme, safeStorage, shell, systemPreferences, type WebContents } from "electron";
import os from "node:os";
import path from "node:path";
import fs from "node:fs/promises";
import { existsSync, readFileSync, writeFileSync, rmSync } from "node:fs";
import { BenchClient } from "./bench-client";
import { batchImport, isLaptopRow, safeJsonlName, toItem, type ImportRow } from "./import-payload";
import { createStore } from "./auth/store";
import { claim, isAuthorizeUrl, startLogin, type Credential } from "./auth/device";
import { createAuth, type AuthState, type Deps } from "./auth/controller";
import { ensureBench, keepToolToken, listTeams, mintSession, mintToolToken, revokeLogin } from "./connect/bench";
import { openTunnel } from "./connect/tunnel";
import { clearMyEnvironment, getEnvironment, listEnvironments, listRepos, listWorkspaces, myEnvironment, setMyEnvironment, volumeHistory } from "./connect/platform";
import { checkPty, checkWatch, closeSocket, readTtydFrame } from "./pty-ipc";
import type WebSocket from "ws";

if (process.env.KL_BOOT_TEST) {
  app.setPath("userData", process.env.KL_BOOT_TEST_PROFILE ?? path.join(os.tmpdir(), `kloudlite-boot-${process.pid}`));
}

// One app, one login, one tunnel: a second launch focuses the first instead. `exit`, not
// `quit`: quit is asynchronous and whenReady below would still open a window first.
if (!app.requestSingleInstanceLock()) app.exit(0);

/**
 * A throw nothing caught is Electron's own "A JavaScript error occurred in the main process"
 * dialog, and the person loses the window to a socket that closed a moment early (owner,
 * 2026-09-18). The app keeps running and the stack goes to stderr, where the log already is: a
 * background socket failing is not a reason to take the desktop down with it.
 */
process.on("uncaughtException", (e: Error) => console.error("uncaught in main:", e?.stack ?? e));
process.on("unhandledRejection", (e: unknown) => console.error("unhandled rejection in main:", (e as Error)?.stack ?? e));

let mainWin: BrowserWindow | undefined;
// Login is always required. HARNESS_BENCH is a developer override for WHERE the bench is
// (a hand-started tunnel or a local harness-bench); without it the app finds the person's own.
const BENCH = process.env.HARNESS_BENCH;
let bench: BenchClient | undefined;
const toRenderer = (ev: Record<string, unknown>) => {
  if (mainWin && !mainWin.isDestroyed()) mainWin.webContents.send("pi:event", ev);
};

// The API base: a build-time default, overridable from the login screen while signed out.
const DEFAULT_API = "https://dev.kloudlite.io";
const apiFile = () => path.join(app.getPath("userData"), "api.txt");
const apiBase = () => {
  try {
    return readFileSync(apiFile(), "utf8").trim() || DEFAULT_API;
  } catch {
    return DEFAULT_API;
  }
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
  void win.loadFile(path.join(__dirname, "..", "renderer", "index.html"), {
    hash: process.env.HARNESS_HASH ?? (process.env.KL_BOOT_TEST ? "boot-session" : ""),
    search: [process.env.HARNESS_THEME && `theme=${process.env.HARNESS_THEME}`, process.env.KL_BOOT_TEST && "boot-test=1"].filter(Boolean).join("&"),
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
  if (!bench) throw new Error("not connected to your bench yet");
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
export const BENCH_ROUTES = new RegExp(
  `^(GET|POST) /sessions$|^POST /sessions/${SID}/(archive|restore|btw|model)$|^DELETE /sessions/${SID}$|^GET /sessions/${SID}/(btw|tools)$|^GET /(tasks|procs|proposals|plans|healthz|models|defaults)$|^GET /bootstrap(\\?session=${SID}(&tail=\\d+)?)?$|^GET /procs/[\\w.-]+/output(\\?since=\\d+(&sinceErr=\\d+)?)?$|^POST /proposals/[\\w.-]+$|^POST /procs/[\\w.-]+/stop$|^POST /tasks/[\\w.-]+/cancel$|^GET /exchanges\\?(session|workspace)=[\\w.-]+$|^GET /memory$|^DELETE /memory/[\\w.-]+$|^GET /memory/[\\w.-]+$|^GET /fs/[\\w.-]+(\\?[^\\s]*)?$|^POST /import$|^POST /workspaces/[a-z0-9-]+(/eph/[a-z0-9-]+)?/session$`,
);
ipcMain.handle("bench", async (_e, method: unknown, p: unknown, body: unknown) => {
  if (typeof method !== "string" || typeof p !== "string" || !BENCH_ROUTES.test(`${method} ${p}`)) throw new Error(`not a bench route: ${String(method)} ${String(p)}`);
  return needBench().rest(method, p, body);
});
ipcMain.handle("bench:messages", (_e, id: unknown) => {
  if (typeof id !== "string" || !SESSION.test(id)) throw new Error("not a session id");
  return needBench().messages(id);
});
ipcMain.handle("bench:bootstrap", (_e, session: unknown) => {
  const q = typeof session === "string" && SESSION.test(session) ? `?session=${encodeURIComponent(session)}` : "";
  return needBench().rest("GET", `/bootstrap${q}`);
});
ipcMain.handle("bench:state", () => ({ configured: !!bench, connected: bench?.connected() ?? false, ...(bench?.cached() ?? { sessions: [], exchanges: [] }) }));

const OPERATION_ID = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/;
const operationId = (value: unknown, what = "operation") => {
  if (typeof value !== "string" || !OPERATION_ID.test(value)) throw new Error(`not an ${what} id`);
  return value;
};
const operationRevision = (value: unknown) => {
  if (!Number.isSafeInteger(value) || (value as number) < 1) throw new Error("not an operation revision");
  return value as number;
};
const exactObject = (value: unknown, keys: string[]) => {
  if (!value || typeof value !== "object" || Array.isArray(value) || Object.keys(value).some((key) => !keys.includes(key))) throw new Error("invalid operation request");
  return value as Record<string, unknown>;
};

async function operation<T>(call: (client: BenchClient, headers: { authorization: string; "x-kl-owner": string; "x-kl-login": string }) => Promise<T>): Promise<T> {
  const s = auth.state();
  const c = s.phase === "ready" ? store.load() : undefined;
  if (s.phase !== "ready" || !c) throw new Error("not signed in");
  try {
    return await call(needBench(), { authorization: `Bearer ${c.token}`, "x-kl-owner": s.team, "x-kl-login": c.username });
  } catch (e) {
    if (e instanceof Error && e.name === "Expired" && !(await stillValid(c))) auth.expired();
    throw e;
  }
}

ipcMain.handle("operations:snapshot", (_e, id: unknown) => operation((client, headers) => client.operationSnapshot(operationId(id), headers)));
ipcMain.handle("operations:events", (_e, id: unknown, after: unknown, limit: unknown) => {
  if (after !== undefined && (typeof after !== "string" || !after)) throw new Error("not an operation cursor");
  if (limit !== undefined && (!Number.isSafeInteger(limit) || (limit as number) < 1 || (limit as number) > 200)) throw new Error("not an operation page limit");
  return operation((client, headers) => client.operationEvents(operationId(id), after as string | undefined, limit as number | undefined, headers));
});
ipcMain.handle("operations:cancel", (_e, value: unknown) => {
  const body = exactObject(value, ["operationId", "expectedRevision"]);
  return operation((client, headers) => client.cancelOperation(operationId(body.operationId), operationRevision(body.expectedRevision), headers));
});
ipcMain.handle("operations:decision", (_e, value: unknown) => {
  const body = exactObject(value, ["operationId", "stepId", "decisionId", "expectedRevision", "outcome"]);
  const id = operationId(body.operationId);
  const decisionId = operationId(body.decisionId, "decision");
  const stepId = operationId(body.stepId, "step");
  const expectedRevision = operationRevision(body.expectedRevision);
  if (body.outcome !== "granted" && body.outcome !== "denied") throw new Error("not a decision outcome");
  return operation((client, headers) => client.recordOperationDecision(id, decisionId, { stepId, expectedRevision, outcome: body.outcome }, headers));
});
ipcMain.handle("operations:input", (_e, value: unknown) => {
  const body = exactObject(value, ["operationId", "stepId", "decisionId", "expectedRevision", "inputs"]);
  const inputs = exactObject(body.inputs, ["answer"]);
  if (typeof inputs.answer !== "string" || !inputs.answer.trim()) throw new Error("not operation input");
  return operation((client, headers) => client.provideOperationInput(operationId(body.operationId), operationId(body.decisionId, "decision"), operationRevision(body.expectedRevision), { answer: inputs.answer }, headers));
});
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
app.on("before-quit", () => disconnect());

// One socket per shell, owned here: the renderer names a shell by id and never
// sees the socket. Nothing is queued — a write to a shell that is gone is
// dropped, exactly as typing into a closed terminal is.
const ptys = new Map<string, WebSocket>();
const sizes = new Map<string, { cols: number; rows: number }>();
// What each shell is attached to, kept past the socket: the tab's x has to
// name the tmux session to kill, and by then its socket may already be gone.

// A shell that is still connecting cannot take bytes yet (`ws.send` throws), and one that is
// closing has nobody to give them to: both are "not open", and the open handler already sends
// the size, so nothing typed or resized in that gap needs keeping.
function openPty(id: string): WebSocket | undefined {
  const w = ptys.get(id);
  return w && w.readyState === w.OPEN ? w : undefined;
}

function closePtys() {
  for (const w of ptys.values()) closeSocket(w);
  ptys.clear();
}


ipcMain.handle("pty:open", (e, rawId: unknown, rawScope: unknown, cols: unknown, rows: unknown) => {
  const { id, scope } = checkPty(rawId, rawScope);
  if (typeof cols !== "number" || typeof rows !== "number") throw new Error("a shell opens at a size");
  // One socket per tab id, always: a second `pty:open` for the same id used to leave the first
  // socket's `message` handler attached, so every byte the shell wrote arrived twice and the
  // cursor walked forward on the prompt line.
  const old = ptys.get(id);
  if (old) {
    old.removeAllListeners();
    old.close();
  }
  const w = needBench().pty(scope);
  ptys.set(id, w);
  const send = (...a: unknown[]) => {
    if (!e.sender.isDestroyed()) e.sender.send(a[0] as string, ...a.slice(1));
  };
  let ended = false;
  const exit = (code: number | undefined, error?: string) => {
    if (ended) return;
    ended = true;
    send("pty:exit", id, code, error);
  };
  // The first frame is the size, so the shell never starts at 80x24 and reflows. The LATEST
  // size, not the one the open was asked with: the view measures itself a moment after dialling,
  // and a resize that lands while the socket is still connecting cannot be sent then — xterm
  // never repeats a size that has not changed, so tmux sat at 80x24 (owner, 2026-09-17 04:10 IST).
  sizes.set(id, { cols, rows });
  w.on("open", () => {
    const at = sizes.get(id) ?? { cols, rows };
    w.send(JSON.stringify({ resize: at }));
  });
  /**
   * ttyd's frames, one byte of opcode then the payload (spec §2.3): `0` output, `1` the title,
   * `2` its preferences, which we ignore — the terminal is themed by this app. The bench splices
   * them through unchanged, so this is where they are read.
   *
   * The opcode is read the SAME WAY whichever kind of websocket frame carried it: ttyd 1.7 sends
   * output as a binary frame and the title and preferences as TEXT ones, and a text frame passed
   * through whole printed `1/nix/profile/current/bin/zsh -l (ws)2{ "disableLeaveAlert"…` into the
   * terminal (owner, on the fleet, 2026-09-18). An opcode this app does not know is dropped, never
   * written: whatever ttyd adds next must not land in the person's scrollback.
   */
  w.on("message", (d: Buffer, isBinary: boolean) => {
    const frame = readTtydFrame(d, isBinary);
    if (frame.kind === "data") return send("pty:data", id, new Uint8Array(frame.data));
    if (frame.kind === "title") return send("pty:title", id, frame.title);
    if (frame.kind === "exit") return exit(frame.code, frame.error);
  });
  w.on("error", () => undefined); // close follows
  w.on("close", () => {
    // A socket that closed without saying why took the shell with it.
    exit(undefined, "disconnected");
    if (ptys.get(id) === w) ptys.delete(id);
  });
});
ipcMain.on("pty:write", (_e, id: unknown, data: unknown) => {
  const w = typeof id === "string" ? openPty(id) : undefined;
  // ttyd takes input as a TEXT frame led by `0`; a binary frame is not its protocol.
  if (w && data instanceof Uint8Array) w.send(`0${Buffer.from(data).toString("utf8")}`);
});
ipcMain.on("pty:resize", (_e, id: unknown, cols: unknown, rows: unknown) => {
  if (typeof id !== "string" || typeof cols !== "number" || typeof rows !== "number") return;
  sizes.set(id, { cols, rows });
  const w = openPty(id);
  // `1` then the size, as ttyd names it: columns and rows, not cols and rows.
  if (w) w.send(`1${JSON.stringify({ columns: cols, rows })}`);
});
/**
 * One file-system watch per workspace on show, owned here the way a shell's socket is. The renderer
 * hears events and patches what it already holds, so opening the Files tab never re-reads a tree it
 * read a minute ago (owner, 2026-09-18).
 *
 * A dropped stream is not a lost workspace: it is redialled on the same backoff the bench client
 * uses, and the renderer is told to read everything once when it comes back, since events during
 * the gap are simply gone.
 */
const watches = new Map<string, { w?: WebSocket; timer?: NodeJS.Timeout; backoff: number; closed?: boolean }>();

function closeWatches() {
  for (const [scope] of watches) stopWatch(scope);
}

function stopWatch(scope: string) {
  const held = watches.get(scope);
  if (!held) return;
  held.closed = true;
  clearTimeout(held.timer);
  held.w?.removeAllListeners();
  closeSocket(held.w);
  watches.delete(scope);
}

ipcMain.handle("watch:open", (e, rawScope: unknown) => {
  const scope = checkWatch(rawScope);
  if (watches.has(scope)) return;
  const held: { w?: WebSocket; timer?: NodeJS.Timeout; backoff: number; closed?: boolean } = { backoff: 1_000 };
  watches.set(scope, held);
  const send = (ev: Record<string, unknown>) => {
    if (!e.sender.isDestroyed()) e.sender.send("watch:event", scope, ev);
  };
  const dial = (resync: boolean) => {
    if (held.closed) return;
    let w: WebSocket;
    try {
      w = needBench().watch(scope);
    } catch {
      held.timer = setTimeout(() => dial(true), held.backoff);
      held.backoff = Math.min(held.backoff * 2, 15_000);
      return;
    }
    held.w = w;
    w.on("open", () => {
      held.backoff = 1_000;
      // Whatever happened while there was no stream was never delivered: one full read covers it.
      if (resync) send({ resync: true });
    });
    w.on("message", (d: Buffer) => {
      try {
        send(JSON.parse(d.toString()) as Record<string, unknown>);
      } catch {
        /* a frame this app cannot read is not a reason to drop the watch */
      }
    });
    w.on("error", () => undefined);
    w.on("close", () => {
      if (held.closed) return;
      held.timer = setTimeout(() => dial(true), held.backoff);
      held.backoff = Math.min(held.backoff * 2, 15_000);
    });
  };
  dial(false);
});

ipcMain.on("watch:close", (_e, scope: unknown) => {
  if (typeof scope === "string") stopWatch(scope);
});

ipcMain.on("pty:close", (_e, id: unknown) => {
  if (typeof id !== "string") return;
  ptys.get(id)?.close();
  ptys.delete(id);
  // Closing the socket IS ending the shell: there is nothing behind it to outlive it (spec §2.3).
  sizes.delete(id);
});
/**
 * Whether the SYSTEM asks for less motion. A Chromium renderer inside Electron answers
 * `prefers-reduced-motion: reduce` even with the OS setting off (measured over CDP on the owner's
 * build, 2026-09-17: `{"reduce":true,"noPref":false}`), which froze every spinner to its
 * animations-off face. The OS's own answer is the one that counts.
 */
ipcMain.handle("app:reduced-motion", () => {
  try {
    return systemPreferences.getAnimationSettings().prefersReducedMotion === true;
  } catch {
    return false;
  }
});

ipcMain.handle("set-theme", (_e, mode: unknown) => {
  if (mode !== "system" && mode !== "light" && mode !== "dark") throw new Error("unknown theme");
  nativeTheme.themeSource = mode;
});

/** Tears down whatever Connect built; safe to call when nothing is connected. */
let closeTunnel: (() => void) | undefined;
let stopToolToken: (() => void) | undefined;
function disconnect() {
  closePtys();
  closeWatches();
  stopToolToken?.();
  stopToolToken = undefined;
  bench?.close();
  bench = undefined;
  closeTunnel?.();
  closeTunnel = undefined;
}

let auth: ReturnType<typeof createAuth>;
let deps: Deps;
let wasReady = false;
let revalidateTimer: NodeJS.Timeout | undefined;
const emitAuth = (s: AuthState) => {
  // One re-validation timer, alive only while ready: leaving ready (sign-out, expiry) clears it.
  if (s.phase === "ready") revalidateTimer ??= setInterval(() => void revalidate(), 5 * 60_000);
  else if (revalidateTimer) (clearInterval(revalidateTimer), (revalidateTimer = undefined));
  // A shell only exists while the login does; leaving `ready` reloads the page
  // under it, and a socket nobody can reach is a shell running for nobody.
  if (s.phase !== "ready") closePtys();
  if (!mainWin || mainWin.isDestroyed()) return;
  // Leaving `ready` reloads the window: App registers listeners for the life of the page, so a
  // fresh page is the honest way back to the login screen.
  if (wasReady && s.phase !== "ready") mainWin.webContents.reload();
  wasReady = s.phase === "ready";
  mainWin.webContents.send("auth:state", s);
};

const credentialFile = () => path.join(app.getPath("userData"), "credential.bin");
let store: Deps["store"];
function credentialStore(): Deps["store"] {
  const raw = createStore(credentialFile(), safeStorage);
  return {
    ...raw,
    load() {
      const c = raw.load();
      // A file that exists but does not decrypt or parse is never going to: drop it.
      if (!c && existsSync(credentialFile())) raw.clear();
      return c;
    },
  };
}
// The chosen team: a plain setting, not a secret; the controller re-checks it against the live list.
const teamFile = () => path.join(app.getPath("userData"), "team.txt");
const teamStore: Deps["team"] = {
  load: () => {
    try {
      return readFileSync(teamFile(), "utf8").trim() || undefined;
    } catch {
      return undefined;
    }
  },
  save: (slug) => writeFileSync(teamFile(), slug),
  clear: () => rmSync(teamFile(), { force: true }),
};
function authDeps(): Deps {
  return {
    api: apiBase,
    store,
    team: teamStore,
    teams: (c: Credential) => listTeams(c.api, c.token, AbortSignal.timeout(10_000)),
    startLogin: (api: string, signal: AbortSignal) => startLogin(api, `${os.hostname()} (desktop)`, { signal }),
    openExternal: async (url: string) => {
      if (!isAuthorizeUrl(apiBase(), url)) throw new Error("refusing to open a URL that is not Kloudlite's login page");
      await shell.openExternal(url);
    },
    validate,
    connect: async (c: Credential, team: string, step: (s: string) => void) => {
      const cache = path.join(app.getPath("userData"), "bench-cache.json");
      try {
        if (BENCH) {
          bench = new BenchClient(BENCH, toRenderer, cache, `${c.username}@${BENCH}`);
        } else {
          await ensureBench(c.api, c.token, team, step);
          // Not fatal: without a tool token the bench still works and its tools say to sign in.
          await mintToolToken(c.api, c.token, team).catch((e) => (e.name === "Expired" ? Promise.reject(e) : console.error(`bench tool token: ${e.message}`)));
          // A 401 on a BENCH route says nothing about the login: the tunnel token is single-use,
          // the bench token changes with the pod, and a recreated bench may not know a route yet.
          // These re-mint on their own beat; the login only ends if the identity endpoint agrees.
          stopToolToken = keepToolToken(c.api, c.token, team, () => void benchRefused(c, "tool token"));
          const t = await openTunnel(
            () => mintSession(c.api, c.token, team),
            (e) => (e.name === "Expired" ? void benchRefused(c, (e as { route?: string }).route || "tunnel") : console.error(`bench tunnel: ${e.message}`)),
          );
          closeTunnel = t.close;
          // The nonce stays in this process: handed to the client, never to IPC, disk or a log.
          bench = new BenchClient(t.base, toRenderer, cache, `${c.username}@${c.api}`, t.nonce);
        }
        bench.start();
      } catch (e) {
        disconnect(); // a half-built connection (tunnel up, bench refused) is not left open
        throw e;
      }
      return disconnect;
    },
    revoke: async (c: Credential) => {
      const jti = claim(c.token, "jti");
      if (!jti) return;
      // Stop renewing first, so no beat re-mints between the delete and the revoke.
      stopToolToken?.();
      stopToolToken = undefined;
      // Best effort: a login that cannot be revoked server-side still leaves this disk.
      await revokeLogin(c.api, c.token, teamStore.load(), jti);
    },
    emit: emitAuth,
  };
}

async function validate(c: Credential) {
  let r: Response;
  try {
    r = await fetch(`${c.api}/v1/cli/tokens`, { headers: { authorization: `Bearer ${c.token}` }, redirect: "error", signal: AbortSignal.timeout(10_000) });
  } catch {
    throw new Error("can't reach Kloudlite");
  }
  await r.body?.cancel();
  if (r.status === 401) return "expired" as const;
  if (!r.ok) throw new Error(`Kloudlite answered ${r.status}`);
  return "ok" as const;
}

// A login revoked elsewhere (the web, `kl-connect`) ends here within 5 min or on the next focus.
// Unreachable is not revoked: the credential stays, and the bench connection's own offline
// state is what the person sees. ponytail: no separate retry banner for a failed re-check.
let revalidating = false;
async function revalidate() {
  if (revalidating || auth?.state().phase !== "ready") return;
  revalidating = true;
  try {
    const c = store.load();
    if (!c || (await validate(c)) === "expired") auth.expired();
  } catch (e) {
    console.error(`login re-check: ${e instanceof Error ? e.message : String(e)}`);
  } finally {
    revalidating = false;
  }
}
app.on("browser-window-focus", () => void revalidate());

ipcMain.handle("auth:status", () => auth.state());
ipcMain.handle("auth:signIn", () => auth.signIn());
ipcMain.handle("auth:cancel", () => auth.cancel());
ipcMain.handle("auth:retry", async () => {
  try {
    await auth.retry();
  } catch (e) {
    // The keychain went away between the error and the retry: sign out, and say why.
    await auth.signOut(e instanceof Error ? e.message : String(e));
  }
});
// Reopens only the current attempt's authorize URL; the renderer names nothing.
ipcMain.handle("auth:openBrowser", async () => {
  const s = auth.state();
  if (s.phase === "waiting") await deps.openExternal(s.url);
});
ipcMain.handle("auth:signOut", () => auth.signOut());
ipcMain.handle("auth:chooseTeam", (_e, slug: unknown) => {
  if (typeof slug !== "string") throw new Error("not a team");
  return auth.chooseTeam(slug);
});
// Slug, name and region only: the list the controller last read, never a fresh fetch per call.
ipcMain.handle("auth:teams", () => auth.teams());
/**
 * A 401 is not proof the login is over. The api rolls, and a request in flight across a roll is
 * answered 401 by a pod that has not loaded its keys yet; `/v1/bench/*` answers 401 for a bench
 * being recreated. Signing out on the first one is what signed the owner out every time (four api
 * rolls and five bench recreates in one night).
 *
 * So a 401 asks the identity endpoint ONCE — the same `validate()` the focus re-check uses — and
 * only a 401 THERE ends the login. Anything else is transient: the caller sees the error, the beat
 * carries on, and the credential stays.
 */
/**
 * A bench route refused us. It is re-minted by the caller's own beat, so this only says so in the
 * footer — and checks the login once, quietly, in case the refusal really was a revoked token.
 */
async function benchRefused(c: Credential, route: string) {
  console.error(`auth: 401 from bench ${route}`);
  toRenderer({ type: "status", text: "bench refused the connection; retrying" });
  if (!(await stillValid(c))) auth.expired();
}

let checking: Promise<"ok" | "expired"> | undefined;
async function stillValid(c: Credential): Promise<boolean> {
  // One check at a time: a burst of 401s from one roll must not become a burst of identity calls.
  checking ??= validate(c).finally(() => (checking = undefined));
  try {
    return (await checking) === "ok";
  } catch {
    // Unreachable is not revoked — the same rule the focus re-check follows.
    return true;
  }
}

// The /v1 reads, one fixed route each, scoped to the connected team; ids are validated and encoded
// in connect/platform.
async function platform<T>(read: (api: string, token: string, team: string) => Promise<T>): Promise<T> {
  const s = auth.state();
  const c = s.phase === "ready" ? store.load() : undefined;
  if (s.phase !== "ready" || !c) throw new Error("not signed in");
  try {
    return await read(c.api, c.token, s.team);
  } catch (e) {
    if (e instanceof Error && e.name === "Expired" && !(await stillValid(c))) auth.expired();
    throw e;
  }
}
ipcMain.handle("platform:workspaces", () => platform(listWorkspaces));
// The team IS the owner the repos are listed under.
ipcMain.handle("platform:repos", () => platform(listRepos));
ipcMain.handle("platform:environments", () => platform(listEnvironments));
ipcMain.handle("platform:environment", (_e, id: unknown) => platform((api, token) => getEnvironment(api, token, id as string)));
ipcMain.handle("platform:snapshots", (_e, volume: unknown) => platform((api, token) => volumeHistory(api, token, volume as string)));
ipcMain.handle("platform:myEnvironment", () => platform(myEnvironment));
ipcMain.handle("platform:setMyEnvironment", (_e, id: unknown) => {
  if (typeof id !== "string") throw new Error("not an environment");
  return platform((api, token, team) => setMyEnvironment(api, token, team, id));
});
ipcMain.handle("platform:clearMyEnvironment", () => platform(clearMyEnvironment));
ipcMain.handle("auth:api", () => apiBase());
ipcMain.handle("auth:setApi", (_e, url: unknown) => {
  if (auth.state().phase !== "signed-out") throw new Error("sign out before changing the Kloudlite address");
  if (typeof url !== "string") throw new Error("not an address");
  if (url.trim() === "") return void rmSync(apiFile(), { force: true });
  const u = new URL(url.trim());
  const local = u.protocol === "http:" && (u.hostname === "127.0.0.1" || u.hostname === "localhost");
  if (u.protocol !== "https:" && !local) throw new Error("the Kloudlite address must be https");
  writeFileSync(apiFile(), u.origin);
});

app.on("second-instance", () => {
  if (!mainWin || mainWin.isDestroyed()) return;
  if (mainWin.isMinimized()) mainWin.restore();
  mainWin.focus();
});

void app.whenReady().then(() => {
  // The standard menus, explicitly: on macOS ⌘C/⌘V/⌘X/⌘A reach a web page
  // only through Edit-menu roles, and a pasted image is a paste event first.
  Menu.setApplicationMenu(
    Menu.buildFromTemplate([
      { role: "appMenu" },
      { role: "editMenu" },
      { role: "viewMenu" },
      { role: "windowMenu" },
      { label: "Account", submenu: [{ label: "Sign Out", click: () => void auth.signOut() }] },
    ]),
  );
  store = credentialStore();
  deps = authDeps();
  auth = createAuth(deps);
  createWindow();
  void auth.launch();
  app.on("activate", () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});

// Model provider keys — the desktop's window onto the auth file pi reads in
// the bench. Its own handler rather than the allow-listed generic route, so a
// key travels this one path only; nothing comes back out, the listing carries
// `configured` and never a value (masked or otherwise).
const PROVIDER_ID = /^[a-z0-9-]+$/;
ipcMain.handle("bench:providers", (_e, action: unknown, id?: unknown, apiKey?: unknown) => {
  if (action === "list") return needBench().rest("GET", "/providers");
  if (typeof id !== "string" || !PROVIDER_ID.test(id)) throw new Error("not a provider id");
  if (action === "save") {
    if (typeof apiKey !== "string" || !apiKey.trim()) throw new Error("apiKey required");
    return needBench().rest("PUT", `/providers/${id}`, { apiKey });
  }
  if (action === "remove") return needBench().rest("DELETE", `/providers/${id}`);
  throw new Error(`not a provider action: ${String(action)}`);
});
