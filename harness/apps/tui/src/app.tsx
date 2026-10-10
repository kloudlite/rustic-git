import { useEffect, useMemo, useRef, useState } from "react";
import { RGBA } from "@opentui/core";
import { useKeyboard, useRenderer, useTerminalDimensions } from "@opentui/react";
import type { ToolDef } from "@kloudlite-tui/tools";
import { SessionTitle } from "./components/SessionTitle.tsx";
import { Queue } from "./components/Queue.tsx";
import { Btw, type BtwState } from "./components/Btw.tsx";
import { Transcript, type Entry } from "./components/Transcript.tsx";
import { foldRetries, foldRetry } from "./retry.ts";
import { Prompt } from "./components/Prompt.tsx";
import { Sidebar } from "./components/Sidebar.tsx";
import { Plan } from "./components/Plan.tsx";
import { PlanScreen } from "./components/PlanScreen.tsx";
import { DoneScreen } from "./components/DoneScreen.tsx";
import { doneTasks, planRows } from "./tasks.ts";
import { HintBar } from "./components/HintBar.tsx";
import { Files } from "./components/Files.tsx";
import { Processes, isCtrlJ } from "./components/Processes.tsx";
import { Spinner } from "./components/Spinner.tsx";
import { menuItems, placeholders } from "./slash.ts";
import { fromSpace, wsPath } from "./workspaces.ts";
import { setTheme, theme, themeNames } from "./theme.ts";
import { catalog, findModel, loadProviderAuth, modelLabel, refreshCatalog } from "./models.ts";
import type {
  Ask as BenchAsk,
  BenchEvent,
  ModelRef,
  PermMode,
  SessionEvent,
  SessionHandle,
  SessionMeta,
  LiveSessionMeta,
  ThinkingLevel,
} from "@kloudlite-tui/backend";
import type { SpaceView } from "@kloudlite-tui/backend";
import { backend, hello } from "./hello.ts";
import { applyState, autoAnswer, grant, keepFocus, transcript, upsertById, userRow } from "./sync.ts";

/** pi's reasoning budgets, least to most; a model may support only a prefix. */
const THINKING_LEVELS = [
  "off",
  "minimal",
  "low",
  "medium",
  "high",
  "xhigh",
  "max",
] as const satisfies readonly ThinkingLevel[];

const THINKING_HINT: Record<ThinkingLevel, string> = {
  off: "no reasoning",
  minimal: "~1k tokens",
  low: "~2k tokens",
  medium: "~8k tokens",
  high: "~16k tokens",
  xhigh: "~32k tokens",
  max: "maximum",
};
import { Login, type LoginType } from "./components/Login.tsx";
import { AskPanel, type Ask } from "./components/Ask.tsx";
import { copySelection, readClipboardImage, type ClipImage } from "./clipboard.ts";
import {
  getSession,
  patchSession,
    sessionIdOf,
  sessionKey,
  pickAgentSession,
  askFor,
  baseOf,
  type QueuedMessage,
  type SessionMap,
} from "./sessions.ts";

// opencode's responsive rule (routes/session/index.tsx): one breakpoint at
// 120 columns. The sidebar defaults to 42 either way — wide terminals
// dock it on the right of the row, narrow ones only show it when explicitly
// opened, and then as an absolute overlay above a dimmed transcript.
const SIDEBAR_WIDTH = 42;
/** Resizing stays inside what the layout can honour: the tree needs room to
 *  read, and the transcript must keep more than half the terminal. */
const SIDEBAR_MIN = 28;
const SIDEBAR_MAX = 64;
const SIDEBAR_STEP = 4;
const clampSidebar = (n: number) => Math.min(SIDEBAR_MAX, Math.max(SIDEBAR_MIN, Math.round(n)));

/** "3m ago" / "2h ago" / "5d ago" — session list hint. */
function ago(at: number): string {
  const mins = Math.max(0, Math.round((Date.now() - at) / 60000));
  if (mins < 1) return "just now";
  if (mins < 60) return `${mins}m ago`;
  if (mins < 1440) return `${Math.round(mins / 60)}h ago`;
  return `${Math.round(mins / 1440)}d ago`;
}

/**
 * What the session column shows. The title bar names the current context on
 * the left and lists these on the right — click one (or press f) to open it.
 */
type ViewId = "agent" | "files" | "processes" | "plan" | "done";
const WIDE_COLUMNS = 120;

/**
 * Sessions are hierarchical: each environment has a main session, and each
 * workspace has its own session running pi's full agent harness (streaming,
 * thinking, coding tools, steering queues, compaction, retries). A running
 * turn belongs to its session and keeps streaming while you look elsewhere.
 */
