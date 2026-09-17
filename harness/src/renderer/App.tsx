import { For, Show, createEffect, createMemo, createSignal, onCleanup, onMount, untrack } from "solid-js";
import { createStore, produce, reconcile } from "solid-js/store";
import { TitleBar } from "./components/TitleBar";
import { MachinePanel } from "./components/MachinePanel";
import { ActivityBar, type Activity } from "./components/ActivityBar";
import { ReposPanel, RegistriesPanel } from "./components/TeamPanel";
import { Chat, fit } from "./components/Chat";
import { Inspector } from "./components/inspector/Inspector";
import { StatusBar } from "./components/StatusBar";
import { TerminalPanel } from "./components/terminal/TerminalPanel";
import { makeTab, nextIndex, reconcile as reconcileTabs, scopeOfTab, sessionIndex, sessionsOfTab, type TermTab } from "./components/terminal/tabs";
import { IMAGES, MACHINE, REPOS, threadOf, type Environment, type Snapshot, type Thread, type Workspace } from "./model";
import { LOADING, ipcError, toEnvironment, toSnapshot, toWorkspace } from "./platform";
import type { Team } from "../connect/bench";
import { KEYS, inTerminal, mayAct, threadIndex } from "./keys";
import { Palette, type PaletteItem } from "./components/Palette";
import { Confirm } from "./ui/Confirm";
import { Icon } from "./ui/Icon";
import * as live from "./live";
import { benchSessions, inFlightItems, openNote, openRoute, procState, refusal, type SessionRow } from "./rows";
import { shouldRefreshOn } from "./refresh";
import { cycleTheme } from "./theme";

