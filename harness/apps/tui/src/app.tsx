import { useEffect, useMemo, useRef, useState } from "react";
import { RGBA } from "@opentui/core";
import { useKeyboard, useRenderer, useTerminalDimensions } from "@opentui/react";
import type { ToolDef } from "@kloudlite-tui/tools";
import { SessionTitle } from "./components/SessionTitle.tsx";
import { Queue } from "./components/Queue.tsx";
import { Transcript, type Entry } from "./components/Transcript.tsx";
import { Prompt } from "./components/Prompt.tsx";
import { Sidebar } from "./components/Sidebar.tsx";
import { HintBar } from "./components/HintBar.tsx";
import { Files } from "./components/Files.tsx";
import { Processes } from "./components/Processes.tsx";
import { Spinner } from "./components/Spinner.tsx";
import { menuItems, placeholders } from "./slash.ts";
import { fromSpace, wsPath } from "./workspaces.ts";
import { setTheme, theme, themeNames } from "./theme.ts";
import { catalog, findModel, loadProviderAuth, modelLabel, refreshCatalog } from "./models.ts";
import type {
  Decision,
  ModelRef,
  PermissionRequest,
  SessionEvent,
  SessionHandle,
  SessionMeta,
  ThinkingLevel,
} from "@kloudlite-tui/backend";
import type { SpaceView } from "@kloudlite-tui/backend";
import { backend, hello } from "./hello.ts";

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
import { readClipboardImage, type ClipImage } from "./clipboard.ts";
import {
  getSession,
  patchSession,
    sessionIdOf,
  sessionKey,
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
type ViewId = "agent" | "files" | "processes";
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
  const inflight = useRef(false);
  const refreshRef = useRef<() => Promise<void>>(async () => {});
  refreshRef.current = async () => {
    if (inflight.current) return; // pod reads can outlast the beat; never stack them
    inflight.current = true;
    let v: SpaceView;
    try {
      v = await backend().space();
    } catch (e: any) {
      v = { available: false, error: String(e?.message ?? e).slice(0, 200), user: "", workspaces: [], environments: [] };
    } finally {
      inflight.current = false;
    }
    const { focus: f, workspaces: old } = live.current;
    const next = fromSpace(v).workspaces;
    const at = f > 0 ? next.findIndex((w) => w.id === old[f - 1]?.id) : -1;
    setSpace(v);
    if (f > 0) setFocus(at >= 0 ? at + 1 : Math.min(f, next.length));
  };
  useEffect(() => {
    void refreshRef.current();
    const t = setInterval(() => void refreshRef.current(), 5000);
    return () => clearInterval(t);
  }, []);
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

  const [sessionNames, setSessionNames] = useState<Record<string, string>>(() =>
    Object.fromEntries(hello().sessions.flatMap((m) => (m.name ? [[m.key, m.name]] : []))),
  );
  const [sessionDescs, setSessionDescs] = useState<Record<string, string>>(() =>
    Object.fromEntries(
      hello().sessions.flatMap((m) => (m.description ? [[m.key, m.description]] : [])),
    ),
  );
  // index into the active session's queue while editing it, else null
  const [queuePick, setQueuePick] = useState<number | null>(null);
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
  const [asks, setAsks] = useState<Ask[]>([]);
  // tools granted "always allow" per session key
  const alwaysAllow = useRef(new Map<string, Set<string>>());
  // Permission mode is per-process and resets on restart: a forgotten "bypass"
  // persisted across launches is the one failure worth not having.
  const [permMode, setPermMode] = useState<PermMode>("default");
  const modeRef = useRef<PermMode>("default");
  modeRef.current = permMode;

  const environment = envs[env]; // undefined until the space has an environment
  const mainBase = "main";
  const mainSession = sessionId[mainBase] ?? "main";
  const mainKey = sessionKey(undefined, mainSession);
  // sessions belong to the context you are in: the environment's, or this
  // workspace's own
  const activeBase = focus === 0 ? "main" : workspaces[focus - 1]!.id;
  const [baseSessions, setBaseSessions] = useState<SessionMeta[]>([]);
  useEffect(() => {
    backend().sessions.list(activeBase).then(setBaseSessions).catch(() => {});
  }, [activeBase, sessionNames, sessionDescs]);
  const activeKey = sessionKey(
    focus === 0 ? undefined : workspaces[focus - 1]!.id,
    sessionId[activeBase] ?? "main",
  );
  const session = getSession(sessions, activeKey);
  const busy = session.busy;

  function exit(): void {
    if (onExit) return onExit(); // server session: close the connection only
    renderer.destroy();
    process.exit(0);
  }

  // the file views only exist inside a workspace; leaving one goes back
  useEffect(() => {
    if (focus === 0) setView("agent");
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
    if (login || asks.length > 0 || filesView || processesView) return;
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
      if (name === "q") {
        setQueuePick(session.queued.length > 0 && queuePick === null ? 0 : null);
        return true;
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

    // ---- vim off: ctrl+<letter> commands, everything else types ----
    if (prefs.vim === "off" && key.ctrl && !key.meta && !menuOpen) {
      if (command(key.name)) return;
    }

    // shift+tab cycles the permission mode in both key schemes; plain tab keeps
    // cycling workspaces in NORMAL
    if (key.name === "tab" && key.shift && !menuOpen)
      return setPermMode((m) => PERM_MODES[(PERM_MODES.indexOf(m) + 1) % PERM_MODES.length]!);
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
      case "agent_end":
        setSessions((map) => patchSession(map, key, { busy: false }));
        void refreshRef.current(); // a create / stop / intercept the turn made shows now, not at the next beat
        break;
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
        const used = msg.usage?.totalTokens ?? 0;
        if (used)
          setSessions((map) => patchSession(map, key, (s) => ({ tokens: s.tokens + used })));
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
        upsert(key, event.toolCallId, () => ({
          kind: "tool",
          id: event.toolCallId,
          name: event.toolName,
          summary: toolSummary(event.toolName, event.args),
          status: "running",
          diff: event.diff,
        }));
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
      case "queue_update":
        setSessions((map) =>
          patchSession(map, key, {
            queued: [
              ...(event.steering ?? []).map((text: string) => ({ text, kind: "steer" as const })),
              ...(event.followUp ?? []).map((text: string) => ({ text, kind: "followUp" as const })),
            ],
          }),
        );
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
    opts?: { fresh?: boolean; model?: ModelRef; codemode?: boolean; after?: Promise<unknown> },
  ): Promise<SessionHandle> {
    const existing = agents.current.get(key);
    if (existing) return existing;
    // `after` is the old session's dispose: the backend keys sessions by name, so
    // a dispose landing after this open would close the new one instead
    const created = (opts?.after ?? Promise.resolve())
      .then(() =>
        backend().session(key, {
          model: opts?.model ?? getSession(sessions, key).model,
          fresh: opts?.fresh,
          thinkingLevel: prefs.thinkingLevel,
          autoCompact: prefs.autoCompact === "on",
          codemode: opts?.codemode ?? prefs.codemode === "on",
          tools: tuiTools,
          permission: (req, signal) => gate(key, req, signal),
        }),
      )
      .then((agent) => {
        agent.subscribe((event) => handleAgentEvent(key, event));
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

  /** Rebuild open sessions whose build-time state changed; each comes back with its transcript. */
  function reopen(keys: string[], opts?: { codemode?: boolean }) {
    for (const k of keys) {
      const old = agents.current.get(k);
      if (!old) continue;
      agents.current.delete(k);
      const after = old.then((a) => a.dispose()).catch(() => {});
      ensureAgent(k, { ...opts, after }).catch(() => {});
    }
  }

  /** Show an interactive prompt and resolve with the chosen option id. */
  function pushAsk(ask: Omit<Ask, "resolve">): Promise<string> {
    return new Promise((resolve) => {
      setAsks((prev) => [
        ...prev,
        {
          ...ask,
          resolve: (id) => {
            setAsks((current) => current.slice(1));
            resolve(id);
          },
        },
      ]);
    });
  }
  const pushAskRef = useRef(pushAsk);
  pushAskRef.current = pushAsk;

  // The model can ask the user a question with options (opencode's question tool).
  const registered = useRef(false);
  if (!registered.current) {
    registered.current = true;
    tuiTools.push({
      name: "question",
      description:
        "Ask the user a question and wait for their answer. Use when you need a decision or clarification. Provide 2-5 short answer options.",
      inputSchema: {
        type: "object",
        properties: {
          question: { type: "string", description: "The question to ask" },
          options: {
            type: "array",
            items: { type: "string" },
            description: "Selectable answer options",
          },
        },
        required: ["question", "options"],
      },
      run: async ({ question, options }: { question: string; options: string[] }) => {
        const picked = await pushAskRef.current({
          title: question,
          options: options.map((label, i) => ({ id: String(i), label })),
        });
        return options[Number(picked)] ?? picked;
      },
    });
  }

  /** One key for "what does the column show": chat › files › processes. */
  function cycleView(): void {
    if (focus === 0) {
      append(activeKey, { kind: "info", text: "enter a workspace first — f opens its files" });
      return;
    }
    const ring: ViewId[] = ["agent", "files", "processes"];
    setView(ring[(ring.indexOf(view) + 1) % ring.length]!);
  }

  /** Help popup: the keyboard/command reference in a panel, not the transcript. */
  function openHelp() {
    pushAskRef.current({
      title: "Keyboard shortcuts",
      body: [
        "Navigation (NORMAL mode)",
        "  i           type a prompt        /    commands",
        "  j k         workspace ring       p    jump: env · session · ws",
        "  f           cycle views           r    (in files) rescan",
        "  1-9 · 0     workspace N · main   p    jump anywhere",
        "  esc         interrupt the agent  ?    this help",
        "  u d         scroll",
        "",
        "Typing (INSERT mode)",
        "  enter       send                 shift+enter · \\+enter  new line",
        "  up / down   prompt history       esc  clear + back to NORMAL",
      ].join("\n"),
      options: [{ id: "close", label: "Close" }],
      escapeId: "close",
    }).catch(() => {});
  }

  /** Tools that require permission before running. */
  // web_fetch leaves the machine, and the URL can come from text the model
  // just read, so the user sees it before it goes out
  const GATED = new Set([
    "bash", "write", "edit", "patch", "exec", "web_fetch",
    "workspace_stop", "workspace_delete", "worktree_drop", "env_delete", "env_stop", "env_restore_in_place",
    "service_remove", "volume_delete", "snapshot_delete",
  ]);
  /** Tools that only mutate the workspace's files — what acceptEdits waves through. */
  const EDITS = new Set(["write", "edit", "patch"]);

/** Shift+tab cycles these in order. */
type PermMode = "default" | "acceptEdits" | "plan" | "bypass";
const PERM_MODES: PermMode[] = ["default", "acceptEdits", "plan", "bypass"];

  /**
   * The permission decision for one gated tool call. The backend calls this only for GATED
   * tools; the mode is read from a ref at call time because it changes under a running session.
   */
  async function gate(key: string, { name, args, diff }: PermissionRequest, _signal: AbortSignal): Promise<Decision> {
    const granted = alwaysAllow.current.get(key) ?? new Set<string>();
    const mode = modeRef.current;
    // plan mode answers rather than asks: a refusal the model can read and
    // work around beats a permission card the user has to reject every turn
    if (mode === "plan")
      return {
        block: true,
        reason: `Plan mode: ${name} is not available. Research and explain what you would do; the user will leave plan mode when they want it done.`,
      };
    if (mode === "bypass" || (mode === "acceptEdits" && EDITS.has(name)) || granted.has(name)) return {};
    const choice = await pushAskRef.current({
      title: "Permission required",
      subtitle:
        name === "bash"
          ? "Shell command"
          : name === "web_fetch"
            ? "Fetch a URL"
            : EDITS.has(name)
              ? `${name === "write" ? "Write" : "Edit"} ${args?.path ?? "file"}`
              : `Run ${name}`,
      body:
        name === "bash"
          ? `$ ${args?.command ?? ""}`
          : name === "web_fetch"
            ? String(args?.url ?? "")
            : diff
              ? undefined
              : toolSummary(name, args),
      diff,
      options: [
        { id: "once", label: "Allow once" },
        { id: "always", label: "Allow always" },
        { id: "reject", label: "Reject" },
      ],
      escapeId: "reject",
    });
    if (choice === "always") {
      granted.add(name);
      alwaysAllow.current.set(key, granted);
    } else if (choice === "reject") {
      return { block: true, reason: "The user rejected this tool call." };
    }
    return {};
  }

  /** Rebuild the transcript + prompt history from a restored session. */
  function restoreTranscript(key: string, agent: SessionHandle) {
    const messages = agent.messages;
    const markRestored = () =>
      setSessions((map) => patchSession(map, key, { restored: true }));
    if (messages.length === 0) return markRestored();
    const entries: Entry[] = [];
    const history: string[] = [];
    for (const m of messages as any[]) {
      if (m.role === "user") {
        const text = (m.content ?? [])
          .filter((b: any) => b.type === "text")
          .map((b: any) => b.text)
          .join("\n");
        if (text) {
          entries.push({ kind: "user", text });
          history.push(text);
        }
      } else if (m.role === "assistant") {
        for (const b of m.content ?? []) {
          if (b.type === "thinking" && b.thinking.trim())
            entries.push({ kind: "thinking", text: b.thinking, done: true });
          if (b.type === "text" && b.text.trim())
            entries.push({ kind: "agent", text: b.text });
          if (b.type === "toolCall")
            entries.push({
              kind: "tool",
              id: b.id,
              name: b.name,
              summary: toolSummary(b.name, b.arguments),
              status: "ok",
            });
        }
      } else if (m.role === "toolResult") {
        // same fold as tool_execution_end, so a reopened session shows what each tool returned
        const i = entries.findIndex((e) => e.kind === "tool" && e.id === m.toolCallId);
        if (i === -1) continue;
        const text = (m.content ?? [])
          .filter((b: any) => b.type === "text")
          .map((b: any) => b.text)
          .join("\n");
        entries[i] = {
          ...(entries[i] as Entry & { kind: "tool" }),
          status: m.isError ? "error" : "ok",
          output: text || undefined,
          error: m.isError ? text.split("\n")[0] : undefined,
          display: m.details?.display,
        };
      }
    }
    setSessions((map) =>
      patchSession(map, key, (s) =>
        s.entries.length === 0 ? { entries, history, restored: true } : { restored: true },
      ),
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
    if (trimmed === "/clear") {
      // start a brand-new persisted session: drop the live agent and its
      // restored history, so the cleared state survives a restart
      const key = activeKey;
      agents.current.get(key)?.then((a) => a.dispose()).catch(() => {});
      agents.current.delete(key);
      setSessions((map) =>
        patchSession(map, key, { entries: [], history: [], tokens: 0, queued: [], busy: false }),
      );
      // archive persisted transcripts, then open the fresh session behind them
      backend()
        .sessions.clear(key)
        .then(() => ensureAgent(key, { fresh: true }))
        .catch(() => {});
      return;
    }
    if (trimmed.startsWith("/session")) {
      const rest = trimmed.slice("/session".length).trim();
      const [verb, ...words] = rest.split(/\s+/);
      const arg = words.join(" ").trim();
      if (verb === "name" && arg) {
        // name the session in use, so it can be found in the list later
        backend().sessions.name(activeKey, arg).catch((e) => append(activeKey, { kind: "error", text: String(e.message ?? e) }));
        setSessionNames((n) => ({ ...n, [activeKey]: arg }));
        return append(activeKey, { kind: "info", text: `session named "${arg}"` });
      }
      if ((verb === "desc" || verb === "describe") && arg) {
        backend().sessions.describe(activeKey, arg).catch((e) => append(activeKey, { kind: "error", text: String(e.message ?? e) }));
        setSessionDescs((d) => ({ ...d, [activeKey]: arg }));
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
        const cur = agents.current.get(activeKey);
        if (cur) {
          // Claude and pi share one transcript (pi's file): a switch across
          // the two rebuilds the session for this key from that file
          cur
            .then(async (a) => {
              if (a.isClaude !== (provider === "anthropic")) {
                agents.current.delete(activeKey);
                await a.dispose();
                await ensureAgent(activeKey, { model: { provider, id } });
              } else {
                await a.setModel({ provider, id });
                // pi re-derives the level on a model switch (its own default, or
                // the level clamped by the last model), so restate the user's
                await a.setThinkingLevel(prefs.thinkingLevel);
              }
            })
            .catch(() => {});
        }
      }
      return;
    }
    if (trimmed.startsWith("/login ")) {
      const [provider, type] = trimmed.slice(7).trim().split(/\s+/);
      if (provider)
        setLogin({
          provider,
          type: type === "api_key" || type === "claude_code" ? type : "oauth",
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
      // pi fixes the tool list when the session is built, so unlike
      // thinkingLevel and autoCompact this cannot be pushed into a live session:
      // every open one is rebuilt instead, its transcript restored from pi's file
      if (key === "codemode" && (value === "on" || value === "off")) {
        const running = [...agents.current.keys()].filter((k) => getSession(sessions, k).busy);
        if (running.length) {
          append(activeKey, {
            kind: "error",
            text: `codemode unchanged — a turn is running in ${running.join(", ")}; let it finish or interrupt it first`,
          });
          return;
        }
        setPrefs((p) => ({ ...p, codemode: value }));
        backend().settings.write({ codemode: value }).catch(() => {});
        reopen([...agents.current.keys()], { codemode: value === "on" });
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
    // an unnamed session takes its title from the first thing asked of it
    if (!sessionNames[key]) {
      const title = trimmed.replace(/\s+/g, " ").slice(0, 40);
      backend().sessions.name(key, title).catch((e) => append(key, { kind: "error", text: String(e.message ?? e) }));
      setSessionNames((n) => ({ ...n, [key]: title }));
    }
    const sent = images;
    setImages([]);
    pasted.current = 0;
    // the tokens go to the model too: they are how it tells one attachment
    // from another, and a message that is only an image would otherwise be
    // empty text with the images silently dropped alongside it
    append(key, { kind: "user", text: trimmed, images: sent.length });
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
    [auth, models, prefs, envs, env, focus, environment, activeBase, sessionId, sessionNames, baseSessions],
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
  const processesView = view === "processes" && focus > 0;
  const wide = columns > WIDE_COLUMNS;
  // "show" is opencode's "auto": docked when wide, otherwise only when opened
  const sidebarVisible = prefs.sidebar === "show" && (wide || sidebarOpen);
  // opencode: dimensions.width - sidebar - 4 (the column's paddingX)
  const contentWidth = columns - (sidebarVisible && wide ? prefs.sidebarWidth : 0) - 4;
  const modalOpen = login !== null || asks.length > 0;
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
      running={workspaces.map((w) => getSession(sessions, w.id).busy)}
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
                    // a Claude child spawned signed out keeps its old credentials,
                    // so idle Claude sessions are rebuilt to spawn a fresh one
                    for (const [k, a] of agents.current)
                      a.then((h) => h.isClaude && !getSession(sessions, k).busy && reopen([k])).catch(() => {});
                    loadProviderAuth().then(setAuth).catch(() => {});
                    refreshCatalog().then(setModels).catch(() => {});
                  }
                }}
              />
            </box>
          ) : processesView ? (
            <Processes
              workspace={workspaces[focus - 1]!.name}
              processes={workspaces[focus - 1]!.processes ?? []}
              onClose={() => setView("agent")}
              onCycle={cycleView}
            />
          ) : filesView ? (
            <Files
              root={hello().cwd}
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
              description={sessionDescs[activeKey]}
              busy={busy}
              width={contentWidth}
              onOpen={() => openCmd("session ")}
            />
            <Transcript
              width={contentWidth}
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
          {!filesView && !processesView && (
          <box flexDirection="column" flexShrink={0}>
          <Queue
            messages={session.queued}
            selected={queuePick}
            width={contentWidth}
            onSelect={(i) => setQueuePick(i)}
          />
          {busy && asks.length === 0 && (
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
          {asks.length > 0 ? (
            <AskPanel ask={asks[0]!} />
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
    </box>
  );
}