export function App({
  tools: sink,
  onExit,
}: {
  /** Receives the tools the TUI owns (question, env_*, workspace_*); tests read it. */
  tools?: ToolDef[];
  /** Called on quit; defaults to killing the process (single-user CLI). */
  onExit?: () => void;
}) {
  const tuiTools = useRef<ToolDef[]>(sink ?? []).current;
  const renderer = useRenderer();
  const [copied, setCopied] = useState<{ ok: boolean; text: string } | null>(null);
  useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    // opentui emits "selection" on mouse-up after a drag over any selectable <text>
    const onSel = (sel: { getSelectedText(): string } | null) => {
      const text = sel?.getSelectedText() ?? "";
      if (!text.trim()) return;
      const ok = copySelection(text, (t) => renderer.copyToClipboardOSC52(t));
      const n = text.split("\n").length;
      setCopied({ ok, text: ok ? `copied ${n} line${n === 1 ? "" : "s"}` : "copy failed — terminal has no clipboard access" });
      clearTimeout(timer);
      timer = setTimeout(() => setCopied(null), 1500);
    };
    renderer.on("selection", onSel);
    return () => {
      renderer.off("selection", onSel);
      clearTimeout(timer);
    };
  }, [renderer]);
  const { width: columns, height: rows } = useTerminalDimensions();
  const [sessions, setSessions] = useState<SessionMap>({});
  const [input, setInput] = useState("");
  // images pasted with ctrl+v, sent with the next prompt
  const [images, setImages] = useState<ClipImage[]>([]);
  const pasted = useRef(0);

  /**
   * ctrl+v in the prompt: keep the image and hand back the token that stands
   * in for it in the text, so it sits where the caret was (opencode's model —
   * an attachment is a part of the value, not a chip beside it).
   */
  function pasteImage(): string | null {
    const img = readClipboardImage();
    if (!img) return null;
    // a text-only model drops attachments without a word — say so here, while
    // the user can still switch models instead of after the answer comes back
    const model = findModel(getSession(sessions, activeKey).model);
    if (model && !model.input.includes("image")) {
      append(activeKey, {
        kind: "error",
        text: `${model.id} does not accept images — switch models with /model to send one.`,
      });
      return null;
    }
    // counted off a ref: a burst of keys in one stdin chunk runs every
    // handler against the same render, so `images.length` is stale here
    const n = ++pasted.current;
    setImages((i) => [...i, img]);
    return `[Image ${n}] `;
  }
  // vim-style modal keyboard: NORMAL (default) = letter commands, INSERT = typing
  // vim keys are a setting; without them the prompt is always live and the
  // letter commands move to ctrl+<letter> (what the splash already advertises)
  const [keyMode, setKeyMode] = useState<"normal" | "insert">(
    () => (hello().settings.vim ?? "off") === "on" ? "normal" : "insert",
  );
  const [hint, setHint] = useState(0);
  // 0 = main context (orchestrator); 1..N = inside workspaces[focus - 1]
  const [focus, setFocus] = useState(0);
  // The space's real workspaces and environments, polled from the backend (see `refreshSpace`).
  // Workspaces belong to the working session, not to an environment.
  const [space, setSpace] = useState<SpaceView>();
  const mapped = useMemo(() => (space ? fromSpace(space) : { workspaces: [], environments: [], envIndex: 0 }), [space]);
  const user = space?.user ?? "";
  const envs = mapped.environments;
  const env = mapped.envIndex;
  const workspaces = mapped.workspaces;
  // Focus is an index into the list, so a refresh that reorders or drops rows has to move it with
  // the workspace it was on (by id), or clamp when that one is gone.
  const live = useRef({ focus, workspaces });
  live.current = { focus, workspaces };
  // the newest view of state that a long-lived session listener needs (it closes over the render it was made in)
  const latest = useRef({ sessions: {} as SessionMap, activeKey: "" });
  const [palette, setPalette] = useState(false);
  const [view, setView] = useState<ViewId>("agent");
  const [filesRefresh, setFilesRefresh] = useState(0);
  // "/" command overlay (NORMAL mode): filter + pick slash commands top-level
  const [cmdMode, setCmdMode] = useState(false);
  /** Open the command overlay. The draft always carries its leading "/", so
   *  what the user sees matches what they typed and what gets submitted. */
  const openCmd = (prefill = "") => {
    setCmdMode(true);
    setHistIdx(null);
    setInput(`/${prefill}`);
  };
  const [login, setLogin] = useState<{ provider: string; type: LoginType } | null>(null);
  // provider id → auth status, re-resolved after a login
  const [auth, setAuth] = useState<Map<string, { ok: boolean; envKey?: string }>>(new Map());
  // seeded from pi's bundled catalog so the picker paints at once, then
  // replaced by the providers' live lists — never block the TUI on the network
  const [models, setModels] = useState(catalog);
  // Every context — the environment's main sessions and each workspace's —
  // can hold several named sessions: base key → id of the one in use, plus a
  // name cache so the UI can label them.
  const [sessionId, setSessionId] = useState<Record<string, string>>({});

  // index into the active session's queue while editing it, else null
  const [queuePick, setQueuePick] = useState<number | null>(null);
  // the /btw panel: never in a transcript, closed by esc. `n` numbers requests so a late answer for a
  // closed or replaced panel is dropped
  const [btw, setBtw] = useState<(BtwState & { key: string }) | null>(null);
  const btwN = useRef(0);
  // narrow terminals (<= 120 cols): sidebar opened explicitly, as an overlay
  const [sidebarOpen, setSidebarOpen] = useState(false);
  // bumped to re-render after an in-place theme swap
  const [, setThemeTick] = useState(0);
  // persisted UI preferences (sidebar/thinking visibility, vim keys)
  const [prefs, setPrefs] = useState(() => {
    const s = hello().settings;
    return {
      sidebar: s.sidebar ?? "show",
      thinking: s.thinking ?? "show",
      thinkingLevel: s.thinkingLevel ?? "medium",
      autoCompact: s.autoCompact ?? "on",
      codemode: s.codemode ?? "on",
      vim: s.vim ?? "off",
      sidebarWidth: clampSidebar(s.sidebarWidth ?? SIDEBAR_WIDTH),
    };
  });
  // Live agent sessions, one per session key (created lazily on first prompt).
  const agents = useRef(new Map<string, Promise<SessionHandle>>());
  // ↑/↓ recall position in the active session's history; null = live input
  const [histIdx, setHistIdx] = useState<number | null>(null);
  // interactive prompts (permissions, model questions), oldest first
  // a daemon card carries its backend id; the help popup and other local panels have none
  const [asks, setAsks] = useState<Ask[]>(() => hello().asks.map(cardFor));
  const [defaultModel, setDefaultModel] = useState<ModelRef>(() => hello().defaultModel);
  // tools granted "always allow" per session key
  const alwaysAllow = useRef(new Map<string, Set<string>>());
  // Permission mode is per-process and resets on restart: a forgotten "bypass"
  // persisted across launches is the one failure worth not having.
  const [permMode, setPermMode] = useState<PermMode>(() => hello().mode);

  const environment = envs[env]; // undefined until the space has an environment
  const mainBase = "main";
  const mainSession = sessionId[mainBase] ?? "main";
  const mainKey = sessionKey(undefined, mainSession);
  // sessions belong to the context you are in: the environment's, or this
  // workspace's own
  const activeBase = focus === 0 ? "main" : workspaces[focus - 1]!.id;
  // Pushed by the bench when it can (`sessions.watch`): every view's list and busy marks move the
  // moment any view opens, names or runs a session. `undefined` = not answered yet, `null` = an
  // old bench, which keeps today's fetch.
  const [watched, setWatched] = useState<LiveSessionMeta[] | null | undefined>(undefined);
  useEffect(() => {
    let off: (() => void) | undefined;
    let gone = false;
    backend()
      .sessions.watch((l) => !gone && setWatched(l))
      .then((f) => (gone ? f() : (off = f)))
      .catch(() => !gone && setWatched(null));
    return () => {
      gone = true;
      off?.();
    };
  }, []);
  // Everything the daemon says outside a session: cards, mode, settings, logins, files, the space.
  useEffect(() => {
    let off: (() => void) | undefined;
    let gone = false;
    backend()
      .watch(onBench)
      .then((f) => (gone ? f() : (off = f)))
      .catch(() => {});
    return () => {
      gone = true;
      off?.();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  /** A daemon card as a panel: choosing answers the daemon, and the card leaves on `ask_resolved`. */
  function cardFor(a: BenchAsk): Ask {
    const card: Ask = {
      ...a,
      escapeId: a.kind === "permission" ? "reject" : undefined,
      resolve: (choice) => {
        if (choice === "always") grant(alwaysAllow.current, a);
        void backend().asks.answer(a.id, choice === "always" ? "once" : choice).catch(() => {});
      },
    };
    return card;
  }
  function onBench(e: BenchEvent) {
    switch (e.type) {
      case "ask": {
        const auto = autoAnswer(e.ask, alwaysAllow.current);
        if (auto) void backend().asks.answer(e.ask.id, auto).catch(() => {});
        else setAsks((l) => (l.some((a) => a.id === e.ask.id) ? l : [...l, cardFor(e.ask)]));
        break;
      }
      case "ask_resolved":
        setAsks((l) => l.filter((a) => a.id !== e.id));
        break;
      case "perm":
        setPermMode(e.mode);
        break;
      case "settings": {
        // the shared knobs only; vim, theme and the layout stay this person's own per view
        const st = e.settings;
        setPrefs((p) => ({
          ...p,
          thinkingLevel: st.thinkingLevel ?? p.thinkingLevel,
          autoCompact: st.autoCompact ?? p.autoCompact,
          codemode: st.codemode ?? p.codemode,
        }));
        if (st.defaultModel) setDefaultModel(st.defaultModel);
        break;
      }
      case "auth_changed":
        loadProviderAuth().then(setAuth).catch(() => {});
        refreshCatalog().then(setModels).catch(() => {});
        break;
      case "fs_changed": {
        const { focus: f, workspaces: ws } = live.current;
        if (f > 0 && ws[f - 1]?.id === e.ws) setFilesRefresh((n) => n + 1);
        break;
      }
      case "space": {
        const f = keepFocus({ focus: live.current.focus, ids: live.current.workspaces.map((w) => w.id) }, e.view);
        setSpace(e.view);
        setFocus(f);
        break;
      }
    }
  }
  const [fetched, setFetched] = useState<SessionMeta[]>([]);
  useEffect(() => {
    if (watched !== null) return;
    backend().sessions.list(activeBase).then(setFetched).catch(() => {});
  }, [watched, activeBase]);
  // same filter as the bench's listSessions(prefix)
  const baseSessions = watched ? watched.filter((m) => m.key.startsWith(activeBase)) : fetched;
  // a clone has no session of its own, only its subagent's: open that one
  useEffect(() => {
    if (activeBase === "main" || sessionId[activeBase]) return;
    const id = pickAgentSession(activeBase, baseSessions);
    if (id) setSessionId((m) => (m[activeBase] ? m : { ...m, [activeBase]: id }));
  }, [activeBase, baseSessions, sessionId]);
  const activeKey = sessionKey(
    focus === 0 ? undefined : workspaces[focus - 1]!.id,
    sessionId[activeBase] ?? "main",
  );
  const session = getSession(sessions, activeKey);
  latest.current = { sessions, activeKey };
  // the card on screen: only an ask from this view's workspace, so another
  // workspace's permission never blocks typing here
  const shownAsk = askFor(asks, activeKey);
  const busy = session.busy;

  function exit(): void {
    if (onExit) return onExit(); // server session: close the connection only
    renderer.destroy();
    process.exit(0);
  }

  // the file views only exist inside a workspace; leaving one goes back
  useEffect(() => {
    if (focus === 0 && view === "files") setView("agent");
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focus]);

  // Switching sessions leaves history recall; also wake the session so a
  // persisted transcript is restored without needing a first prompt.
  useEffect(() => {
    setHistIdx(null);
    ensureAgent(activeKey).catch(() => {});
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeKey]);

  useEffect(() => {
    loadProviderAuth()
      .then((a) => {
        setAuth(a);
        // First run: no credentials anywhere, so there is no model to send to
        // and an empty /model menu would be the only clue. Say so on arrival —
        // typing /login is not discoverable, and the models list is filtered
        // to connected providers, so this is the way in.
        if (![...a.values()].some((p) => p.ok))
          append(activeKey, {
            kind: "info",
            text: "No AI provider connected yet — run /login to connect one.",
          });
      })
      .catch(() => {});
    refreshCatalog().then(setModels).catch(() => {});
    // mount only: the notice belongs on the session the user arrived in
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const id = setInterval(
      () => setHint((h) => (h + 1) % placeholders.length),
      6000,
    );
    return () => clearInterval(id);
  }, []);

  /**
   * ↑/↓ recall, called by the prompt only when the caret has nowhere left to go
   * inside a multiline value — moving between lines comes first.
   */
  const recall = (dir: -1 | 1) => {
    if (palette || cmdMode) return false;
    // inside queue editing the arrows walk the queue
    if (queuePick !== null) {
      setQueuePick(Math.max(0, Math.min(session.queued.length - 1, queuePick + dir)));
      return true;
    }
    // ↑ on an empty prompt with something queued edits the queue; history recall otherwise
    if (dir === -1 && input === "" && histIdx === null && session.queued.length > 0) {
      setQueuePick(session.queued.length - 1);
      return true;
    }
    const h = session.history;
    if (h.length === 0) return false;
    if (dir === -1) {
      const idx = histIdx === null ? h.length - 1 : Math.max(0, histIdx - 1);
      setHistIdx(idx);
      setInput(h[idx]!);
      return true;
    }
    if (histIdx === null) return false;
    const idx = histIdx + 1;
    setHistIdx(idx >= h.length ? null : idx);
    setInput(idx >= h.length ? "" : h[idx]!);
    return true;
  };

  useKeyboard((key) => {
    if (key.ctrl && key.name === "c") return exit();
    // a full-column view owns the keyboard while it is up
    if (login || shownAsk || filesView || processesView || planView || doneView) return;
    if (btw && btw.key === activeKey && key.name === "escape") {
      btwN.current++; // ponytail: no cancel; a late answer is just dropped
      setBtw(null);
      return;
    }
    if (palette && key.name === "escape") {
      setPalette(false);
      setInput("");
      return;
    }
    if (cmdMode && key.name === "escape") {
      setCmdMode(false);
      setInput("");
      return;
    }
    const menuOpen = palette || cmdMode;
    if (inputLive && !menuOpen) {
    }
    const n = workspaces.length;
    // only your own workspaces can be entered; others are visible but attached
    // to their owner's session
    const own = workspaces
      .map((w, i) => (w.owner === user ? i + 1 : -1))
      .filter((i) => i > 0);
    // cycle through the whole hierarchy: main, then your workspaces
    const ring = [0, ...own];
    const cycle = (delta: number) =>
      setFocus((f) => {
        const pos = Math.max(0, ring.indexOf(f));
        const next = ring[(pos + delta + ring.length) % ring.length]!;
        return next;
      });

    /**
     * The command surface, shared by both key schemes: a bare letter in vim's
     * NORMAL mode, ctrl+<letter> when vim is off (where typing must reach the
     * prompt). Returns true once it has handled the key.
     */
    const command = (name: string, seq?: string): boolean => {
      // queue editing: target the queue, then pick/edit/drop within it
      if (queuePick !== null) {
        const q = session.queued;
        if (name === "j") {
          setQueuePick(Math.min(q.length - 1, queuePick + 1));
          return true;
        }
        if (name === "k") {
          setQueuePick(Math.max(0, queuePick - 1));
          return true;
        }
        if (name === "d") {
          const next = q.filter((_, i) => i !== queuePick);
          rewriteQueue(activeKey, next);
          setQueuePick(next.length === 0 ? null : Math.min(queuePick, next.length - 1));
          return true;
        }
      }
      if (name === "f") {
        cycleView();
        return true;
      }
      if (name === "j") {
        cycle(1);
        return true;
      }
      if (name === "k") {
        cycle(-1);
        return true;
      }
      if (name === "p") {
        setInput("");
        setHistIdx(null);
        setPalette(true);
        return true;
      }
      if (name === "b") {
        setFocus(0);
        return true;
      }
      if (/^[0-9]$/.test(name)) {
        const d = Number(name);
        if (d === 0) setFocus(0);
        else if (d <= n && workspaces[d - 1]!.owner === user) setFocus(d);
        return true;
      }
      if (seq === "[" || seq === "]") {
        // ponytail: opentui gives no drag stream, so the divider cannot be
        // dragged — resize is keyboard-only ([ / ] and /settings width)
        resizeSidebar(seq === "]" ? SIDEBAR_STEP : -SIDEBAR_STEP);
        return true;
      }
      if (seq === "?") {
        openHelp();
        return true;
      }
      return false;
    };

    // ctrl+s steers the live prompt into a running turn. ctrl+enter means the
    // same thing, but only terminals speaking the kitty protocol report the
    // modifier on return — with it off ctrl+enter is an indistinguishable \r,
    // so this is the binding that works everywhere.
    if (key.ctrl && key.name === "s" && !menuOpen && session.busy && input.trim()) {
      submit(input, true);
      return;
    }

    // ^j is the Jobs screen in both key schemes (vim's bare j still walks the workspaces)
    if (isCtrlJ(key) && !menuOpen) return setView((v) => (v === "processes" ? "agent" : "processes"));

    // ^g the whole plan, ^q the finished tasks: both key schemes
    if (key.ctrl && !key.meta && !menuOpen && (key.name === "g" || key.name === "q"))
      return setView((v) => (v === (key.name === "g" ? "plan" : "done") ? "agent" : key.name === "g" ? "plan" : "done"));

    // ---- vim off: ctrl+<letter> commands, everything else types ----
    if (prefs.vim === "off" && key.ctrl && !key.meta && !menuOpen) {
      if (command(key.name)) return;
    }

    // shift+tab cycles the permission mode in both key schemes; plain tab keeps
    // cycling workspaces in NORMAL
    if (key.name === "tab" && key.shift && !menuOpen)
      return void backend().mode.set(PERM_MODES[(PERM_MODES.indexOf(permMode) + 1) % PERM_MODES.length]!).catch(() => {});
    if (key.name === "tab" && !menuOpen && keyMode === "normal") return cycle(1);
    // enter in the queue pulls the picked message back into the prompt
    if (key.name === "return" && queuePick !== null && !menuOpen) {
      const picked = session.queued[queuePick];
      if (!picked) return setQueuePick(null);
      rewriteQueue(activeKey, session.queued.filter((_, i) => i !== queuePick));
      setQueuePick(null);
      setInput(picked.text);
      return setKeyMode("insert");
    }
    if (key.name === "escape") {
      if (queuePick !== null) return setQueuePick(null); // esc leaves the queue
      if (prefs.vim === "off") {
        if (input) {
          setInput(""); // esc abandons the draft
          setHistIdx(null);
          return;
        }
        if (busy) return interrupt(activeKey);
        return;
      }
      if (keyMode === "insert") {
        setInput(""); // esc abandons the draft
        setHistIdx(null);
        return setKeyMode("normal");
      }
      if (busy) return interrupt(activeKey); // esc in NORMAL interrupts
      return;
    }

    // ---- vim NORMAL mode: single letters are commands (no modifiers, tmux-safe) ----
    if (
      prefs.vim === "on" &&
      keyMode === "normal" &&
      !palette &&
      !cmdMode &&
      !key.ctrl &&
      !key.meta &&
      !key.option
    ) {
      if (key.name === "i") return setKeyMode("insert");
      if (key.sequence === "/") return openCmd();
      if (command(key.name, key.sequence)) return;
      return; // unbound NORMAL-mode keys do nothing (never leak into the input)
    }
  });

  /**
   * Replace a session's queue. pi exposes no per-item removal, so the only way
   * to drop or change one is to clear the queue and re-queue what we keep, in
   * order — each onto the queue its kind names.
   * ponytail: a full rewrite per edit; fine for a handful of queued prompts.
   */
  function rewriteQueue(key: string, next: QueuedMessage[]): void {
    agents.current
      .get(key)
      ?.then(async (agent) => {
        await agent.clearQueue();
        for (const m of next) {
          if (m.kind === "steer") await agent.steer(m.text);
          else await agent.followUp(m.text);
        }
      })
      .catch(() => {});
    // reflect it immediately; a queue_update event will confirm
    setSessions((map) => patchSession(map, key, { queued: next }));
  }

  /** Human summary of a tool call, opencode-style: the salient arg, not JSON. */
  function toolSummary(name: string, args: any): string {
    const one = (v: unknown) => String(v ?? "").replace(/\s+/g, " ").trim();
    const clip = (v: string) => (v.length > 100 ? `${v.slice(0, 99)}…` : v);
    if (args && typeof args === "object") {
      if (name === "bash" && args.command) return clip(one(args.command));
      if (name === "exec" && args.cmd) return clip(one(Array.isArray(args.cmd) ? args.cmd.join(" ") : args.cmd));
      // codemode's arg is a whole script — keep its newlines, the block renders them
      if (name === "codemode" && args.code) return String(args.code);
      const path = args.path ?? args.file_path ?? args.filePath;
      if (path) return clip(one(path));
      const vals = Object.values(args).filter((v) => typeof v === "string");
      if (vals.length) return clip(vals.map(one).join(" "));
    }
    return clip(JSON.stringify(args) ?? "");
  }

  /** Strip pi's internal doc paths / stack noise from error text. */
  const cleanError = (text: string) =>
    text
      .split("\n")
      .filter((l) => !/node_modules|^\s*at /.test(l))
      .join("\n")
      .replace(/\s*See:\s*$/m, "")
      .trim();

  const append = (key: string, entry: Entry) =>
    setSessions((map) =>
      patchSession(map, key, (s) => ({ entries: [...s.entries, entry] })),
    );

  /** Insert-or-update an entry by id (streaming text, tool status). */
  const upsert = (key: string, id: string, make: (prev?: Entry) => Entry) =>
    setSessions((map) =>
      patchSession(map, key, (s) => {
        const i = s.entries.findIndex((e) => "id" in e && e.id === id);
        if (i === -1) return { entries: [...s.entries, make()] };
        const entries = [...s.entries];
        entries[i] = make(entries[i]);
        return { entries };
      }),
    );

  function handleAgentEvent(key: string, event: SessionEvent) {
    switch (event.type) {
      case "agent_start":
        setSessions((map) => patchSession(map, key, { busy: true }));
        break;
      case "session_closed":
        // the daemon disposed this agent: the handle is dead (ensureAgent's own listener already
        // dropped the entry). `reopen` means it was rebuilt or cleared and every view must reload
        // the snapshot; one nobody is looking at stays closed until its next prompt.
        setSessions((map) => patchSession(map, key, { busy: false }));
        if (event.reopen && (key === latest.current.activeKey || latest.current.sessions[key]?.entries.length))
          ensureAgent(key).catch(() => {});
        break;
      case "agent_end":
        setSessions((map) => patchSession(map, key, { busy: false }));
        break;
      case "message_start": {
        // the person's prompt arrives from the daemon, so every view shows it
        const row = userRow(event);
        if (row) setSessions((map) => patchSession(map, key, (s) => ({ entries: upsertById(s.entries, row) })));
        break;
      }
      case "message_update":
      case "message_end": {
        const msg = event.message as any;
        if (msg.role !== "assistant") break;
        // Stable per-message entry ids: the message's creation timestamp
        // survives every streaming update, so deltas update in place.
        const mid = `m${msg.timestamp}`;
        const thinking = msg.content
          .filter((b: any) => b.type === "thinking")
          .map((b: any) => b.thinking)
          .join("");
        if (thinking)
          upsert(key, `${mid}t`, () => ({
            kind: "thinking",
            id: `${mid}t`,
            text: thinking,
            // live reasoning stays a one-line ticker; a finished block opens up
            done: event.type === "message_end",
          }));
        const text = msg.content
          .filter((b: any) => b.type === "text")
          .map((b: any) => b.text)
          .join("");
        if (text) upsert(key, mid, () => ({ kind: "agent", id: mid, text }));
        if (event.type !== "message_end") break;
        const m = msg;
        if (m.role === "assistant" && (m.stopReason === "error" || m.stopReason === "aborted")) {
          append(key, {
            kind: "error",
            text: m.stopReason === "aborted" ? "interrupted" : cleanError(m.errorMessage ?? "request failed"),
          });
        }
        break;
      }
      case "tool_execution_start":
        setSessions((map) =>
          patchSession(map, key, (s) => {
            const made: Entry = {
              kind: "tool",
              id: event.toolCallId,
              name: event.toolName,
              summary: toolSummary(event.toolName, event.args),
              status: "running",
              diff: event.diff,
            };
            const i = s.entries.findIndex((e) => "id" in e && e.id === event.toolCallId);
            if (i === -1) return { entries: foldRetry(s.entries, made) }; // a retry replaces its failed row
            const entries = [...s.entries];
            entries[i] = made;
            return { entries };
          }),
        );
        break;
      case "tool_execution_update": {
        // streaming tool output → rendered as the bash block's tail
        const partial = (event as any).partialResult;
        const text =
          typeof partial === "string"
            ? partial
            : (partial?.content?.filter((b: any) => b.type === "text").map((b: any) => b.text).join("\n") ?? "");
        const display = partial?.details?.display;
        if (text || display?.length)
          upsert(key, event.toolCallId, (prev) => ({
            ...(prev as Entry & { kind: "tool" }),
            ...(text ? { output: text } : {}),
            ...(display?.length ? { display } : {}),
          }));
        break;
      }
      case "tool_execution_end": {
        if (event.toolName === "edit" || event.toolName === "write") setFilesRefresh((n) => n + 1);
        const result = (event as any).result;
        const text =
          typeof result === "string"
            ? result
            : (result?.content?.filter((b: any) => b.type === "text").map((b: any) => b.text).join("\n") ?? "");
        upsert(key, event.toolCallId, (prev) => ({
          ...(prev as Entry & { kind: "tool" }),
          status: event.isError ? "error" : "ok",
          output: text || (prev as any)?.output,
          error: event.isError ? text.split("\n")[0] : undefined,
          display: result?.details?.display ?? (prev as any)?.display,
        }));
        break;
      }
      case "session_state":
        setSessions((map) => patchSession(map, key, applyState(event)));
        break;
      case "compaction_start":
        append(key, { kind: "info", text: "compacting context…" });
        break;
      case "compaction_end": {
        // an aborted or failed compaction must not report success
        if (event.aborted) append(key, { kind: "info", text: "compaction cancelled" });
        else if (event.errorMessage)
          append(key, { kind: "error", text: cleanError(event.errorMessage) });
        else {
          const before = event.result?.tokensBefore;
          const after = event.result?.estimatedTokensAfter;
          append(key, {
            kind: "info",
            text: before
              ? `context compacted — ${before.toLocaleString()} tokens${after ? ` → ~${after.toLocaleString()}` : ""}`
              : "context compacted",
          });
        }
        break;
      }
      case "auto_retry_start":
        append(key, {
          kind: "info",
          text: `retrying (${event.attempt}/${event.maxAttempts}): ${event.errorMessage}`,
        });
        break;
      case "auto_retry_end":
        if (!event.success)
          append(key, { kind: "error", text: event.finalError ?? "retries exhausted" });
        break;
    }
  }

  /** Get or lazily open the backend session behind a session key. */
  function ensureAgent(
    key: string,
    opts?: { fresh?: boolean },
  ): Promise<SessionHandle> {
    const existing = agents.current.get(key);
    if (existing) return existing;
    const created = backend()
      .session(key, {
        initial: {
          model: getSession(sessions, key).model ?? defaultModel,
          thinkingLevel: prefs.thinkingLevel,
          autoCompact: prefs.autoCompact === "on",
          codemode: prefs.codemode === "on",
        },
        fresh: opts?.fresh,
        tools: tuiTools,
      })
      .then((agent) => {
        agent.subscribe((event) => {
          if (event.type === "session_closed" && agents.current.get(key) === created) agents.current.delete(key);
          handleAgentEvent(key, event);
        });
        // a turn that outlived the previous client is still going: show it as such
        setSessions((map) => patchSession(map, key, { ...applyState(agent.state), busy: agent.busy }));
        if (!opts?.fresh) restoreTranscript(key, agent);
        return agent;
      });
    agents.current.set(key, created);
    created.catch(() => {
      agents.current.delete(key);
      setSessions((map) => patchSession(map, key, { restored: true }));
    });
    return created;
  }

  /** Show an interactive prompt and resolve with the chosen option id. */
  function pushAsk(ask: Omit<Ask, "resolve">): Promise<string> {
    return new Promise((resolve) => {
      const self: Ask = {
        ...ask,
        resolve: (id) => {
          setAsks((current) => current.filter((a) => a !== self));
          resolve(id);
        },
      };
      setAsks((prev) => [...prev, self]);
    });
  }
  const pushAskRef = useRef(pushAsk);
  pushAskRef.current = pushAsk;

  /** ^f / f: chat ⇄ files. Jobs have their own key (^j). */
  function cycleView(): void {
    if (focus === 0) {
      append(activeKey, { kind: "info", text: "enter a workspace first — f opens its files" });
      return;
    }
    setView(view === "files" ? "agent" : "files");
  }

  /** Help popup: the keyboard/command reference in a panel, not the transcript. */
  function openHelp() {
    pushAskRef.current({
      key: activeKey,
      title: "Keyboard shortcuts",
      body: [
        "Navigation (NORMAL mode)",
        "  i           type a prompt        /    commands",
        "  j k         workspace ring       p    jump: env · session · ws",
        "  f · ^j      files · jobs screen   r    (in files) rescan",
        "  ^g · ^q     the plan · finished tasks",
        "  1-9 · 0     workspace N · main   p    jump anywhere",
        "  esc         interrupt the agent  ?    this help",
        "  u d         scroll",
        "",
        "Typing (INSERT mode)",
        "  enter       send                 shift+enter · \\+enter  new line",
        "  up / down   prompt history (up on an empty prompt edits the queue)",
        "              esc  clear + back to NORMAL",
      ].join("\n"),
      options: [{ id: "close", label: "Close" }],
      escapeId: "close",
    }).catch(() => {});
  }

  /** Shift+tab cycles these in order. */
  const PERM_MODES: PermMode[] = ["default", "acceptEdits", "plan", "bypass"];

  /** Rebuild the transcript + prompt history from the daemon's messages; always replaces the entries. */
  function restoreTranscript(key: string, agent: SessionHandle) {
    const { entries, history } = transcript(agent.messages, agent.busy, toolSummary);
    setSessions((map) =>
      patchSession(map, key, (s) => ({
        entries: foldRetries(entries),
        history: s.history.length ? s.history : history,
        restored: true,
      })),
    );
  }

  function interrupt(key: string) {
    agents.current.get(key)?.then((a) => a.abort());
  }

  // Dispose sessions when the app unmounts.
  useEffect(() => () => {
    for (const agent of agents.current.values()) agent.then((a) => a.dispose()).catch(() => {});
  }, []);

  /** Widen or narrow the sidebar by `delta`, clamped, and persist it. */
  function resizeSidebar(delta: number) {
    setPrefs((p) => {
      const sidebarWidth = clampSidebar(p.sidebarWidth + delta);
      backend().settings.write({ sidebarWidth }).catch(() => {});
      return { ...p, sidebarWidth };
    });
  }

  function changeInput(v: string) {
    setHistIdx(null); // typing exits history recall
    // a leading "/" on an empty draft is the command overlay, not text — the
    // prompt owns the key when it is live, so this is where "/" is caught.
    // The "/" stays in the draft: Input keeps its own copy of the value, and
    // clearing it here would desync the two.
    if (v === "/" && !cmdMode && !palette) setCmdMode(true);
    // ...and deleting it back out closes it again, so the overlay never
    // outlives the slash that opened it
    if (cmdMode && !v.startsWith("/")) setCmdMode(false);
    setInput(v);
  }

  /** `steer` interrupts the running turn; otherwise a mid-turn prompt queues. */
  function submit(text: string, steer = false) {
    if (palette) {
      setPalette(false);
      setInput("");
      return;
    }
    const trimmed = text.trim();
    if (!trimmed && images.length === 0) return;
    setInput("");

    if (trimmed === "/exit") return exit();
    if (trimmed === "/btw" || trimmed.startsWith("/btw ")) {
      const q = trimmed.slice(4).trim();
      if (!q) return append(activeKey, { kind: "info", text: "usage: /btw <question> — a side question, not saved" });
      const key = activeKey;
      const n = ++btwN.current;
      setBtw({ key, q });
      const done = (p: Partial<BtwState>) => btwN.current === n && setBtw({ key, q, ...p });
      ensureAgent(key)
        .then((a) => a.btw(q))
        .then((answer) => done({ answer }))
        .catch((e) => done({ error: String(e?.message ?? e) }));
      return;
    }
    if (trimmed === "/clear") {
      // the daemon archives the transcript and tells every view (this one included) to reopen empty
      void backend().sessions.clear(activeKey).catch((e) => append(activeKey, { kind: "error", text: String(e.message ?? e) }));
      return;
    }
    if (trimmed.startsWith("/session")) {
      const rest = trimmed.slice("/session".length).trim();
      const [verb, ...words] = rest.split(/\s+/);
      const arg = words.join(" ").trim();
      if (verb === "name" && arg) {
        // name the session in use, so it can be found in the list later
        backend().sessions.name(activeKey, arg).catch((e) => append(activeKey, { kind: "error", text: String(e.message ?? e) }));
        return append(activeKey, { kind: "info", text: `session named "${arg}"` });
      }
      if ((verb === "desc" || verb === "describe") && arg) {
        backend().sessions.describe(activeKey, arg).catch((e) => append(activeKey, { kind: "error", text: String(e.message ?? e) }));
        return append(activeKey, { kind: "info", text: `session described "${arg}"` });
      }
      if (verb === "use" && arg) {
        setSessionId((m) => ({ ...m, [activeBase]: arg }));
        return;
      }
      openCmd("session ");
      return;
    }
    if (trimmed === "/tools") {
      const names = [...hello().tools, ...tuiTools.map((t) => t.name)];
      return append(activeKey, {
        kind: "agent",
        text: names.length ? names.join(", ") : "No tools registered.",
      });
    }
    if (trimmed === "/help") return openHelp();
    if (trimmed === "/files") {
      if (focus > 0) setView("files");
      else append(activeKey, { kind: "info", text: "enter a workspace first — /files browses its files and diffs" });
      return;
    }
    if (trimmed.startsWith("/theme ")) {
      const name = trimmed.slice(7).trim();
      setTheme(name);
      backend().settings.write({ theme: name }).catch(() => {}); // persists across restarts
      setThemeTick((t) => t + 1);
      return;
    }
    if (trimmed.startsWith("/model ")) {
      const ref = trimmed.slice(7).trim();
      const slash = ref.indexOf("/");
      if (slash > 0) {
        const provider = ref.slice(0, slash);
        const id = ref.slice(slash + 1);
        setSessions((map) => patchSession(map, activeKey, { model: { provider, id } }));
        backend().settings.write({ defaultModel: { provider, id } }).catch(() => {}); // persists across restarts
        setDefaultModel({ provider, id });
        agents.current.get(activeKey)?.then((a) => a.setModel({ provider, id })).catch(() => {});
      }
      return;
    }
    if (trimmed.startsWith("/login ")) {
      const [provider, type] = trimmed.slice(7).trim().split(/\s+/);
      if (provider)
        setLogin({
          provider,
          type: type === "api_key" ? type : "oauth",
        });
      return;
    }
    if (trimmed.startsWith("/settings ")) {
      const [key, value] = trimmed.slice(10).trim().split(/\s+/);
      if ((key === "sidebar" || key === "thinking") && (value === "show" || value === "hide")) {
        setPrefs((p) => ({ ...p, [key]: value }));
        backend().settings.write({ [key]: value }).catch(() => {});
        // narrow terminal: "show" also opens the overlay, "hide" closes it
        if (key === "sidebar") setSidebarOpen(value === "show");
      }
      if (key === "width" && (value === "wider" || value === "narrower" || value === "reset")) {
        if (value === "reset") resizeSidebar(SIDEBAR_WIDTH - prefs.sidebarWidth);
        else resizeSidebar(value === "wider" ? SIDEBAR_STEP : -SIDEBAR_STEP);
      }
      if (key === "thinkingLevel" && (THINKING_LEVELS as readonly string[]).includes(value ?? "")) {
        const level = value as ThinkingLevel;
        setPrefs((p) => ({ ...p, thinkingLevel: level }));
        backend().settings.write({ thinkingLevel: level }).catch(() => {});
        // every live session, not just the visible one — a turn may be streaming elsewhere
        for (const agent of agents.current.values())
          agent.then((a) => a.setThinkingLevel(level)).catch(() => {});
      }
      if (key === "autoCompact" && (value === "on" || value === "off")) {
        setPrefs((p) => ({ ...p, autoCompact: value }));
        backend().settings.write({ autoCompact: value }).catch(() => {});
        for (const agent of agents.current.values())
          agent.then((a) => a.setAutoCompactionEnabled(value === "on")).catch(() => {});
      }
      // the daemon rebuilds idle agents now and busy ones at agent_end
      if (key === "codemode" && (value === "on" || value === "off")) {
        setPrefs((p) => ({ ...p, codemode: value }));
        backend().settings.write({ codemode: value }).catch(() => {});
        append(activeKey, { kind: "info", text: `codemode ${value} — every open session now runs with it` });
      }
      if (key === "vim" && (value === "on" || value === "off")) {
        setPrefs((p) => ({ ...p, vim: value }));
        backend().settings.write({ vim: value }).catch(() => {});
        // leaving vim behind drops you in the prompt; entering it starts in NORMAL
        setKeyMode(value === "on" ? "normal" : "insert");
      }
      return;
    }
    // bare option-commands: reopen the command overlay with the prefix
    if (["/model", "/theme", "/login", "/settings"].includes(trimmed)) {
      openCmd(`${trimmed.slice(1)} `);
      return;
    }

    // The turn belongs to the session it started in. pi's prompt() *refuses*
    // while a turn streams ("Agent is already processing") rather than
    // queueing — steer() is the explicit verb for that, so pick it here from
    // the target session's own busy flag, not the visible one's: the user may
    // be looking at a different session than the one they are prompting.
    const key = activeKey;
    const streaming = getSession(sessions, key).busy;
    const sent = images;
    setImages([]);
    pasted.current = 0;
    // the tokens go to the model too: they are how it tells one attachment
    // from another, and a message that is only an image would otherwise be
    // empty text with the images silently dropped alongside it
    setSessions((map) =>
      patchSession(map, key, (s) => ({ history: [...s.history, trimmed] })),
    );
    setHistIdx(null);
    ensureAgent(key)
      .then((agent) =>
        streaming
          ? // mid-turn: plain enter queues behind the turn, ctrl+enter (or ^s)
            // cuts in. Both carry the images; dropping them loses the paste.
            void (steer
              ? agent.steer(trimmed, sent.length ? sent : undefined)
              : agent.followUp(trimmed, sent.length ? sent : undefined))
          : agent.prompt(trimmed, sent.length ? { images: sent } : undefined),
      )
      .catch((err) => append(key, { kind: "error", text: cleanError(String(err)) }));
  }

  type JumpItem = { label: string; hint: string; group: string; run: () => void };
  const paletteItems: JumpItem[] = [
    ...workspaces
      .map((w, i) => ({ w, i }))
      .filter(({ w }) => w.owner === user)
      .map(({ w, i }) => ({
        label: w.name,
        hint: w.status === "cloning" ? (w.progress ?? w.status) : w.status,
        group: `Workspaces · ${environment?.name ?? "no environment"}`,
        run: () => setFocus(i + 1),
      })),
    {
      label: "main context",
      hint: "orchestrator",
      group: "Context",
      run: () => setFocus(0),
    },
  ];

  // Built once per auth or catalog change, NOT per keystroke — sorting/mapping
  // the full model catalog on every key press is feelable input latency.
  const menuCtx = useMemo(
    () => ({
      // Only models you can actually run: a provider with no credentials
      // contributes nothing to pick from, and the catalog is 1300+ models, so
      // the unreachable ones are pure noise. /login is how a provider appears.
      // Grouped by provider and sorted within it, so a provider's models sit
      // together in the menu's scrolling window. The label is already
      // `provider/id`, so the hint carries the model's display name.
      models: [...models]
        .filter((m) => auth.get(m.provider)?.ok)
        .sort((a, b) => a.provider.localeCompare(b.provider) || a.id.localeCompare(b.id))
        .map((m) => ({ provider: m.provider, id: m.id, hint: m.name })),
      themes: themeNames,
      logins: hello().logins,
      // this environment's main sessions, newest first
      // sessions of the context you are in (environment main, or this workspace)
      sessions: baseSessions
        .filter((m) => m.key === activeBase || m.key.startsWith(`${activeBase}:`))
        .map((m) => {
          const id = sessionIdOf(activeBase, m.key);
          return {
            id,
            label: m.name ?? (id === "main" ? "main" : "untitled"),
            hint: [id === (sessionId[activeBase] ?? "main") ? "current" : "", ago(m.updated)]
              .filter(Boolean)
              .join(" · "),
          };
        }),
      settings: [
        ...(["sidebar", "thinking"] as const).flatMap((key) =>
          (["show", "hide"] as const).map((value) => ({
            key,
            value,
            hint: prefs[key] === value ? "current" : "",
          })),
        ),
        ...THINKING_LEVELS.map((value) => ({
          key: "thinkingLevel",
          value,
          hint: [prefs.thinkingLevel === value ? "current" : "", THINKING_HINT[value]]
            .filter(Boolean)
            .join(" · "),
        })),
        ...(["on", "off"] as const).map((value) => ({
          key: "autoCompact",
          value,
          hint: prefs.autoCompact === value ? "current" : "",
        })),
        ...(["on", "off"] as const).map((value) => ({
          key: "codemode",
          value,
          hint: [prefs.codemode === value ? "current" : "", "model scripts its tool calls"]
            .filter(Boolean)
            .join(" · "),
        })),
        ...(["wider", "narrower", "reset"] as const).map((value) => ({
          key: "width",
          value,
          hint:
            value === "reset"
              ? `back to ${SIDEBAR_WIDTH}`
              : `${prefs.sidebarWidth} cols${prefs.sidebarWidth === (value === "wider" ? SIDEBAR_MAX : SIDEBAR_MIN) ? " — at the limit" : ""}`,
        })),
        ...(["on", "off"] as const).map((value) => ({
          key: "vim",
          value,
          hint: prefs.vim === value ? "current" : value === "on" ? "letter commands" : "type freely",
        })),
      ],
    }),
    [auth, models, prefs, envs, env, focus, environment, activeBase, sessionId, baseSessions],
  );
  const jumpMatches = useMemo(
    () =>
      palette
        ? paletteItems.filter((it) => it.label.toLowerCase().includes(input.toLowerCase()))
        : [],
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [palette, input, envs, env, focus],
  );
  const menu = useMemo(() => {
    if (palette)
      return jumpMatches.map((it, i) => ({
        insert: String(i),
        label: it.label,
        hint: it.hint ? `${it.group} · ${it.hint}` : it.group,
      }));
    if (cmdMode) return menuItems(input, menuCtx);
    return [];
  }, [palette, cmdMode, jumpMatches, input, menuCtx]);
  const filesView = view === "files" && focus > 0;
  const processesView = view === "processes";
  const planView = view === "plan";
  const doneView = view === "done";
  // the plan: boards per session, in this session's words (main sees all, a workspace only itself)
  const nameOf = (key: string) => workspaces.find((w) => w.id === key)?.name ?? key;
  const stateOf = (key: string) => {
    const i = workspaces.findIndex((w) => w.id === key);
    if (i < 0) return "";
    const w = workspaces[i]!;
    return asks.some((a) => baseOf(a.key) === key) ? "needs you" : getSession(sessions, key).busy || watched?.some((m) => baseOf(m.key) === key && m.busy) ? "working" : w.status === "stopped" ? "" : "idle";
  };
  const planAll = (max: number) => planRows(space?.boards ?? [], nameOf, activeBase, stateOf, max);
  // main's Jobs screen: every workspace's processes, ids prefixed so two workspaces' "p1" stay apart
  const allJobs = workspaces
    .filter((w) => w.processes?.length)
    .map((w) => ({ label: w.name, processes: w.processes!.map((p) => ({ ...p, id: `${w.id}/${p.id}` })) }));
  const wide = columns > WIDE_COLUMNS;
  // "show" is opencode's "auto": docked when wide, otherwise only when opened
  const sidebarVisible = prefs.sidebar === "show" && (wide || sidebarOpen);
  // opencode: dimensions.width - sidebar - 4 (the column's paddingX)
  const contentWidth = columns - (sidebarVisible && wide ? prefs.sidebarWidth : 0) - 4;
  const modalOpen = login !== null || shownAsk !== undefined;
  // typing reaches the input only in INSERT (jump mode always types the filter)
  const inputLive = !modalOpen && (prefs.vim === "off" || keyMode === "insert" || palette || cmdMode);

  // just where you are: the workspace (with its parent, if it is an ephemeral
  // one), or the working session at the main context — the same inherited names
  // the title bar shows, never derived from a prompt.
  const contextPath = (
    focus === 0 ? ["Working Session"] : wsPath(workspaces, workspaces[focus - 1]!)
  ).join(" › ");

  const sidebarEl = (
    <Sidebar
      workspaces={workspaces}
      services={environment?.services ?? []}
      envName={environment?.name}
      snapshot={environment?.snapshot}
      envOwner={environment && environment.owner !== user ? environment.owner : undefined}
      user={user}
      unavailable={space && !space.available ? space.error ?? "unknown error" : undefined}
      focus={focus}
      width={prefs.sidebarWidth}
      running={workspaces.map((w) => getSession(sessions, w.id).busy || !!watched?.some((m) => baseOf(m.key) === w.id && m.busy))}
      waiting={workspaces.map((w) => asks.some((a) => baseOf(a.key) === w.id))}
      onFocus={(f) => {
        setFocus(f);
      }}
    />
  );

  return (
    // pinned to the terminal size: without it the tree grows with content
    // and pushes the strip/hint bar (and tabs) off screen
    <box flexDirection="column" width={columns} height={rows} backgroundColor={theme.bg}>
      <box flexDirection="row" flexGrow={1} minHeight={0} flexBasis={0} flexShrink={1} overflow="hidden">
        {/* session column: paddingX 2, paddingBottom 1, gap 1 — no top
            padding, the title bar is the column's top edge */}
        <box
          flexDirection="column"
          flexGrow={1}
          minHeight={0}
          paddingLeft={2}
          paddingRight={2}
          paddingBottom={1}
          gap={1}
        >
          {login ? (
            <box flexGrow={1} flexDirection="column">
              <Login
                provider={login.provider}
                type={login.type}
                onDone={(ok) => {
                  setLogin(null);
                  if (ok) {
                    loadProviderAuth().then(setAuth).catch(() => {});
                    refreshCatalog().then(setModels).catch(() => {});
                  }
                }}
              />
            </box>
          ) : processesView ? (
            <Processes
              workspace={focus === 0 ? "main" : workspaces[focus - 1]!.name}
              processes={focus === 0 ? allJobs.flatMap((g) => g.processes) : workspaces[focus - 1]!.processes ?? []}
              groups={focus === 0 ? allJobs : undefined}
              onClose={() => setView("agent")}
            />
          ) : planView ? (
            <PlanScreen rows={planAll(Infinity)} width={contentWidth} onClose={() => setView("agent")} />
          ) : doneView ? (
            <DoneScreen tasks={doneTasks(space?.boards ?? [], nameOf, activeBase)} main={focus === 0} onClose={() => setView("agent")} />
          ) : filesView ? (
            <Files
              root={hello().cwd}
              workspace={workspaces[focus - 1]!.id}
              refreshKey={filesRefresh}
              onClose={() => setView("agent")}
              onCycle={cycleView}
            />
          ) : (
            <>
            <SessionTitle
              // the title is inherited, never derived from the first prompt:
              // the main session is the working session, a workspace session is
              // the workspace, an ephemeral one is its own agent name
              title={focus === 0 ? "Working Session" : workspaces[focus - 1]!.name}
              description={watched?.find((m) => m.key === activeKey)?.description}
              busy={busy}
              width={contentWidth}
              onOpen={() => openCmd("session ")}
            />
            <Transcript
              width={contentWidth}
              names={nameOf}
              to={focus === 0 ? "main" : workspaces[focus - 1]!.name}
              keys={
                modalOpen
                  ? "off"
                  : prefs.vim === "on" && keyMode === "normal" && !palette && !cmdMode
                    ? "normal"
                    : "page"
              }
              entries={
                prefs.thinking === "hide"
                  ? session.entries.filter((e) => e.kind !== "thinking")
                  : session.entries
              }
              ready={session.restored === true}
            />
            </>
          )}
          {/* prompt block, opencode structure: card + strip + footer row stack
              tight; question/permission panels replace the whole block */}
          {!filesView && !processesView && !planView && !doneView && (
          <box flexDirection="column" flexShrink={0}>
          <Plan rows={planAll(Math.max(6, Math.floor(rows * 0.4)))} width={contentWidth} />
          <Queue
            messages={session.queued}
            selected={queuePick}
            width={contentWidth}
            onSelect={(i) => setQueuePick(i)}
          />
          {btw && btw.key === activeKey && <Btw state={btw} width={contentWidth} />}
          {busy && !shownAsk && (
            <box paddingLeft={1} marginBottom={1} flexDirection="row">
              <text>
                <Spinner fg={theme.accent} />{" "}
                <span fg={theme.muted}>
                  {(() => {
                    const tool = [...session.entries]
                      .reverse()
                      .find((e) => e.kind === "tool" && e.status === "running") as
                      | (Entry & { kind: "tool" })
                      | undefined;
                    if (tool) return `${tool.name} · ${tool.summary}`;
                    const last = session.entries[session.entries.length - 1];
                    return last?.kind === "thinking" ? "thinking…" : "working…";
                  })()}
                </span>
              </text>
            </box>
          )}
          {shownAsk ? (
            <AskPanel ask={shownAsk} />
          ) : (
          <>
            <Prompt
              value={input}
              onPasteImage={pasteImage}
              onHistory={recall}
              onChange={changeInput}
              onSubmit={
                cmdMode
                  ? (text) => {
                      setCmdMode(false);
                      setInput("");
                      submit(text);
                    }
                  : submit
              }
              overlay={
                palette
                  ? "jump"
                  : cmdMode
                    ? "command"
                    : prefs.vim === "on" && keyMode === "normal"
                      ? "normal"
                      : undefined
              }
              onPick={
                palette
                  ? (insert) => {
                      jumpMatches[Number(insert)]?.run();
                      setPalette(false);
                      setInput("");
                    }
                  : cmdMode
                    ? (insert) => {
                        if (insert.endsWith(" ")) {
                          // command with options: stay in the overlay, filter them
                          setInput(insert);
                          return;
                        }
                        setCmdMode(false);
                        setInput("");
                        submit(insert);
                      }
                    : undefined
              }
              placeholder={palette ? "Jump to…" : cmdMode ? "Type a command…" : session.entries.length === 0 ? placeholders[hint]! : ""}
              model={modelLabel(session.model)}
              provider={session.model.provider}
              session={contextPath}
              inputActive={inputLive}
              menu={menu}
            />
          <HintBar
            permMode={permMode}
            normal={prefs.vim === "on" && keyMode === "normal" && !palette && !cmdMode}
            vim={prefs.vim === "on"}
            busy={busy}
            tokens={session.tokens}
            queued={session.queued.length}
            active={environment?.name ?? ""}
            inWorkspace={focus > 0}
            compact={!wide}
            onHint={(id) => {
              if (id === "type") return setKeyMode("insert");
              if (id === "jump") {
                setInput("");
                return setPalette(true);
              }
              if (id === "files") return focus > 0 ? setView("files") : undefined;
              if (id === "jobs") return setView("processes");
              if (id === "plan") return setView("plan");
              if (id === "done") return setView("done");
              if (id === "commands") return openCmd();
              if (id === "help") return openHelp();
              if (id === "queue") {
                setKeyMode("normal");
                return setQueuePick(session.queued.length > 0 ? 0 : null);
              }
            }}
          />
          </>
          )}
          </box>
          )}
        </box>
        {sidebarVisible && wide && sidebarEl}
        {sidebarVisible && !wide && (
          <box
            position="absolute"
            top={0}
            left={0}
            right={0}
            bottom={0}
            alignItems="flex-end"
            backgroundColor={RGBA.fromInts(0, 0, 0, 70)}
          >
            {sidebarEl}
          </box>
        )}
      </box>
      {copied && (
        <box position="absolute" right={2} bottom={1}>
          <text selectable={false} fg={theme.bg} bg={copied.ok ? theme.success : theme.error}>
            {` ${copied.ok ? "✓" : "✕"} ${copied.text} `}
          </text>
        </box>
      )}
    </box>
  );
}
