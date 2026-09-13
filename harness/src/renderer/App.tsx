import { For, Show, createEffect, createMemo, createSignal, onCleanup, onMount } from "solid-js";
import { createStore, produce } from "solid-js/store";
import { TitleBar } from "./components/TitleBar";
import { MachinePanel } from "./components/MachinePanel";
import { ActivityBar, type Activity } from "./components/ActivityBar";
import { ReposPanel, RegistriesPanel } from "./components/TeamPanel";
import { Chat, fit } from "./components/Chat";
import { Inspector } from "./components/inspector/Inspector";
import { StatusBar } from "./components/StatusBar";
import { TerminalPanel } from "./components/terminal/TerminalPanel";
import { makeTab, type TermTab } from "./components/terminal/tabs";
import { ENVIRONMENTS, IMAGES, MACHINES, REPOS, SNAPSHOTS, TEAMS, threadOf, type Thread } from "./model";
import { KEYS, threadIndex } from "./keys";
import { Palette, type PaletteItem } from "./components/Palette";
import { Confirm } from "./ui/Confirm";
import { Icon } from "./ui/Icon";
import * as live from "./live";
import { cycleTheme } from "./theme";

export function App() {
  // A developer has exactly one bench per team, so switching team is what
  // switches machine; there is nothing to choose within a team.
  // HARNESS_HASH=team:<id>/... opens on that team, for looking at an empty bench.
  const teamHash = /^team:([^/]+)/.exec(location.hash.slice(1))?.[1];
  const [teamId, setTeamId] = createSignal(TEAMS.find((t) => t.id === teamHash)?.id ?? TEAMS[0].id);
  const machine = createMemo(() => MACHINES.find((m) => m.teamId === teamId()) ?? MACHINES[0]);

  // Environments belong to the team, not the machine: the machine is connected
  // to one of them at a time.
  const [connected, setConnected] = createSignal(machine().environmentId);
  const environment = createMemo(() => ENVIRONMENTS.find((e) => e.id === connected()) ?? ENVIRONMENTS[0]);

  // Tabs are threads, and a thread belongs to a node: selecting the machine, a
  // workspace or an ephemeral opens its thread as a tab. There is nothing to
  // create — a thread exists because its node does.
  const hashView = location.hash.slice(1).replace(/^team:[^/]+\/?/, "");
  const first = (!hashView.startsWith("settings") && hashView.split("/")[0]) || machine().id;
  // Panes: the centre is one pane, or two side by side. Each pane has its own
  // tabs and its own selected tab; the ACTIVE pane is the one the keys, the
  // sidebar and the inspector speak to. A tab is dragged to reorder within a
  // pane or across; ⌘\ splits the current tab out to the right.
  type Pane = { open: string[]; sel: string };
  const [panes, setPanes] = createStore<Pane[]>([{ open: [first], sel: first }]);
  const [activePane, setActivePane] = createSignal(0);
  const pane = () => panes[Math.min(activePane(), panes.length - 1)];
  const open = () => pane().open;
  const selected = () => pane().sel;
  const paneOf = (id: string) => panes.findIndex((p) => p.open.includes(id));
  // A bench is several sessions at once — one per thing being worked on —
  // each a full pi of its own that is resumed on relaunch. Session 1 is the
  // "bench" pi; the rest are `s-N`. The list is the harness's to remember.
  // ponytail: no routing yet — a session's code change does not reach a
  // workspace's queue; the platform decides that next, the fixture stands in.
  type Session = { id: string; name: string; seq: number; lastActive?: number; archived?: boolean };
  // A session untouched this long is archived on start: its pi is not spawned
  // and it folds away, one click from coming back. Never the one being used.
  const ARCHIVE_AFTER = 24 * 60 * 60 * 1000;
  const stored = (): Session[] => {
    try {
      const v = JSON.parse(localStorage.getItem("harness.sessions") ?? "") as Session[];
      if (Array.isArray(v) && v.length && v[0].id === "bench") return v;
    } catch {
      /* first run, or unreadable: one session */
    }
    return [{ id: "bench", name: "session 1", seq: 1 }];
  };
  const [sessions, setSessions] = createStore<Session[]>(stored());
  const touch = (id: string) => {
    const i = sessions.findIndex((x) => x.id === id);
    if (i >= 0) setSessions(i, "lastActive", Date.now());
  };
  const live_ = () => sessions.filter((x) => !x.archived);
  const archived = () => sessions.filter((x) => x.archived);
  // Smart archive: at start, anything idle past the window goes to sleep.
  setSessions(produce((xs) => {
    const now = Date.now();
    for (const x of xs) if (!x.archived && x.lastActive && now - x.lastActive > ARCHIVE_AFTER && xs.filter((y) => !y.archived).length > 1) x.archived = true;
  }));
  createEffect(() => localStorage.setItem("harness.sessions", JSON.stringify(sessions.map((x) => ({ ...x })))));
  createEffect(() => live.setSessionCount(sessions.length));
  const isDefaultName = (x: Session) => x.name === `session ${x.seq}`;
  /** A session is named by its first prompt until it has one. */
  const nameSession = (id: string, text: string) => {
    const i = sessions.findIndex((x) => x.id === id);
    if (i >= 0 && isDefaultName(sessions[i]) && text.trim()) setSessions(i, "name", text.trim().replace(/\s+/g, " ").slice(0, 40));
  };
  const sessionThread = (id: string): Thread | undefined => {
    const x = sessions.find((y) => y.id === id);
    return x && { id, name: x.name, kind: "session", readonly: false, messages: [], pi: id };
  };
  const newSession = () => {
    const seq = Math.max(...sessions.map((x) => x.seq)) + 1;
    const id = `s-${seq}`;
    void window.harness.spawnPi(id).then(() => {
      setSessions((xs) => [...xs, { id, name: `session ${seq}`, seq, lastActive: Date.now() }]);
      showThread(id);
    });
  };
  /** Archive: the pi stops, the tab closes, the row folds away; nothing is lost. */
  const archiveSession = (id: string) => {
    if (live_().length < 2) return void live.thread(cur()).note("this is the only open session; start another before archiving it");
    void window.harness.stopPi(id);
    sides.filter((t) => t.session === id).forEach((t) => removeSide(t.id));
    setSessions(sessions.findIndex((x) => x.id === id), "archived", true);
    if (paneOf(id) >= 0) closeThread(id);
  };
  /** Back from the archive: the pi resumes its own session file. */
  const restoreSession = (id: string) => {
    void (id === "bench" ? Promise.resolve() : window.harness.spawnPi(id)).then(async () => {
      setSessions(sessions.findIndex((x) => x.id === id), { archived: false, lastActive: Date.now() });
      const r = await window.harness.pi({ type: "get_messages" }, id);
      live.thread(id).replay((r.data as { messages?: unknown[] } | undefined)?.messages ?? []);
      showThread(id);
    });
  };
  /** Delete: gone from the harness for good; pi's own session file stays on
      disk. What the session had in flight is stopped first, and what it sent
      to the workspaces is discarded with it — after the person says so. */
  const [confirm, setConfirm] = createSignal<{ id: string; items: string[] } | undefined>();
  const inFlight = (id: string) => [
    ...(live.thread(id).busy() ? ["the reply being written"] : []),
    ...live.tasks.filter((t) => t.session === id && (t.state === "running" || t.state === "background")).map((t) => `${t.tool} ${t.arg}`),
    ...live.procs.filter((p) => p.session === id && !p.ended).map((p) => `process ${p.name}`),
    ...live.thread(id).queue.map((q) => `queued: ${q.text}`),
  ];
  const idleSessions = () => live_().filter((x) => x.lastActive && Date.now() - x.lastActive > ARCHIVE_AFTER && x.id !== cur());
  const deleteSession = (id: string) => {
    const items = inFlight(id);
    if (items.length) return void setConfirm({ id, items });
    reallyDelete(id);
  };
  const reallyDelete = (id: string) => {
    setConfirm(undefined);
    // Stop what runs under it — commands and processes live in their own
    // process groups, so ending pi alone would leave them running.
    live.tasks.filter((t) => t.session === id && (t.state === "running" || t.state === "background")).forEach(live.cancel);
    live.procs.filter((p) => p.session === id && !p.ended).forEach(live.stopProc);
    sides.filter((t) => t.session === id).forEach((t) => removeSide(t.id));
    live.discard(id);
    if (id === "bench" || live_().length < 2) {
      // The first session's process is the bench's own and never ends: it
      // is emptied in place instead, and what was there is left on disk.
      void window.harness.pi({ type: "abort" }, id).then(() => window.harness.pi({ type: "new_session" }, id)).then(() => live.thread(id).replay([]));
      const i = sessions.findIndex((x) => x.id === id);
      if (i >= 0) setSessions(i, { name: `session ${sessions[i].seq}`, lastActive: Date.now(), archived: false });
      return;
    }
    void window.harness.pi({ type: "abort" }, id).finally(() => void window.harness.stopPi(id, true));
    setSessions((xs) => xs.filter((x) => x.id !== id));
    if (paneOf(id) >= 0) closeThread(id);
  };
  // Side sessions (`/btw`): read-only forks of a session, each on its own pi.
  // Their messages come from their own live state.
  const [sides, setSides] = createStore<Thread[]>([]);
  let sideSeq = 0;
  const threadsOf = (p: Pane) =>
    p.open
      .map((id) => threadOf(machine(), id) ?? sessionThread(id) ?? sides.find((t) => t.id === id))
      .filter((t) => t !== undefined)
      .map((t) => (t.pi ? { ...t, messages: live.thread(t.pi).messages } : t));
  const threads = createMemo(() => threadsOf(pane()));
  const setSelectedRaw = (id: string) => setPanes(activePane(), "sel", id);
  const setSelected = (id: string) => {
    const at = paneOf(id);
    if (at >= 0) setActivePane(at);
    else setPanes(activePane(), "open", (o) => [...o, id]);
    setPanes(at >= 0 ? at : activePane(), "sel", id);
  };
  const closeThread = (id: string) => {
    const pi = paneOf(id);
    if (pi < 0) return;
    const list = panes[pi].open;
    const i = list.indexOf(id);
    const rest = list.filter((x) => x !== id);
    if (!rest.length && panes.length > 1) {
      setPanes(produce((ps) => void ps.splice(pi, 1)));
      setActivePane(Math.max(0, Math.min(activePane(), panes.length - 1)));
      return;
    }
    setPanes(pi, { open: rest, sel: panes[pi].sel === id ? (list[i + 1] ?? list[i - 1] ?? "") : panes[pi].sel });
  };
  /** A btw is removed from the side bar, not by closing its tab: its answer
      is kept until then. Removing ends its process if it is still answering. */
  const removeSide = (id: string) => {
    void window.harness.stopPi(id);
    setSides((ts) => ts.filter((t) => t.id !== id));
    if (paneOf(id) >= 0) closeThread(id);
  };

  /** Drop `id` at `index` of pane `to` (from wherever it is); a pane emptied by the move goes away. */
  const moveTab = (id: string, to: number, index: number) => {
    const from = paneOf(id);
    if (from < 0) return;
    setPanes(produce((ps) => {
      const fromList = ps[from].open;
      const fromIdx = fromList.indexOf(id);
      fromList.splice(fromIdx, 1);
      if (from === to && fromIdx < index) index--;
      ps[to].open.splice(Math.max(0, Math.min(index, ps[to].open.length)), 0, id);
      ps[to].sel = id;
      if (ps[from].sel === id && from !== to) ps[from].sel = ps[from].open[0] ?? "";
      if (!ps[from].open.length && ps.length > 1) ps.splice(from, 1);
    }));
    setActivePane(Math.min(to, panes.length - 1));
  };
  const splitRight = () => {
    if (panes.length >= 2 || open().length < 2) return;
    const id = selected();
    setPanes(produce((ps) => void ps.push({ open: [], sel: "" })));
    moveTab(id, 1, 0);
  };

  // The environment opens in the centre, as its own tab. While it is showing,
  // the inspector is put away: it describes the machine's tree, which has
  // nothing to say about a team's environment, and leaving a workspace's details
  // beside it only invites the wrong reading.
  const [envTab, setEnvTabRaw] = createSignal(false);
  // Settings is a page like the environment; only one page shows at a time.
  const [settingsTab, setSettingsTab] = createSignal(hashView.startsWith("settings"));
  const setEnvTab = (v: boolean | ((p: boolean) => boolean)) => {
    const next = typeof v === "function" ? v(envTab()) : v;
    if (next) setSettingsTab(false);
    setEnvTabRaw(next);
  };
  const openSettings = () => {
    setEnvTabRaw(false);
    setFile(undefined);
    setSettingsTab(true);
  };

  // A file opens as a tab too: reading one is a subject of its own, not a
  // property of the workspace it came from.
  const [file, setFile] = createSignal<{ path: string; status?: string } | undefined>(
    hashView.endsWith("/diff") ? { path: "bins/agent/src/controller/run.rs", status: "M" } : undefined,
  );
  // A task's log opens in place like a file does; a process is shown through
  // the same page, its ring of output as the "output" and its uptime as the clock.
  const asTask = (p?: live.Proc): live.Task | undefined =>
    p && { id: p.id, session: p.session ?? "bench", tool: "Process", arg: `${p.name} · ${p.command}`, state: p.ended ? (p.code === 0 ? "done" : "failed") : "running", started: p.started, ended: p.ended, output: p.tail };
  const [taskId, setTaskId] = createSignal<string | undefined>();
  const inspector = () => rightOpen() && !envTab() && !settingsTab();

  const switchTeam = (id: string) => {
    setTeamId(id);
    const m = MACHINES.find((x) => x.teamId === id);
    if (m) {
      setConnected(m.environmentId);
      setPanes(produce((ps) => void ps.splice(0, ps.length, { open: [m.id], sel: m.id })));
      setActivePane(0);
    }
  };

  // Either side dock can be put away; the conversation takes the room.
  const [leftOpen, setLeftOpen] = createSignal(true);
  // Which view the side panel shows; the activity bar picks it, and picking
  // the current one again puts the panel away.
  const [view, setView] = createSignal<Activity>("workspaces");
  const pickView = (v: Activity) => {
    if (view() === v && leftOpen()) setLeftOpen(false);
    else (setView(v), setLeftOpen(true));
  };
  const [rightOpen, setRightOpen] = createSignal(true);

  // Terminals live here rather than in the panel, because a shell is opened from
  // the thing it belongs to — the machine or a working copy, in the inspector —
  // and the panel only shows what is open.
  const [tabs, setTabs] = createSignal<TermTab[]>([]);
  const [active, setActive] = createSignal("");
  const [maximised, setMaximised] = createSignal(false);
  const [height, setHeight] = createSignal(300);

  const openShell = (scopeId: string) => {
    const t = makeTab(machine(), environment().name, scopeId);
    setTabs((ts) => [...ts, t]);
    setActive(t.id);
  };
  const closeTab = (id: string) => {
    const rest = tabs().filter((t) => t.id !== id);
    setTabs(rest);
    if (rest.length === 0) setMaximised(false);
    else if (active() === id) setActive(rest[rest.length - 1].id);
  };
  const closePanel = () => {
    setTabs([]);
    setMaximised(false);
  };
  /** The Shell button and ⌘J are one gesture: open a shell here, or put the drawer away. */
  const toggleShell = (scopeId: string) => (tabs().length ? closePanel() : openShell(scopeId));

  /** Back out of one layer at a time: a file, then the environment, then a
      maximised shell, then the shell. Nothing else swallows escape. */
  const back = () => {
    if (file()) setFile(undefined);
    else if (taskId()) setTaskId(undefined);
    else if (envTab()) setEnvTab(false);
    else if (settingsTab()) setSettingsTab(false);
    else if (maximised()) setMaximised(false);
    else if (tabs().length) closePanel();
    else return false;
    return true;
  };
  /** ⌘W: whatever escape would close, else the thread tab itself (never main). */
  const closeCurrent = () => {
    if (back()) return;
    if (selected()) closeThread(selected());
  };

  const cycleThread = (by: number) => {
    const ts = open();
    if (ts.length < 2) return;
    const i = ts.indexOf(selected());
    showThread(ts[(i + by + ts.length) % ts.length]);
  };

  // Find is the page's own: a bar over the transcript drives Chromium's
  // find-in-page, so highlights and scrolling come for free.
  const [find, setFind] = createSignal<string | undefined>();
  const openFind = () => {
    setFind((v) => v ?? "");
    queueMicrotask(() => (document.getElementById("find") as HTMLInputElement | null)?.select());
  };
  const closeFind = () => {
    setFind(undefined);
    void window.harness.find("");
  };

  // The palette: ⌘P goes anywhere, ⌘T narrows to the machine and its working
  // copies, ⌘⇧P runs a command. One component, fed three lists built here
  // because this is where every action already lives.
  const [palette, setPalette] = createSignal<"go" | "workspaces" | "commands" | undefined>();
  // A thread is for typing into: opening one puts the caret in its prompt,
  // after the tab has rendered so the element exists to focus.
  const showThread = (id: string) => {
    setEnvTab(false);
    setSettingsTab(false);
    setFile(undefined);
    setSelected(id);
    queueMicrotask(() => composer()?.focus());
  };
  const goTo = showThread;
  // The selected session's pi and live state: what every command acts on.
  const cur = () => threads().find((t) => t.id === selected())?.pi ?? "bench";
  const L = () => live.thread(cur());
  const placeItems = createMemo<PaletteItem[]>(() => {
    const m = machine();
    const out: PaletteItem[] = [{ id: m.id, label: "Bench Thread", detail: m.goal, kind: "machine", icon: "machine", run: () => goTo(m.id) }];
    for (const x of sessions.slice(1)) out.push({ id: x.id, label: x.name, detail: x.id, kind: "session", icon: "thread", run: () => goTo(x.id) });
    for (const w of m.workspaces) {
      out.push({ id: w.id, label: w.name, detail: `${w.repo} · ${w.branch}`, kind: "workspace", icon: "workspace", run: () => goTo(w.id) });
      for (const e of w.ephemerals) out.push({ id: e.id, label: e.task, detail: `${w.name} · ${e.agent}`, kind: "ephemeral", icon: "ephemeral", run: () => goTo(e.id) });
    }
    return out;
  });
  const goItems = createMemo<PaletteItem[]>(() => {
    const out = [...placeItems()];
    for (const w of machine().workspaces)
      for (const c of w.changes) out.push({ id: `${w.id}:${c.path}`, label: c.path, detail: w.name, kind: "file", icon: "diff", run: () => (setEnvTab(false), setFile({ path: c.path, status: c.status })) });
    for (const s of environment().services)
      for (const p of s.ports)
        if (p.url) out.push({ id: `${s.name}:${p.port}`, label: `${s.name}:${p.port}`, detail: environment().name, kind: "service", icon: "globe", run: () => window.harness.openPreview(p.url!, `${environment().name} · ${s.name}:${p.port}`) });
    return out;
  });
  const commandItems = createMemo<PaletteItem[]>(() => [
    { id: "composer", label: "Focus the prompt", keys: KEYS.composer.keys, run: () => composer()?.focus() },
    { id: "shell", label: tabs().length ? "Close the shell" : "Open a shell", keys: KEYS.shell.keys, run: () => toggleShell(scope()) },
    { id: "env", label: envTab() ? "Close the environment" : "Open the environment", keys: KEYS.environment.keys, run: () => (setFile(undefined), setEnvTab((v) => !v)) },
    { id: "panel", label: leftOpen() ? "Hide workspaces" : "Show workspaces", keys: KEYS.panel.keys, run: () => setLeftOpen((v) => !v) },
    { id: "inspector", label: rightOpen() ? "Hide the inspector" : "Show the inspector", keys: KEYS.inspector.keys, run: () => setRightOpen((v) => !v) },
    { id: "workspaces", label: "Switch workspace…", keys: KEYS.workspaces.keys, run: () => setPalette("workspaces") },
    { id: "go", label: "Go to…", keys: KEYS.quickOpen.keys, run: () => setPalette("go") },
    { id: "settings", label: "Settings", keys: KEYS.settings.keys, run: openSettings },
    { id: "bg", label: "Send the running command to the background", keys: KEYS.background.keys, run: () => void window.harness.pi({ type: "prompt", message: "/bg" }, cur()) },
    { id: "abort", label: "Stop this session", run: () => void window.harness.pi({ type: "abort" }, cur()) },
    { id: "newSession", label: "New session", run: newSession },
    { id: "deleteSession", label: "Delete this session", run: () => deleteSession(cur()) },
    { id: "archiveSession", label: "Archive this session", run: () => archiveSession(cur()) },
    { id: "archiveIdle", label: "Archive idle sessions (untouched for a day)", run: () => idleSessions().forEach((x) => archiveSession(x.id)) },
    { id: "split", label: "Split the tab to the right", keys: KEYS.split.keys, run: splitRight },
    { id: "find", label: "Find in page", keys: KEYS.find.keys, run: openFind },
    { id: "prev", label: "Previous thread", keys: KEYS.prevThread.keys, run: () => cycleThread(-1) },
    { id: "next", label: "Next thread", keys: KEYS.nextThread.keys, run: () => cycleThread(1) },
    { id: "nth", label: "Thread by position", keys: "⌘1…9", run: () => setPalette("go") },
    { id: "close", label: "Close what is open", keys: KEYS.close.keys, run: closeCurrent },
    { id: "theme", label: "Cycle theme", run: cycleTheme },
    ...machine().plugins.filter((p) => p.kind === "skill" && p.enabled).map((p) => ({
      id: `skill:${p.name}`, label: `/${p.name}`, detail: (p as { summary: string }).summary, kind: "skill",
      run: () => {
        const c = composer();
        if (c) (c.value = `/${p.name} `, fit(c), c.focus());
      },
    })),
    ...TEAMS.filter((t) => t.id !== teamId()).map((t) => ({ id: `team:${t.id}`, label: `Switch to ${t.name}`, run: () => switchTeam(t.id) })),
    ...ENVIRONMENTS.filter((e) => e.id !== connected()).map((e) => ({ id: `env:${e.id}`, label: `Connect to ${e.name}`, run: () => setConnected(e.id) })),
  ]);

  const onKey = (e: KeyboardEvent) => {
    const hit = (b: { match: (e: KeyboardEvent) => boolean }) => b.match(e);
    const stop = () => e.preventDefault();

    if (hit(KEYS.steer)) return (stop(), send("steer"));
    if (hit(KEYS.send)) return (stop(), send());
    if (hit(KEYS.background)) return (stop(), void window.harness.pi({ type: "prompt", message: "/bg" }, cur()));
    if (hit(KEYS.split)) return (stop(), splitRight());
    if (hit(KEYS.focusPane)) return (stop(), void setActivePane((p) => (p + 1) % panes.length));
    if (hit(KEYS.commands)) return (stop(), void setPalette("commands"));
    if (hit(KEYS.quickOpen)) return (stop(), void setPalette("go"));
    if (hit(KEYS.workspaces)) return (stop(), void setPalette("workspaces"));
    if (hit(KEYS.find)) return (stop(), openFind());
    if (hit(KEYS.settings)) return (stop(), openSettings());
    if (palette()) return;
    if (hit(KEYS.back) && find() !== undefined) return closeFind();

    if (hit(KEYS.shell)) return (stop(), toggleShell(scope()));
    if (hit(KEYS.environment)) return (stop(), setFile(undefined), void setEnvTab((v) => !v));
    if (hit(KEYS.inspector)) return (stop(), void setRightOpen((v) => !v));
    if (hit(KEYS.panel)) return (stop(), void setLeftOpen((v) => !v));
    if (hit(KEYS.prevThread)) return (stop(), cycleThread(-1));
    if (hit(KEYS.nextThread)) return (stop(), cycleThread(1));
    if (hit(KEYS.close)) return (stop(), closeCurrent());
    if (hit(KEYS.back)) return void back();
    if (hit(KEYS.composer)) {
      stop();
      setFile(undefined);
      setEnvTab(false);
      composer()?.focus();
      return;
    }

    const i = threadIndex(e);
    if (i !== undefined && open()[i]) {
      stop();
      showThread(open()[i]);
    }
  };
  document.addEventListener("keydown", onKey);
  onCleanup(() => document.removeEventListener("keydown", onKey));
  onMount(() => composer()?.focus());

  // The bench thread is live: pi's events land here and the prompt goes to
  // pi. A workspace's thread stays a recorded fixture for now.
  window.harness.onPi(live.onEvent);
  // Every remembered session comes back: its pi resumed, its history replayed
  // before anything new lands, its name taken from its first prompt.
  for (const x of sessions.filter((y) => !y.archived)) {
    void (x.id === "bench" ? Promise.resolve() : window.harness.spawnPi(x.id)).then(async () => {
      const r = await window.harness.pi({ type: "get_messages" }, x.id);
      const ms = (r.data as { messages?: unknown[] } | undefined)?.messages ?? [];
      live.thread(x.id).replay(ms);
      const first = ms.find((m) => (m as { role?: string }).role === "user") as { content?: unknown } | undefined;
      const text = typeof first?.content === "string" ? first.content : ((first?.content as { text?: string }[] | undefined) ?? []).map((c) => c.text ?? "").join("");
      if (text) nameSession(x.id, text);
      void window.harness.pi({ type: "get_state" }, x.id);
    });
  }
  // Slash commands the harness answers itself, before anything reaches pi;
  // what is not listed here (/bg, /kl-login, /cancel, /skill:…) goes through.
  const SLASH: Record<string, { help: string; run: (arg: string) => void }> = {
    "/clear": { help: "start this session afresh; the old one stays on disk", run: () => void window.harness.pi({ type: "new_session" }, cur()).then(() => L().replay([])) },
    "/new": { help: "open another session beside this one", run: newSession },
    "/compact": { help: "summarise the older part of this session", run: () => void window.harness.pi({ type: "compact" }, cur()) },
    "/abort": { help: "stop what this session is doing", run: () => void window.harness.pi({ type: "abort" }, cur()) },
    "/model": { help: "switch model: /model provider/id", run: (arg) => { const [provider, modelId] = arg.split("/"); if (provider && modelId) void window.harness.pi({ type: "set_model", provider, modelId }, cur()); else L().note("usage: /model provider/id"); } },
    "/login": { help: "log in to Kloudlite in your browser", run: () => void window.harness.pi({ type: "prompt", message: "/kl-login" }, cur()) },
    "/settings": { help: "open settings", run: openSettings },
    "/btw": {
      help: "ask one question of a read-only fork of this session: /btw <question>",
      run: (arg) => void window.harness.pi({ type: "get_state" }, cur()).then(async (r) => {
        const from = cur();
        if (!arg.trim()) return L().note("usage: /btw <question> — one question, one answer, nothing changed");
        const file = (r.data as { sessionFile?: string } | undefined)?.sessionFile;
        if (!file) return L().note("this session has no file yet; say something first");
        const id = `btw-${++sideSeq}`;
        await window.harness.spawnPi(id, file);
        setSides((ts) => [...ts, { id, name: arg ? `btw · ${arg.slice(0, 40)}` : `btw #${sideSeq}`, kind: "btw", readonly: true, messages: [], pi: id, session: from }]);
        // Beside the bench when there is room for a second pane, else a tab.
        if (panes.length < 2) {
          setPanes(produce((ps) => void ps.push({ open: [id], sel: id })));
          setActivePane(panes.length - 1);
          queueMicrotask(() => composer()?.focus());
        } else showThread(id);
        // The fork holds the bench's whole history for the model; the tab
        // does not repeat it — one line says where this came from.
        const side = live.thread(id);
        const ms = await window.harness.pi({ type: "get_messages" }, id);
        const n = ((ms.data as { messages?: unknown[] } | undefined)?.messages ?? []).filter((m) => (m as { role?: string }).role === "user").length;
        side.replay([]);
        side.note(`forked from the bench · ${n} ${n === 1 ? "prompt" : "prompts"} of context · read-only · one answer`);
        side.sent(arg);
        void window.harness.pi({ type: "prompt", message: arg }, id);
        // One answer is the whole session: once it lands, the process goes.
        let ran = false;
        createEffect(() => {
          if (side.busy()) ran = true;
          else if (ran) (void window.harness.stopPi(id), side.setStatus("answered"));
        });
      }),
    },
    "/help": { help: "this list", run: () => L().note(Object.entries(SLASH).map(([k, v]) => `${k.padEnd(10)} ${v.help}`).join("\n") + "\n/bg        send the running command to the background (^B)\n/kl-login  log in to Kloudlite") },
  };

  /** Everything a `/` can start: the harness's own, pi's, and each enabled skill. */
  const slashItems = createMemo(() => [
    ...Object.entries(SLASH).map(([name, v]) => ({ name, help: v.help })),
    { name: "/bg", help: "send the running command to the background (^B)" },
    { name: "/cancel", help: "kill a running or backgrounded command: /cancel #N" },
    { name: "/kl-login", help: "log in to Kloudlite in your browser" },
    ...machine().plugins.filter((p) => p.kind === "skill" && p.enabled).map((p) => ({ name: `/${p.name}`, help: (p as { summary: string }).summary })),
  ]);

  /** The active pane's composer: every pane has one, so a global id would always find the first. */
  const composer = () => document.querySelector<HTMLTextAreaElement>(`[data-pane="${activePane()}"] textarea[data-composer]`);

  /** While the bench works, ↩ queues after it finishes and ⌘↩ steers it now. */
  const send = (how: "queue" | "steer" = "queue") => {
    const c = composer();
    const text = c?.value.trim() ?? "";
    const pi = threads().find((t) => t.id === selected())?.pi;
    const slash = /^(\/[a-z-]+)\s*(.*)$/i.exec(text);
    // The harness's own commands act on the selected session; typed in a
    // read-only fork they go to that fork's pi like any other line.
    if (slash && SLASH[slash[1].toLowerCase()] && c && pi && !pi.startsWith("btw-")) {
      c.value = "";
      fit(c);
      c.dispatchEvent(new Event("input", { bubbles: true })); // the composer's own state (completion) follows the value
      live.thread(pi).sent(text);
      SLASH[slash[1].toLowerCase()].run(slash[2]);
      return;
    }
    if (!c || !pi) return;
    const L = live.thread(pi);
    const atts = L.takeAttachments();
    const images = atts.map((i) => ({ type: "image", data: i.data, mimeType: i.mimeType }));
    if (!text && !images.length) return;
    c.value = "";
    fit(c);
    c.dispatchEvent(new Event("input", { bubbles: true }));
    L.sent(text, atts.map((i) => i.n));
    if (!pi.startsWith("btw-")) nameSession(pi, text);
    const cmd: Record<string, unknown> = { type: "prompt", message: text || "(see image)" };
    if (images.length) cmd.images = images;
    // Sent while it runs: a follow-up waits for the turn to end; a steer is
    // delivered before the next model call. The queue shows either until then.
    if (L.busy()) {
      cmd.streamingBehavior = how === "steer" ? "steer" : "followUp";
      L.queued(text, how);
    } else L.sent(text, atts.map((i) => i.n));
    void window.harness.pi(cmd, pi);
  };

  /** A shell opened by shortcut lands where the selection is, else the machine. */
  const scope = () => {
    const s = selected();
    const ws = machine().workspaces;
    return ws.find((w) => w.id === s || w.ephemerals.some((e) => e.id === s)) ? s : "machine";
  };

  /** Dragging the drawer's top edge resizes it inside the pane it lives in. */
  const startResize = (e: PointerEvent) => {
    e.preventDefault();
    const startY = e.clientY;
    const startH = height();
    const move = (ev: PointerEvent) =>
      setHeight(Math.max(120, Math.min(window.innerHeight - 220, startH + startY - ev.clientY)));
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  };

  return (
    <div class="relative grid h-full grid-rows-[35px_minmax(0,1fr)_22px]">
      <TitleBar machine={machine()} teams={TEAMS} teamId={teamId()} onSwitchTeam={switchTeam} onSearch={() => setPalette("go")} />
      <Confirm
        open={!!confirm()}
        title={`Delete ${sessions.find((x) => x.id === confirm()?.id)?.name ?? "this session"}?`}
        body={
          <>
            <p class="m-0">It still has work in progress. Deleting stops all of it and discards every message this session sent to the workspaces:</p>
            <ul class="mt-2 mb-0 list-disc pl-5 font-mono text-xs">
              <For each={confirm()?.items ?? []}>{(it) => <li class="truncate">{it}</li>}</For>
            </ul>
          </>
        }
        danger="Stop everything and delete"
        onYes={() => reallyDelete(confirm()!.id)}
        onNo={() => setConfirm(undefined)}
      />
      <Palette
        open={palette() !== undefined}
        mode={palette() === "commands" ? "commands" : "go"}
        items={palette() === "workspaces" ? placeItems() : goItems()}
        commands={commandItems()}
        onClose={() => setPalette(undefined)}
      />
      <Show when={find() !== undefined}>
        <div class="absolute top-[42px] right-4 z-30 flex h-8 items-center gap-2 rounded-md border border-widget-line bg-overlay px-2 shadow-overlay">
          <input
            id="find"
            class="w-56 bg-transparent font-mono text-sm outline-none placeholder:text-subtle"
            placeholder="find"
            value={find()}
            onInput={(e) => (setFind(e.currentTarget.value), void window.harness.find(e.currentTarget.value))}
            onKeyDown={(e) => {
              if (e.key === "Enter") void window.harness.find(find() ?? "", true);
              if (e.key === "Escape") closeFind();
            }}
          />
          <button class="text-subtle hover:text-fg" onClick={closeFind} title="Close (esc)"><Icon name="x" size={12} /></button>
        </div>
      </Show>

      <div
        class="grid min-h-0"
        style={{
          "grid-template-columns": `48px ${leftOpen() ? "280px " : ""}minmax(0,1fr)${inspector() ? " 300px" : ""}`,
        }}
      >
        <ActivityBar view={view()} panelOpen={leftOpen()} onView={pickView} onSettings={openSettings} owner={machine().owner} />
        <Show when={leftOpen() && view() === "repos"}>
          <ReposPanel repos={REPOS.filter((r) => r.teamId === teamId())} workspaces={machine().workspaces} />
        </Show>
        <Show when={leftOpen() && view() === "registries"}>
          <RegistriesPanel images={IMAGES.filter((i) => i.teamId === teamId())} />
        </Show>
        <Show when={leftOpen() && view() === "workspaces"}>
          <MachinePanel
            machine={machine()}
            team={TEAMS.find((t) => t.id === teamId())?.name ?? ""}
            sessions={live_()}
            busy={(id) => live.thread(id).busy()}
            onNewSession={newSession}
            onArchiveSession={archiveSession}
            onRestoreSession={restoreSession}
            onDeleteSession={deleteSession}
            onArchiveIdle={() => idleSessions().forEach((x) => archiveSession(x.id))}
            idle={idleSessions().length}
            archived={archived()}
            sides={sides}
            onCloseSide={removeSide}
            environment={environment()}
            environments={ENVIRONMENTS}
            selected={selected()}
            onSelect={showThread}
            onOpenEnv={() => setEnvTab(true)}
            onConnect={setConnected}
          />
        </Show>

        <div class="grid min-h-0 min-w-0" style={{ "grid-template-columns": panes.map(() => "minmax(0,1fr)").join(" ") }}>
          <For each={panes}>
            {(p, pi) => {
              const isActive = () => pi() === activePane();
              return (
            <div class="grid min-h-0 min-w-0" data-pane={pi()} classList={{ "border-l border-line": pi() > 0 }} on:pointerdown={{ handleEvent: () => setActivePane(pi()), capture: true }}>
            <Chat
              machine={machine()}
              team={TEAMS.find((t) => t.id === teamId())?.name ?? ""}
              env={isActive() && envTab() ? environment() : undefined}
              file={isActive() ? file() : undefined}
              onCloseFile={() => setFile(undefined)}
              task={isActive() ? (live.tasks.find((t) => t.id === taskId()) ?? asTask(live.procs.find((p) => p.id === taskId()))) : undefined}
              onCloseTask={() => setTaskId(undefined)}
              snapshots={SNAPSHOTS.filter((s) => s.environment === environment().name)}
              onCloseEnv={() => setEnvTab(false)}
              settings={isActive() && settingsTab()}
              onCloseSettings={() => setSettingsTab(false)}
              threads={threadsOf(p)}
              threadId={p.sel}
              onThread={(id) => (setActivePane(pi()), showThread(id))}
              onSwitch={() => setPalette("workspaces")}
              commands={slashItems()}
              inspectorOpen={rightOpen()}
              onToggleInspector={() => setRightOpen((v) => !v)}
              onCloseThread={closeThread}
              onDropTab={(id, index) => moveTab(id, pi(), index)}
              onSplit={panes.length < 2 && p.open.length > 1 ? splitRight : undefined}
              shell={
                isActive() && tabs().length ? (
                  <div
                    class="grid min-h-0 grid-rows-[3px_minmax(0,1fr)] border-t border-line"
                    style={{ height: maximised() ? "100%" : `${height()}px` }}
                  >
                    <div class="cursor-row-resize hover:bg-accent" onPointerDown={startResize} title="Drag to resize" />
                    <TerminalPanel
                      machine={machine()}
                      tabs={tabs()}
                      active={active()}
                      maximised={maximised()}
                      onActivate={setActive}
                      onOpen={openShell}
                      onCloseTab={closeTab}
                      onToggleMaximise={() => setMaximised((v) => !v)}
                      onClose={closePanel}
                    />
                  </div>
                ) : undefined
              }
            />
            </div>
              );
            }}
          </For>
        </div>

        {/* The secondary side bar: full height beside the editor area, one for
            the window, describing whatever the active pane has selected. */}
        <Show when={inspector()}>
          <Inspector
            machine={machine()}
            selected={selected()}
            onOpenShell={toggleShell}
            onOpenTask={(id) => (setEnvTab(false), setFile(undefined), setTaskId(id))}
            onOpenFile={(path, status) => {
              setEnvTab(false);
              setFile({ path, status });
            }}
          />
        </Show>
      </div>

      <StatusBar
        machine={machine()}
        shells={tabs().length}
        env={environment().name}
        leftOpen={leftOpen()}
        onToggleLeft={() => setLeftOpen((v) => !v)}
      />
    </div>
  );
}