export function App() {
  // A developer has exactly one bench per team, so switching team is what
  // switches machine: the real `auth.chooseTeam` reconnects, and leaving ready
  // reloads this page, so nothing here resets state by hand.
  const [teams, setTeams] = createSignal<Team[]>([]);
  const [teamId, setTeamId] = createSignal("");
  const [who, setWho] = createSignal("");
  const teamName = () => teams().find((t) => t.slug === teamId())?.name || teamId();
  // The team's real workspaces and environments, read by main from /v1. What the API has no field
  // for — the bench's goal and plan — stays empty rather than faked.
  const [workspaces, setWorkspaces] = createSignal<Workspace[]>([]);
  const [environments, setEnvironments] = createSignal<Environment[]>([]);
  const [snapshots, setSnapshots] = createSignal<Snapshot[]>([]);
  const [wsNote, setWsNote] = createSignal<string | undefined>(LOADING);
  const [envNote, setEnvNote] = createSignal<string | undefined>(LOADING);
  const machine = createMemo(() => ({ ...MACHINE, owner: who(), goal: "", todos: [], workspaces: workspaces() }));

  // Environments belong to the team, not the machine, and WHICH one this space follows is the
  // platform's answer (`/v1/me/environments`), not a choice this window keeps: every device and
  // every pod of the space reads the same row. Empty means the space follows nothing — never the
  // first environment in the list, which would show a connection nobody asked for.
  const [connected, setConnected] = createSignal("");
  const environment = createMemo(() => environments().find((e) => e.id === connected()));
  const envId = createMemo(() => environment()?.id);

  const platform = window.harness.platform;
  /** The open environment page reads its one environment fresh, then that volume's history. */
  const loadEnvPage = async (id: string) => {
    try {
      const e = toEnvironment(await platform.environment(id), teamId());
      setEnvironments((l) => l.map((x) => (x.id === e.id ? e : x)));
      setSnapshots(e.volume ? (await platform.snapshots(e.volume)).map((s) => toSnapshot(s, e.name)) : []);
    } catch (e) {
      setEnvNote(ipcError(e));
    }
  };
  // One read at a time: a focus landing mid-refresh is dropped, not queued.
  let refreshing = false;
  const refresh = async () => {
    if (refreshing || !teamId()) return;
    refreshing = true;
    try {
      await Promise.all([
        platform.workspaces().then((r) => (setWorkspaces(r.map(toWorkspace)), setWsNote(undefined)), (e) => setWsNote(ipcError(e))),
        platform.environments().then((r) => (setEnvironments(r.map((x) => toEnvironment(x, teamId()))), setEnvNote(undefined)), (e) => setEnvNote(ipcError(e))),
        // A read that fails leaves the last known choice rather than disconnecting the window.
        platform.myEnvironment().then((id) => setConnected(id ?? ""), () => undefined),
      ]);
      const id = envTab() ? envId() : undefined;
      if (id) await loadEnvPage(id);
    } finally {
      refreshing = false;
    }
  };
  void window.harness.auth.status().then((s) => {
    if (s.phase !== "ready") return;
    setWho(s.username);
    setTeamId(s.team);
    void refresh();
  });
  void window.harness.auth.teams().then(setTeams);
  // A team change reloads this page (leaving ready does), so only focus and the beat remain.
  const onFocus = () => void refresh();
  window.addEventListener("focus", onFocus);
  const beat = setInterval(() => document.visibilityState === "visible" && void refresh(), 10_000);
  // A shell exits after whatever it ran — an install, a `kl` verb — so the lists it may have moved
  // are re-read at once instead of at the next beat. The code does not matter: a failed command
  // can still have changed half of it.
  const offExit = window.harness.pty.onExit(() => void refresh());
  onCleanup(() => (window.removeEventListener("focus", onFocus), clearInterval(beat), offExit()));

  /** The space's environment is set on the platform first; the window shows what came back. */
  const connectTo = (id: string) =>
    void (id ? platform.setMyEnvironment(id) : platform.clearMyEnvironment()).then(refresh, (e) => setEnvNote(ipcError(e)));

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
  // Sessions are the bench's: the list lives in /bench/sessions.json on the
  // person's bench and every device is a view of it. Nothing here persists the
  // list; the cache for a cold, disconnected start is main's. The first live
  // session is the machine's own row and tab.
  type Session = SessionRow;
  const ARCHIVE_AFTER = 24 * 60 * 60 * 1000;
  const [sessions, setSessions] = createStore<Session[]>([]);
  const bench = window.harness.bench;
  const refreshSessions = async () => void setSessions(reconcile(await bench<Session[]>("GET", "/sessions")));
  const live_ = () => benchSessions(sessions).filter((x) => !x.archived);
  const archived = () => benchSessions(sessions).filter((x) => x.archived);
  createEffect(() => live.setSessionCount(live_().length));
  // Ids are what every event names a workspace by; the name is what a person reads.
  createEffect(() => live.setWorkspaceNames(workspaces()));
  /** A thread's own model, from the bench's session row — a workspace tab is not the bench. */
  const modelOf = (id: string) => (sessions.find((y) => y.id === id) as { model?: string } | undefined)?.model;
  const sessionThread = (id: string): Thread | undefined => {
    const x = sessions.find((y) => y.id === id);
    // The model is the SESSION's, from sessions.json: a window that opened after the child started
    // never saw pi's `started` event, and read its status ("not started") as the model's name.
    return x && { id, name: x.name, kind: "session", readonly: !live.connected(), messages: [], pi: id, model: (x as { model?: string }).model };
  };
  const fail = (e: Error) => live.thread(cur()).note(e.message);
  const loadThread = async (id: string) => live.thread(id).replay(await window.harness.benchMessages(id));
  // A workspace tab is its thread on the bench: open it there (idempotent),
  // then read its history. An ephemeral is watched, never driven: it only
  // reads. Offline or unwritable skips the open through refusal() and reads
  // what main cached, read-only.
  const openLive = (id: string) => {
    const t = threadOf(machine(), id);
    if (!t?.pi || (t.kind !== "workspace" && t.kind !== "ephemeral")) return;
    const w = machine().workspaces.find((x) => x.id === id || x.ephemerals.some((e) => e.id === id))!;
    const L = live.thread(t.pi);
    const route = openRoute(t.kind, w.id);
    const skip = !route || refusal({ type: "new_session" }, { session: t.pi, connected: live.connected(), writable: live.writable() });
    void (async () => {
      const sid = skip ? t.pi! : (await bench<Session>("POST", route)).id;
      if (sid !== t.pi) return L.note(`the bench opened ${sid}, not ${t.pi}`);
      await loadThread(sid);
    })().catch((e: Error) => L.note(openNote(e.message)));
  };
  const newSession = () =>
    void bench<Session>("POST", "/sessions").then(async (s) => {
      await refreshSessions();
      await loadThread(s.id);
      showThread(s.id);
    }, fail);
  /** Archive: the bench stops its pi, the tab closes, the row folds away; nothing is lost. */
  const archiveSession = (id: string) =>
    void bench("POST", `/sessions/${id}/archive`).then(() => {
      sides.filter((t) => t.session === id).forEach((t) => removeSide(t.id));
      if (paneOf(id) >= 0) closeThread(id);
      return refreshSessions();
    }, fail);
  const restoreSession = (id: string) =>
    void bench("POST", `/sessions/${id}/restore`).then(async () => {
      await refreshSessions();
      await loadThread(id);
      showThread(id);
    }, fail);
  const [confirm, setConfirm] = createSignal<{ id: string; items: string[] } | undefined>();
  const idleSessions = () => live_().filter((x) => x.lastActive && Date.now() - x.lastActive > ARCHIVE_AFTER && x.id !== cur());
  /** Delete asks the bench; what it says is in flight comes back as the confirm list. */
  const deleteSession = (id: string) =>
    void bench("DELETE", `/sessions/${id}`, { stop: false }).then(() => afterDelete(id), (e: Error) => {
      const items = inFlightItems(e.message);
      if (items) setConfirm({ id, items });
      else fail(e);
    });
  const reallyDelete = (id: string) => {
    setConfirm(undefined);
    void bench("DELETE", `/sessions/${id}`, { stop: true }).then(() => afterDelete(id), fail);
  };
  const afterDelete = (id: string) => {
    sides.filter((t) => t.session === id).forEach((t) => removeSide(t.id));
    live.discard(id);
    if (paneOf(id) >= 0) closeThread(id);
    void refreshSessions();
  };
  // Side sessions (`/btw`): read-only forks of a session, each on its own pi.
  // Their messages come from their own live state.
  const [sides, setSides] = createStore<Thread[]>([]);
  let sideSeq = 0;
  const threadsOf = (p: Pane) =>
    p.open
      .map((id) => threadOf(machine(), id) ?? sessionThread(id) ?? sides.find((t) => t.id === id))
      .filter((t) => t !== undefined)
      .map((t) => (t.kind === "machine" ? { ...t, pi: live_()[0]?.id ?? "", readonly: !live.connected() } : t))
      // Every thread carries its own session's model: a workspace tab is not the bench, and
      // reading only the bench's row showed "no model" in one (owner, 2026-09-17).
      .map((t) => (t.pi ? { ...t, messages: live.thread(t.pi).messages, model: t.model ?? modelOf(t.pi) } : t));
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
      is kept until then; the bench's answer file stays. */
  const removeSide = (id: string) => {
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
  // Which settings section to land on; a fresh object each time so asking for the same section twice still moves there.
  const [settingsPage, setSettingsPage] = createSignal<{ id: string }>();
  const openSettings = (page?: string) => {
    setEnvTabRaw(false);
    setFile(undefined);
    if (page) setSettingsPage({ id: page });
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
    p && { id: p.id, session: p.session ?? "", tool: "Process", arg: `${p.name} · ${p.command}`, state: procState(p), started: p.started, ended: p.ended, output: p.tail };
  const [taskId, setTaskId] = createSignal<string | undefined>();
  const inspector = () => rightOpen() && !envTab() && !settingsTab();

  const switchTeam = (slug: string) => void window.harness.auth.chooseTeam(slug);
  // Opening the environment page, or switching which one, reads it fresh. Keyed on the id memo, so
  // the page's own write back into the list does not re-run this.
  createEffect(() => {
    const id = envTab() ? envId() : undefined;
    if (id) untrack(() => (setSnapshots([]), void loadEnvPage(id)));
  });

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
  // The drawer hides; it never closes what is in it. A shell is a process somebody may have left
  // running, so putting the panel away keeps every tab mounted (display:none) and only the tab's
  // own × ends its shell (owner, 2026-09-16: "every time I'm closing shell it's closing the session").
  const [drawer, setDrawer] = createSignal(true);

  // The terminals of the tab the panel is showing. All stay mounted; only these
  // are visible, so switching tabs never drops a socket.
  const tabsHere = () => tabs().filter((t) => t.owner === selected());
  // Which session tabs have had their scope's tmux listed at least once. The 5 s
  // reconcile below is the truth after that.
  const listed = new Set<string>();
  // Owners with a `pty:open` in flight: a session that was just asked for is not
  // listed yet, and reconciling against that listing would close the new tab.
  const opening = new Set<string>();

  /**
   * A terminal belongs to the session tab it was opened from, and its scope is
   * that tab's — there is nothing to pick. The first open of a tab adopts
   * whatever tmux already holds under this tab's name (another device, an
   * earlier run of the app) instead of forking a second shell beside it.
   */
  const openShell = async (owner = selected()) => {
    const scopeId = scopeOfTab(machine(), owner);
    setDrawer(true);
    opening.add(owner);
    try {
      const live = await window.harness.pty.sessions(scopeId).catch(() => []);
      const names = live.map((s) => s.name);
      if (!listed.has(owner)) {
        listed.add(owner);
        const mine = sessionsOfTab(names, owner);
        if (mine.length) {
          const made = mine.map((n) => makeTab(machine(), teamName(), owner, scopeId, sessionIndex(n, owner)));
          setTabs((ts) => [...ts, ...made]);
          setActive(made[made.length - 1].id);
          return;
        }
      }
      // A free index in BOTH the listing and the open tabs, so "+" never lands
      // on a session somebody else's device is already holding.
      const taken = [...names, ...tabs().filter((x) => x.owner === owner).map((x) => x.session)];
      const t = makeTab(machine(), teamName(), owner, scopeId, nextIndex(taken, owner));
      setTabs((ts) => [...ts, t]);
      setActive(t.id);
    } finally {
      opening.delete(owner);
    }
  };
  /** Drop a tab from the window. The view unmounts, which closes its socket; tmux is untouched. */
  const dropTab = (id: string) => {
    const rest = tabs().filter((t) => t.id !== id);
    setTabs(rest);
    if (rest.length === 0) setMaximised(false);
    else if (active() === id) setActive(rest[rest.length - 1].id);
  };
  // The x ends the shell for good — tmux kill-session on the far side — because
  // a person closing a terminal means it, while a dropped socket never does.
  const closeTab = (id: string) => {
    void window.harness.pty.kill(id);
    dropTab(id);
  };

  /**
   * Tabs mirror tmux sessions both ways: one opened on another device shows up
   * here, and one killed there takes its tab with it. Listing is the only
   * evidence — a tab is never killed by this, only dropped.
   */
  const syncTabs = async () => {
    const cur = tabs().find((t) => t.id === active());
    if (!cur || opening.has(cur.owner)) return;
    const live = await window.harness.pty.sessions(cur.scope).catch(() => undefined);
    if (!live || opening.has(cur.owner)) return;
    const { add, remove } = reconcileTabs(tabs(), live.map((s) => s.name), cur.owner, Date.now());
    if (!add.length && !remove.length) return;
    const made = add.map((n) => makeTab(machine(), teamName(), cur.owner, cur.scope, sessionIndex(n, cur.owner)));
    setTabs((ts) => [...ts.filter((t) => !remove.includes(t.id)), ...made]);
    const rest = tabs();
    if (!rest.some((t) => t.id === active())) setActive(rest[rest.length - 1]?.id ?? "");
    if (rest.length === 0) setMaximised(false);
  };
  onMount(() => {
    const beat = setInterval(() => void syncTabs(), 5_000);
    onCleanup(() => clearInterval(beat));
  });
  // On a tab switch, and the moment the bench comes back: the listing is stale
  // exactly when nobody was watching it.
  createEffect(() => (active(), live.connected(), void untrack(() => syncTabs())));

  const closePanel = () => {
    setDrawer(false);
    setMaximised(false);
  };
  const shellShown = () => tabsHere().length > 0 && drawer();
  /** The Shell button and ⌘J are one gesture: open a shell here, bring the drawer back, or put it away. */
  const toggleShell = () => void (shellShown() ? closePanel() : tabsHere().length ? setDrawer(true) : openShell());

  /** Back out of one layer at a time: a file, then the environment, then a
      maximised shell, then the shell. Nothing else swallows escape. */
  const back = () => {
    if (file()) setFile(undefined);
    else if (taskId()) setTaskId(undefined);
    else if (envTab()) setEnvTab(false);
    else if (settingsTab()) setSettingsTab(false);
    else if (maximised()) setMaximised(false);
    else if (shellShown()) closePanel();
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
    openLive(id);
    setEnvTab(false);
    setSettingsTab(false);
    setFile(undefined);
    setSelected(id);
    queueMicrotask(() => composer()?.focus());
  };
  const goTo = showThread;
  // The selected session's pi and live state: what every command acts on.
  const cur = () => threads().find((t) => t.id === selected())?.pi ?? live_()[0]?.id ?? "";
  const L = () => live.thread(cur());
  // The plan is the session's own: the panel follows whichever thread is being read. It sits here
  // rather than on `machine` above because the active thread is only known this far down.
  const planned = createMemo(() => ({ ...machine(), todos: live.planOf(cur()) }));
  /** Every pi call the harness makes goes through here: a refusal or a failure is a note, never silence. */
  const pi = (cmd: Record<string, unknown> & { type: string }, id = cur()) => {
    const why = refusal(cmd, { session: id, connected: live.connected(), writable: live.writable() });
    if (why) return void live.thread(id).note(why);
    return window.harness.pi(cmd, id).then(
      (r) => (r.success === false ? (live.thread(id).note(String(r.error)), undefined) : r),
      (e: Error) => void live.thread(id).note(e.message),
    );
  };
  const placeItems = createMemo<PaletteItem[]>(() => {
    const m = machine();
    const out: PaletteItem[] = [{ id: m.id, label: "Bench Thread", detail: m.goal, kind: "machine", icon: "machine", run: () => goTo(m.id) }];
    for (const x of live_().slice(1)) out.push({ id: x.id, label: x.name, detail: x.id, kind: "session", icon: "thread", run: () => goTo(x.id) });
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
    const env = environment();
    for (const s of env?.services ?? [])
      for (const p of s.ports)
        if (p.url) out.push({ id: `${s.name}:${p.port}`, label: `${s.name}:${p.port}`, detail: env!.name, kind: "service", icon: "globe", run: () => window.harness.openPreview(p.url!, `${env!.name} · ${s.name}:${p.port}`) });
    return out;
  });
  const commandItems = createMemo<PaletteItem[]>(() => [
    // Suggested: what a person does next. Session: what they do to this conversation.
    { id: "mode", group: "Suggested", label: `Mode: ${live.mode()} (cycle)`, keys: KEYS.mode.keys, run: () => {
      const next = live.MODES[(live.MODES.indexOf(live.mode()) + 1) % live.MODES.length];
      live.setMode(next);
      void pi({ type: "prompt", message: `/mode ${next === "plan" ? "plan" : "build"}` })?.catch(() => undefined);
    } },
    { id: "level", group: "Suggested", label: `Thinking level: ${live.level()}`, keys: KEYS.level.keys, run: () => {
      const next = live.LEVELS[(live.LEVELS.indexOf(live.level()) + 1) % live.LEVELS.length];
      live.noteLevel(next);
      void pi({ type: "set_thinking_level", level: next })?.catch(() => undefined);
    } },
    { id: "composer", group: "Suggested", label: "Focus the prompt", keys: KEYS.composer.keys, run: () => composer()?.focus() },
    { id: "shell", group: "Suggested", label: shellShown() ? "Hide the shell" : tabsHere().length ? "Show the shell" : "Open a shell", keys: KEYS.shell.keys, run: () => toggleShell() },
    { id: "env", group: "Suggested", label: envTab() ? "Close the environment" : "Open the environment", keys: KEYS.environment.keys, run: () => (setFile(undefined), setEnvTab((v) => !v)) },
    { id: "panel", group: "Suggested", label: leftOpen() ? "Hide workspaces" : "Show workspaces", keys: KEYS.panel.keys, run: () => setLeftOpen((v) => !v) },
    { id: "inspector", group: "Suggested", label: rightOpen() ? "Hide the inspector" : "Show the inspector", keys: KEYS.inspector.keys, run: () => setRightOpen((v) => !v) },
    { id: "workspaces", group: "Suggested", label: "Switch workspace…", keys: KEYS.workspaces.keys, run: () => setPalette("workspaces") },
    { id: "go", group: "Suggested", label: "Go to…", keys: KEYS.quickOpen.keys, run: () => setPalette("go") },
    { id: "settings", group: "Suggested", label: "Settings", keys: KEYS.settings.keys, run: () => openSettings() },
    { id: "bg", group: "Session", label: "Send the running command to the background", keys: KEYS.background.keys, run: () => void pi({ type: "prompt", message: "/bg" }) },
    { id: "abort", group: "Session", label: "Stop this session", run: () => void pi({ type: "abort" }) },
    { id: "newSession", group: "Session", label: "New session", run: newSession },
    { id: "benchImport", group: "Session", label: "Import this laptop's sessions into the bench", run: () => {
      if (!live.connected() || !live.writable().ok) return void live.thread(cur()).note("not connected to the bench; nothing was sent");
      const raw = localStorage.getItem("harness.sessions");
      if (!raw) return void live.thread(cur()).note("nothing to import: this laptop has no local session list");
      void window.harness.benchImport(JSON.parse(raw) as { id: string; name: string; seq: number; lastActive?: number; archived?: boolean }[]).then((r) => {
        localStorage.setItem("harness.sessions.imported", String(Date.now()));
        live.thread(cur()).note(r.added.length ? `imported ${r.added.length} sessions and ${r.files} files` : "already imported: the bench has every session");
        return refreshSessions();
      }, (e: Error) => live.thread(cur()).note(e.message));
    } },
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
    ...teams().filter((t) => t.slug !== teamId() && t.region).map((t) => ({ id: `team:${t.slug}`, label: `Switch to ${t.name || t.slug}`, run: () => switchTeam(t.slug) })),
    ...environments().filter((e) => e.id !== environment()?.id).map((e) => ({ id: `env:${e.id}`, label: `Connect to ${e.name}`, run: () => connectTo(e.id) })),
  ]);

  const onKey = (e: KeyboardEvent) => {
    // A terminal owns the keyboard while it has focus: nothing here may take a key from it except
    // a chord it cannot mean. Everything else reaches the PTY, once, through xterm alone.
    if (!mayAct(e, inTerminal(e.target))) return;
    const hit = (b: { match: (e: KeyboardEvent) => boolean }) => b.match(e);
    const stop = () => e.preventDefault();

    if (hit(KEYS.steer)) return (stop(), send("steer"));
    if (hit(KEYS.send)) return (stop(), send());
    if (hit(KEYS.background)) return (stop(), void pi({ type: "prompt", message: "/bg" }));
    if (hit(KEYS.split)) return (stop(), splitRight());
    if (hit(KEYS.focusPane)) return (stop(), void setActivePane((p) => (p + 1) % panes.length));
    if (hit(KEYS.commands) || hit(KEYS.palette)) return (stop(), void setPalette("commands"));
    // Build ⇄ Plan. PLAN is read-only: the platform tools that change something are turned off in
    // pi itself, so a plan cannot quietly become a change (§16b's `tab`).
    if (hit(KEYS.mode)) {
      stop();
      // build → plan → accept-edits → build, as Claude Code's shift+tab cycles.
      const next = live.MODES[(live.MODES.indexOf(live.mode()) + 1) % live.MODES.length];
      live.setMode(next);
      // The extension owns which tools are live; accept-edits is the desktop's own answer policy,
      // so pi is told plan or build and nothing else.
      void pi({ type: "prompt", message: `/mode ${next === "plan" ? "plan" : "build"}` })?.catch(() => undefined);
      return;
    }
    // How hard the model thinks, cycled: pi's own `set_thinking_level`.
    if (hit(KEYS.level)) {
      stop();
      const next = live.LEVELS[(live.LEVELS.indexOf(live.level()) + 1) % live.LEVELS.length];
      live.noteLevel(next);
      void pi({ type: "set_thinking_level", level: next })?.catch(() => undefined);
      return;
    }
    if (hit(KEYS.quickOpen)) return (stop(), void setPalette("go"));
    if (hit(KEYS.workspaces)) return (stop(), void setPalette("workspaces"));
    if (hit(KEYS.find)) return (stop(), openFind());
    if (hit(KEYS.settings)) return (stop(), openSettings());
    if (palette()) return;
    if (hit(KEYS.back) && find() !== undefined) return closeFind();

    if (hit(KEYS.shell)) return (stop(), toggleShell());
    if (hit(KEYS.environment)) return (stop(), setFile(undefined), void setEnvTab((v) => !v));
    if (hit(KEYS.inspector)) return (stop(), void setRightOpen((v) => !v));
    if (hit(KEYS.panel)) return (stop(), void setLeftOpen((v) => !v));
    if (hit(KEYS.prevThread)) return (stop(), cycleThread(-1));
    if (hit(KEYS.nextThread)) return (stop(), cycleThread(1));
    if (hit(KEYS.close)) return (stop(), closeCurrent());
    // The composer's own hint promises "esc to interrupt": a running turn is
    // stopped first; only an idle session treats Escape as back.
    if (hit(KEYS.back) && L().busy()) return (stop(), void pi({ type: "abort" }));
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

  // The bench is live: session events and the bench's own changes land here.
  window.harness.onPi((ev) => {
    live.onEvent(ev);
    if (shouldRefreshOn(ev)) void refresh();
    if (ev.type === "sessions") void refreshSessions().catch(() => undefined);
    // After a reconnect, every open session pages in what it missed.
    if (ev.type === "bench:resync") void refreshSessions().then(() => Promise.all(live_().map((x) => loadThread(x.id))), () => undefined);
  });
  void window.harness.benchState().then(async (st) => {
    live.setConnected(st.connected);
    if (!st.configured) return void live.thread("bench").note("not connected to your bench yet");
    // Cold and offline: the cached list and messages, read-only until connected.
    setSessions(reconcile(st.sessions as Session[]));
    // The asks already in flight: a window opened mid-conversation shows the queue, not a blank.
    live.seedExchanges(st.exchanges ?? []);
    for (const x of live_()) void loadThread(x.id);
    if (!st.connected) return;
    // ONE request for everything a window needs to open: six separate ones each paid a TLS
    // handshake at the edge (measured 2026-09-17), and the bench's own answers were never the
    // slow part. Live updates still arrive over the events socket.
    const boot = (await window.harness.benchBootstrap(cur()).catch(() => undefined)) as
      | { sessions?: Session[]; plans?: { session: string; items: unknown[] }[]; procs?: unknown[]; tasks?: { row?: unknown }[]; exchanges?: unknown[]; messages?: { messages: unknown[] } }
      | undefined;
    if (boot?.sessions) setSessions(reconcile(boot.sessions));
    for (const r of boot?.plans ?? []) live.onEvent({ type: "plan", ...r });
    if (boot?.procs) live.onEvent({ type: "procs", rows: boot.procs });
    for (const row of boot?.tasks ?? []) live.onEvent({ type: "task", row });
    if (boot?.exchanges) live.seedExchanges(boot.exchanges);
    if (boot?.messages) live.thread(cur()).replay(boot.messages.messages);
    if (boot) return;
    await refreshSessions().catch(fail);
    // These three are VIEWS the bench also pushes as events: a first read that fails (an older
    // main with a narrower allow-list, a bench mid-restart) must not put an error in somebody's
    // conversation — the next event fills them in. Only what a person ASKED for reports failure.
    const quiet = () => undefined;
    void bench<Record<string, unknown>[]>("GET", "/procs").then((rows) => live.onEvent({ type: "procs", rows }), quiet);
    void bench<{ session: string; items: unknown[] }[]>("GET", "/plans").then((rows) => rows.forEach((r) => live.onEvent({ type: "plan", ...r })), quiet);
    void bench<Record<string, unknown>[]>("GET", "/tasks").then((rows) => rows.forEach((row) => live.onEvent({ type: "task", row })), quiet);
  });
  // Slash commands the harness answers itself, before anything reaches pi;
  // what is not listed here (/bg, /cancel, /skill:…) goes through.
  // `local` entries never reach the bench, so they run offline; the rest are refused first, not echoed.
  const SLASH: Record<string, { help: string; local?: true; run: (arg: string) => void }> = {
    // Abort first: pi refuses a new session mid-turn, and a refusal nobody sees reads as "/clear
    // does nothing" (owner, 2026-09-17). The transcript is emptied only once pi says it happened.
    "/clear": { help: "start this session afresh; the old one stays on disk", run: () => {
      const L = live.thread(cur());
      void (L.busy() ? pi({ type: "abort" })?.catch(() => undefined) : Promise.resolve())
        ?.then(() => pi({ type: "new_session" }))
        ?.then((r) => {
          if (r?.success === false) return L.note(`clear refused: ${String(r.error ?? "pi would not start a new session")}`);
          L.replay([]);
          L.note("new session");
        })
        ?.catch((e: Error) => L.note(e.message));
    } },
    "/new": { help: "open another session beside this one", run: newSession },
    "/compact": { help: "summarise the older part of this session", run: () => void pi({ type: "compact" }) },
    "/abort": { help: "stop what this session is doing", run: () => void pi({ type: "abort" }) },
    "/model": { help: "switch model: /model provider/id", run: (arg) => { const [provider, modelId] = arg.split("/"); if (provider && modelId) void pi({ type: "set_model", provider, modelId }); else L().note("usage: /model provider/id"); } },
    "/settings": { help: "open settings", local: true, run: () => openSettings() },
    "/btw": {
      help: "ask one question of a read-only fork of this session: /btw <question>",
      run: (arg) => {
        const from = cur();
        if (!arg.trim()) return L().note("usage: /btw <question> — one question, one answer, nothing changed");
        // The fork's read tools run on the bench pod: on a workspace thread they would read the wrong machine.
        if (/^[we]-/.test(from ?? "")) return L().note("btw is only for bench sessions");
        const id = `btw-${++sideSeq}`;
        setSides((ts) => [...ts, { id, name: `btw · ${arg.slice(0, 40)}`, kind: "btw", readonly: true, messages: [], pi: id, session: from }]);
        // Beside the session when there is room for a second pane, else a tab.
        if (panes.length < 2) {
          setPanes(produce((ps) => void ps.push({ open: [id], sel: id })));
          setActivePane(panes.length - 1);
        } else showThread(id);
        const side = live.thread(id);
        side.replay([]);
        side.note("a read-only fork of this session on the bench · one answer");
        side.sent(arg);
        side.setStatus("answering…");
        // No streaming: the bench answers when the fork is done, under its own
        // btw id, so the local id only names the tab.
        void bench<{ id: string; entries: unknown[] }>("POST", `/sessions/${from}/btw`, { question: arg }).then(
          (a) => {
            side.replay(a.entries);
            side.setStatus("answered");
          },
          (e: Error) => (side.note(e.message), side.setStatus("no answer")),
        );
      },
    },
    "/help": { help: "this list", local: true, run: () => L().note(Object.entries(SLASH).map(([k, v]) => `${k.padEnd(10)} ${v.help}`).join("\n") + "\n/bg        send the running command to the background (^B)") },
  };

  /** Everything a `/` can start: the harness's own, pi's, and each enabled skill. */
  const slashItems = createMemo(() => [
    ...Object.entries(SLASH).map(([name, v]) => ({ name, help: v.help })),
    { name: "/bg", help: "send the running command to the background (^B)" },
    { name: "/cancel", help: "kill a running or backgrounded command: /cancel #N" },
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
    const entry = slash && SLASH[slash[1].toLowerCase()];
    // /btw posts through the bench REST and needs it up too, but writes nothing pi-side.
    const why = entry?.local ? undefined : refusal({ type: entry && slash![1].toLowerCase() === "/btw" ? "get_state" : "prompt" }, { session: pi, connected: live.connected(), writable: live.writable() });
    if (why && c) return void live.thread(pi ?? "").note(why);
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
    const cmd: Record<string, unknown> = { type: "prompt", message: text || "(see image)" };
    if (images.length) cmd.images = images;
    // A PERSON's message mid-turn is a steer, and is announced as one: the model sees that they
    // said something while it was working and answers it in the same turn, rather than finding it
    // at the back of a queue behind three agent reports (§17.4). Asks and replies stay follow-ups.
    if (L.busy()) {
      cmd.streamingBehavior = "steer";
      cmd.message = `The person sent a new message while you were working:\n${text || "(see image)"}`;
      L.queued(text, how);
    } else L.sent(text, atts.map((i) => i.n));
    void window.harness.pi(cmd, pi).then((r) => void (r.success === false && L.note(String(r.error))), (e: Error) => L.note(e.message));
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
      <TitleBar machine={machine()} teams={teams()} team={teamId()} onSwitchTeam={switchTeam} onSearch={() => setPalette("go")} />
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
        <ActivityBar view={view()} panelOpen={leftOpen()} onView={pickView} onSettings={() => openSettings()} onProfile={() => openSettings("account")} owner={machine().owner} />
        <Show when={leftOpen() && view() === "repos"}>
          <ReposPanel repos={REPOS.filter((r) => r.teamId === teamId())} workspaces={machine().workspaces} />
        </Show>
        <Show when={leftOpen() && view() === "registries"}>
          <RegistriesPanel images={IMAGES.filter((i) => i.teamId === teamId())} />
        </Show>
        <Show when={leftOpen() && view() === "workspaces"}>
          <MachinePanel
            machine={machine()}
            team={teamName()}
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
            environments={environments()}
            wsNote={wsNote()}
            envNote={envNote()}
            selected={selected()}
            onSelect={showThread}
            onOpenEnv={() => setEnvTab(true)}
            onConnect={connectTo}
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
              team={teamName()}
              env={isActive() && envTab() ? environment() : undefined}
              file={isActive() ? file() : undefined}
              onCloseFile={() => setFile(undefined)}
              task={isActive() ? (live.tasks.find((t) => t.id === taskId()) ?? asTask(live.procs.find((p) => p.id === taskId()))) : undefined}
              onCloseTask={() => setTaskId(undefined)}
              snapshots={snapshots()}
              onCloseEnv={() => setEnvTab(false)}
              settings={isActive() && settingsTab()}
              settingsPage={settingsPage()}
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
              shellFull={isActive() && maximised() && drawer() && tabsHere().length > 0}
              shell={
                isActive() && tabsHere().length ? (
                  <div
                    class="grid min-h-0 grid-rows-[3px_minmax(0,1fr)] border-t border-line"
                    style={{ height: maximised() ? "100%" : `${height()}px`, display: drawer() ? undefined : "none" }}
                  >
                    <div class="cursor-row-resize hover:bg-accent" onPointerDown={startResize} title="Drag to resize" />
                    <TerminalPanel
                      tabs={tabsHere()}
                      active={active()}
                      maximised={maximised()}
                      onActivate={setActive}
                      onOpen={() => void openShell()}
                      onCloseTab={closeTab}
                      onEnded={dropTab}
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
            machine={planned()}
            selected={selected()}
            onOpenShell={() => toggleShell()}
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
        shells={tabsHere().length}
        env={environment()?.name ?? "no environment"}
      />
    </div>
  );
}
