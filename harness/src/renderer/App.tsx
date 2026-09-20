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
import { makeTab, nextIndex, scopeOfTab, type TermTab } from "./components/terminal/tabs";
import { IMAGES, MACHINE, threadOf, type Repo, type Environment, type Snapshot, type Thread, type Workspace } from "./model";
import { LOADING, ipcError, toEnvironment, toRepo, toSnapshot, toWorkspace } from "./platform";
import type { Team } from "../connect/bench";
import { cycleMotion, motionChoice } from "./components/Motion";
import { playDemo, wantsDemo } from "./demo";
import { KEYS, LEADER, LEADER_FORGET_MS, inTerminal, keyHint, leaderIndex, mayAct, threadIndex, underLeader } from "./keys";
import { Palette, type PaletteItem } from "./components/Palette";
import { Confirm } from "./ui/Confirm";
import { Icon } from "./ui/Icon";
import * as live from "./live";
import { agentRows, benchSessions, displayModel, inFlightItems, noteModelNames, openNote, openRoute, procState, refusal, type SessionRow } from "./rows";
import { shouldRefreshOn } from "./refresh";
import { cycleTheme } from "./theme";
import { collectOperationEventPages, configureOperationBridge, isTerminalOperation, type OperationProjection, type OperationRendererBridge } from "./operations/index.ts";

export function App() {
  // No operation-changed event exists on the `onPi` stream yet (main only pushes `bench` and
  // `bench:resync`), so `watch` polls the event log itself rather than inventing an IPC channel
  // this lane may not add. Bounded to a running/waiting operation only: the store's own `catchUp`
  // is a no-op once the fetched cursor is not ahead, so this just costs one idle events request
  // per tick until the projection reaches a terminal state, when the poll clears itself.
  const OPERATION_POLL_MS = 2_000;
  const operationBridge: OperationRendererBridge = {
    loadSnapshot: window.harness.operations.snapshot,
    loadEvents: (operationId, afterSequence) => collectOperationEventPages(
      (after) => window.harness.operations.events(operationId, after, 200),
      afterSequence,
    ),
    watch: (operationId, onChanged) => {
      const timer = setInterval(() => {
        const state = operations?.entries().find((entry) => entry.operationId === operationId)?.view().state;
        if (state !== undefined && isTerminalOperation(state)) return clearInterval(timer);
        // MAX_SAFE_INTEGER cannot spin the store: `catchUp` only ever queues one in-flight fetch
        // (`pendingNotification`) and clears that flag before requeuing on completion, so a tick
        // arriving mid-fetch coalesces into the next one rather than stacking.
        onChanged(Number.MAX_SAFE_INTEGER);
      }, OPERATION_POLL_MS);
      return () => clearInterval(timer);
    },
    // Every control refreshes the view once the write lands, so a granted decision stops
    // offering Approve rather than waiting on the next poll tick or reconnect.
    decide: (payload) => window.harness.operations.decision(payload).then(() => refreshOperation(payload.operationId)),
    answer: (payload) => window.harness.operations.input(payload).then(() => refreshOperation(payload.operationId)),
    cancel: (payload) => window.harness.operations.cancel(payload).then(() => refreshOperation(payload.operationId)),
  };
  const refreshOperation = (operationId: string) => {
    void operations?.entries().find((entry) => entry.operationId === operationId)?.reload();
  };
  const operations = configureOperationBridge(operationBridge);
  onCleanup(() => operations?.dispose());
  // A developer has exactly one bench per team, so switching team is what
  // switches machine: the real `auth.chooseTeam` reconnects, and leaving ready
  // reloads this page, so nothing here resets state by hand.
  const [teams, setTeams] = createSignal<Team[]>([]);
  const [teamId, setTeamId] = createSignal("");
  const [who, setWho] = createSignal("");
  const bootTest = new URLSearchParams(location.search).has("boot-test");
  const teamName = () => teams().find((t) => t.slug === teamId())?.name || teamId();
  // The team's real workspaces and environments, read by main from /v1. What the API has no field
  // for — the bench's goal and plan — stays empty rather than faked.
  const [workspaces, setWorkspaces] = createSignal<Workspace[]>([]);
  // The team's repos, from `GET /v1/repos`. Empty until the first read answers: an empty list is
  // "this team has none", and a FAILED read says so in the footer rather than showing a fixture.
  const [repos, setRepos] = createSignal<Repo[]>([]);
  const [environments, setEnvironments] = createSignal<Environment[]>([]);
  const [snapshots, setSnapshots] = createSignal<Snapshot[]>([]);
  const [wsNote, setWsNote] = createSignal<string | undefined>(LOADING);
  const [envNote, setEnvNote] = createSignal<string | undefined>(LOADING);
  if (bootTest) {
    setTeams([{ slug: "boot-team", name: "Boot Team", region: "boot", personal: true }]);
    setWho("boot-user");
    setTeamId("boot-team");
    setWorkspaces([{
      id: "boot-workspace", name: "boot-workspace", repo: "boot-repo", branch: "main", state: "running",
      queue: [], packages: [], ephemerals: [], files: [], changes: [],
    }]);
    setWsNote(undefined);
    setEnvNote(undefined);
  }
  /**
   * The workspaces, with each one's AGENTS nested under it (spec §4.5). An agent works in a tree of
   * the workspace now, not in a clone of it, so its row comes from the bench's own session list
   * rather than from a second `/v1` workspace whose name happened to end in `-eph-`.
   */
  // Declared before `machine` below reads it: a memo runs at creation, and a `const` read before
  // its line threw at mount and left the sign-in splash on screen (18 Sep).
  type Session = SessionRow;
  const [sessions, setSessions] = createStore<Session[]>([]);
  const machine = createMemo(() => {
    const agents = agentRows(sessions, live.exchanges);
    return {
      ...MACHINE,
      owner: who(),
      goal: "",
      todos: [],
      workspaces: workspaces().map((w) => ({
        ...w,
        ephemerals: agents
          .filter((a) => a.workspace === w.id)
          .map((a) => ({ id: a.id, task: a.task, agent: a.agent, state: a.state, started: "", changes: [] })),
      })),
    };
  });

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
        // NOT on the beat: `/v1/repos` authenticates with `caller()`, a SESSION JWT only, and the
        // desktop holds a CLI token — `identify` refuses those deliberately (`crates/api/src/lib.rs`
        // :541), so every call 401s. Polling it signed the owner out once a beat. The panel stays
        // empty until the api accepts a CLI login there (`identify_or_cli`, as /v1/workspaces does).
        // ponytail: one route away — re-enable this line when it does.
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
    if (bootTest) return;
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
  // Opening the bench opens the session you were last in — there is no thread above the sessions.
  const first = (!hashView.startsWith("settings") && hashView.split("/")[0]) || "";
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
  const ARCHIVE_AFTER = 24 * 60 * 60 * 1000;
  const bench = window.harness.bench;
  const refreshSessions = async () => void setSessions(reconcile(await bench<Session[]>("GET", "/sessions")));
  const live_ = () => benchSessions(sessions).filter((x) => !x.archived);
  const archived = () => benchSessions(sessions).filter((x) => x.archived);
  createEffect(() => live.setSessionCount(live_().length));
  // Ids are what every event names a workspace by; the name is what a person reads.
  createEffect(() => live.setWorkspaceNames(workspaces()));
  /** A thread's own model, from the bench's session row — a workspace tab is not the bench. */
  const modelOf = (id: string) => (sessions.find((y) => y.id === id) as { model?: string } | undefined)?.model;
  /** The session's remembered thinking and effort, from the same row (spec §1.2). */
  const tripleOf = (id: string) => {
    const r = sessions.find((y) => y.id === id) as { thinking?: string; effort?: string } | undefined;
    return { thinking: r?.thinking, effort: r?.effort };
  };
  // A turn stamps the triple that was in force when it ENDED, so live.ts needs it by session id.
  // The same pass draws a DIVIDER when a pick actually lands — derived from the bench's own rows,
  // never a message: what changed is worth reading back, the keypress that changed it is not
  // (owner: "if we need to show something show properly like model changed etc").
  createEffect(() => {
    for (const r of sessions) {
      const row = r as { id: string; model?: string; thinking?: string; effort?: string };
      live.noteTriple(row.id, { model: row.model, thinking: row.thinking, effort: row.effort }, row.model ? displayModel(row.model) : undefined);
    }
  });
  const sessionThread = (id: string): Thread | undefined => {
    const x = sessions.find((y) => y.id === id);
    // The model is the SESSION's, from sessions.json: a window that opened after the child started
    // never saw pi's `started` event, and read its status ("not started") as the model's name.
    return x && { id, name: x.name, kind: "session", readonly: !live.connected(), messages: [], pi: id, model: (x as { model?: string }).model };
  };
  const fail = (e: Error) => live.setStatusNote(e.message);
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
      if (sid !== t.pi) return live.setStatusNote(`the bench opened ${sid}, not ${t.pi}`);
      await loadThread(sid);
    })().catch((e: Error) => live.setStatusNote(openNote(e.message)));
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
      operations?.archiveSession(id);
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
    operations?.disposeSession(id);
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
      .map((t) => (t.kind === "session" ? { ...t, readonly: !live.connected() } : t))
      // Every thread carries its own session's model: a workspace tab is not the bench, and
      // reading only the bench's row showed "no model" in one (owner, 2026-09-17).
      .map((t) => (t.pi ? { ...t, messages: live.thread(t.pi).messages, model: t.model ?? modelOf(t.pi), ...tripleOf(t.pi) } : t));
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
    // The tab goes and so does everything TAB level held for it: the open file was drawing over
    // the next tab, and a dialog left open belonged to a view that no longer exists.
    setFiles(id, undefined);
    live.closeTab(id);
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
  // A file is opened FROM somewhere: the scope says which workspace's tool server holds it.
  //
  // It belongs to the TAB it was opened in, not to the window: as one global signal it outlived the
  // tab that opened it and drew over the next one (owner: a Dockerfile still showing after its tab
  // was closed). Keyed by thread id, so it also follows a tab moved between panes, comes back when
  // the tab is selected again, and goes when the tab does.
  type OpenFile = { path: string; status?: string; scope?: string };
  const [files, setFiles] = createStore<Record<string, OpenFile | undefined>>(
    hashView.endsWith("/diff") ? { [first]: { path: "bins/agent/src/controller/run.rs", status: "M" } } : {},
  );
  const file = () => files[selected()];
  const setFile = (f: OpenFile | undefined, id = selected()) => setFiles(id, f);
  // A task's log opens in place like a file does; a process is shown through
  // the same page, its ring of output as the "output" and its uptime as the clock.
  const asTask = (p?: live.Proc): live.Task | undefined =>
    p && { id: p.id, session: p.session ?? "", tool: "Process", arg: `${p.name} · ${p.command}`, state: procState(p), started: p.started, ended: p.ended, output: p.tail };
  const [taskId, setTaskId] = createSignal<string | undefined>();
  const [operationTask, setOperationTask] = createSignal<OperationProjection | undefined>();
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
  /**
   * A terminal belongs to the session tab it was opened from, and its scope is that tab's. A tab
   * IS a socket to the pod's shell sidecar (spec §2.3): there is nothing to adopt, nothing to
   * reconcile and nothing to reattach — a new tab is a new shell, and a closed one is finished.
   */
  const openShell = (owner = selected()) => {
    const scopeId = scopeOfTab(machine(), owner);
    setDrawer(true);
    const taken = tabs().filter((x) => x.owner === owner).map((x) => x.session);
    const t = makeTab(machine(), teamName(), owner, scopeId, nextIndex(taken, owner));
    setTabs((ts) => [...ts, t]);
    setActive(t.id);
  };
  /** Drop a tab from the window; the view unmounts, which closes its socket and ends that shell. */
  const dropTab = (id: string) => {
    const rest = tabs().filter((t) => t.id !== id);
    setTabs(rest);
    if (rest.length === 0) setMaximised(false);
    else if (active() === id) setActive(rest[rest.length - 1].id);
  };
  // Closing a tab ends its shell, because the socket is the shell. Nothing outlives it.
  const closeTab = (id: string) => dropTab(id);

  const closePanel = () => {
    setDrawer(false);
    setMaximised(false);
  };
  const shellShown = () => tabsHere().length > 0 && drawer();
  /** The Shell button and ⌘J are one gesture: open a shell here, bring the drawer back, or put it away. */
  const toggleShell = () => (shellShown() ? closePanel() : tabsHere().length ? setDrawer(true) : openShell());

  /** Back out of one layer at a time: a file, then the environment, then a
      maximised shell, then the shell. Nothing else swallows escape. */
  const back = () => {
    if (file()) setFile(undefined);
    else if (taskId()) setTaskId(undefined);
    else if (operationTask()) setOperationTask(undefined);
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
    if (why) return void live.setStatusNote(why);
    return window.harness.pi(cmd, id).then(
      (r) => (r.success === false ? (live.setStatusNote(String(r.error)), undefined) : r),
      (e: Error) => void live.setStatusNote(e.message),
    );
  };
  const placeItems = createMemo<PaletteItem[]>(() => {
    const m = machine();
    // Every session is its own place to go; there is no thread above them.
    // One row per session, by its own title. The second loop listed every session after the first
    // a SECOND time — a leftover from when row 0 was "the bench thread" and the rest were extra.
    const out: PaletteItem[] = live_().map((x) => ({ id: x.id, label: x.name, detail: "session", kind: "session" as const, icon: "thread", run: () => goTo(x.id) }));
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
    { id: "mode", group: "Suggested", label: `Mode: ${live.mode()} (cycle)`, keys: keyHint(KEYS.mode), run: () => {
      const next = live.MODES[(live.MODES.indexOf(live.mode()) + 1) % live.MODES.length];
      live.setMode(next);
      void pi({ type: "prompt", message: `/mode ${next === "plan" ? "plan" : "build"}` })?.catch(() => undefined);
    } },
    { id: "model", group: "Suggested", label: "Model…", keys: "/model", run: () => live.setDialog(selected(), "model") },
    { id: "thinking", group: "Suggested", label: "Thinking level (cycle)", keys: keyHint(KEYS.thinking), run: () => cycleThinking() },
    { id: "effort", group: "Suggested", label: "Effort (cycle)", keys: keyHint(KEYS.effort), run: () => cycleEffort() },
    { id: "composer", group: "Suggested", label: "Focus the prompt", keys: keyHint(KEYS.composer), run: () => composer()?.focus() },
    { id: "shell", group: "Suggested", label: shellShown() ? "Hide the shell" : tabsHere().length ? "Show the shell" : "Open a shell", keys: keyHint(KEYS.shell), run: () => toggleShell() },
    { id: "env", group: "Suggested", label: envTab() ? "Close the environment" : "Open the environment", keys: keyHint(KEYS.environment), run: () => (setFile(undefined), setEnvTab((v) => !v)) },
    { id: "panel", group: "Suggested", label: leftOpen() ? "Hide workspaces" : "Show workspaces", keys: keyHint(KEYS.panel), run: () => setLeftOpen((v) => !v) },
    { id: "inspector", group: "Suggested", label: rightOpen() ? "Hide the inspector" : "Show the inspector", keys: keyHint(KEYS.inspector), run: () => setRightOpen((v) => !v) },
    { id: "workspaces", group: "Suggested", label: "Switch workspace…", keys: keyHint(KEYS.workspaces), run: () => setPalette("workspaces") },
    { id: "go", group: "Suggested", label: "Go to…", keys: keyHint(KEYS.quickOpen), run: () => setPalette("go") },
    { id: "settings", group: "Suggested", label: "Settings", keys: keyHint(KEYS.settings), run: () => openSettings() },
    { id: "bg", group: "Session", label: "Send the running command to the background", keys: keyHint(KEYS.background), run: () => void pi({ type: "prompt", message: "/bg" }) },
    { id: "motionDemo", group: "Session", label: "Replay demo turn (to watch the animations)", run: () => playDemo(cur(), (ev) => live.onEvent({ ...ev, pi: cur() } as never)) },
    { id: "abort", group: "Session", label: "Stop this session", run: () => void pi({ type: "abort" }) },
    { id: "newSession", group: "Session", label: "New session", run: newSession },
    { id: "benchImport", group: "Session", label: "Import this laptop's sessions into the bench", run: () => {
      if (!live.connected() || !live.writable().ok) return void live.setStatusNote("not connected to the bench; nothing was sent");
      const raw = localStorage.getItem("harness.sessions");
      if (!raw) return void live.setStatusNote("nothing to import: this laptop has no local session list");
      void window.harness.benchImport(JSON.parse(raw) as { id: string; name: string; seq: number; lastActive?: number; archived?: boolean }[]).then((r) => {
        localStorage.setItem("harness.sessions.imported", String(Date.now()));
        live.setStatusNote(r.added.length ? `imported ${r.added.length} sessions and ${r.files} files` : "already imported: the bench has every session");
        return refreshSessions();
      }, (e: Error) => live.setStatusNote(e.message));
    } },
    { id: "deleteSession", label: "Delete this session", run: () => deleteSession(cur()) },
    { id: "archiveSession", label: "Archive this session", run: () => archiveSession(cur()) },
    { id: "archiveIdle", label: "Archive idle sessions (untouched for a day)", run: () => idleSessions().forEach((x) => archiveSession(x.id)) },
    { id: "split", label: "Split the tab to the right", keys: keyHint(KEYS.split), run: splitRight },
    { id: "find", label: "Find in page", keys: keyHint(KEYS.find), run: openFind },
    { id: "prev", label: "Previous thread", keys: keyHint(KEYS.prevThread), run: () => cycleThread(-1) },
    { id: "next", label: "Next thread", keys: keyHint(KEYS.nextThread), run: () => cycleThread(1) },
    { id: "nth", label: "Thread by position", keys: "⌘1…9", run: () => setPalette("go") },
    { id: "close", label: "Close what is open", keys: keyHint(KEYS.close), run: closeCurrent },
    { id: "theme", label: "Cycle theme", run: cycleTheme },
    { id: "motion", label: `Animations: ${motionChoice()} (cycle)`, run: cycleMotion },
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

  /** Whether opencode's leader (`ctrl+x`) is armed, and the timer that forgets it. */
  const [armed, setArmed] = createSignal(false);
  let leaderTimer: ReturnType<typeof setTimeout> | undefined;
  const arm = () => {
    setArmed(true);
    clearTimeout(leaderTimer);
    leaderTimer = setTimeout(() => setArmed(false), LEADER_FORGET_MS);
  };
  onCleanup(() => clearTimeout(leaderTimer));

  const onKey = (e: KeyboardEvent) => {
    // A terminal owns the keyboard while it has focus: nothing here may take a key from it except
    // a chord it cannot mean. Everything else reaches the PTY, once, through xterm alone.
    if (!mayAct(e, inTerminal(e.target))) return;
    const hit = (b: { match: (e: KeyboardEvent) => boolean }) => b.match(e);
    const stop = () => e.preventDefault();

    // The leader layer, first: while it is armed the next key is opencode's, not ours.
    if (armed()) {
      setArmed(false);
      clearTimeout(leaderTimer);
      const b = underLeader(e);
      if (b) {
        stop();
        if (b === KEYS.panel) return void setLeftOpen((v) => !v);
        if (b === KEYS.inspector) return void setRightOpen((v) => !v);
        if (b === KEYS.quickOpen) return void setPalette("go");
        if (b === KEYS.palette) return void setPalette("commands");
        if (b === KEYS.close) return closeCurrent();
      }
      const slot = leaderIndex(e);
      if (slot !== undefined && open()[slot]) return (stop(), showThread(open()[slot]));
      return;
    }
    if (hit(LEADER)) return (stop(), arm());
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
    // How hard the model thinks, and how hard it tries — both cycled through the BENCH, so the
    // session remembers them and the general default moves with the pick (spec §1.2). Neither key
    // is ever sent to pi from here.
    if (hit(KEYS.thinking)) return (stop(), cycleThinking());
    if (hit(KEYS.effort)) return (stop(), cycleEffort());
    if (hit(KEYS.quickOpen)) return (stop(), void setPalette("go"));
    if (hit(KEYS.workspaces)) return (stop(), void setPalette("workspaces"));
    if (hit(KEYS.find)) return (stop(), openFind());
    if (hit(KEYS.settings)) return (stop(), openSettings());
    if (palette()) return;
    // The dialog owns escape while it is open: without this, a focus that had drifted off the card
    // would abort the turn instead of closing the picker.
    if (hit(KEYS.back) && live.dialog(selected())) return (stop(), void live.setDialog(selected(), undefined));
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
  // `?motion-demo` plays the canned turn once, so every animation can be seen (and shot) without
  // spending a model turn on it.
  onMount(() => void (wantsDemo(location.search) && setTimeout(() => playDemo(cur(), (ev) => live.onEvent({ ...ev, pi: cur() } as never)), 400)));

  // The bench is live: session events and the bench's own changes land here.
  window.harness.onPi((ev) => {
    live.onEvent(ev);
    if (ev.type === "bench" && typeof ev.connected === "boolean") operations?.connection(ev.connected);
    if (shouldRefreshOn(ev)) void refresh();
    if (ev.type === "sessions") void refreshSessions().catch(() => undefined);
    // After a reconnect, every open session pages in what it missed.
    if (ev.type === "bench:resync") {
      operations?.connection(true);
      void Promise.all((operations?.entries() ?? []).map((projection) => projection.reload()));
      void refreshSessions().then(() => Promise.all(live_().map((x) => loadThread(x.id))), () => undefined);
    }
  });
  void window.harness.benchState().then(async (st) => {
    if (bootTest) {
      live.setConnected(true);
      setSessions([{ id: "boot-session", name: "Boot session", seq: 1, kind: "bench" }]);
      live.thread("boot-session").replay([
        { role: "user", content: "Show the boot fixture", timestamp: 1 },
        { role: "assistant", content: [{ type: "text", text: "The authenticated fixture is ready." }], timestamp: 2 },
      ]);
      return;
    }
    live.setConnected(st.connected);
    if (!st.configured) return void live.setStatusNote("not connected to your bench yet");
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
      | {
          model?: string;
          sessions?: Session[];
          plans?: { session: string; items: unknown[] }[];
          procs?: unknown[];
          tasks?: { row?: unknown }[];
          exchanges?: unknown[];
          proposals?: { id: string; session: string }[];
          messages?: { messages: unknown[] };
        }
      | undefined;
    if (boot?.model) live.setBenchModel(boot.model);
    // The catalogue ONCE at connect, so a model reads by its name everywhere — the footer showed
    // the raw `deepseek-v4-flash-vision-exp` because only the `/model` picker had ever fetched it,
    // and a person who never opened the picker never saw a readable name (owner, on the fleet).
    void live.models().then((r) => noteModelNames(r.flatMap((p) => p.models.map((m) => ({ id: `${p.id}/${m.id}`, name: m.name })))), () => undefined);
    if (boot?.sessions) setSessions(reconcile(boot.sessions));
    for (const r of boot?.plans ?? []) live.onEvent({ type: "plan", ...r });
    if (boot?.procs) live.onEvent({ type: "procs", rows: boot.procs });
    for (const row of boot?.tasks ?? []) live.onEvent({ type: "task", row });
    if (boot?.exchanges) live.seedExchanges(boot.exchanges);
    // A question the bench is HOLDING must show the moment a window connects. The bootstrap carried
    // these all along and nothing read them: after the bench pod was recreated the desktop showed no
    // card at all while `GET /proposals` had an open one, and the person was blocked with no idea
    // why (owner, 2026-09-17).
    for (const row of boot?.proposals ?? []) live.onEvent({ type: "proposal", row });
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
          if (r?.success === false) return live.setStatusNote(`clear refused: ${String(r.error ?? "pi would not start a new session")}`);
          L.replay([]);
          live.setStatusNote("new session");
        })
        ?.catch((e: Error) => live.setStatusNote(e.message));
    } },
    "/new": { help: "open another session beside this one", run: newSession },
    "/compact": { help: "summarise the older part of this session", run: () => void pi({ type: "compact" }) },
    "/abort": { help: "stop what this session is doing", run: () => void pi({ type: "abort" }) },
    // The picker IS the surface (spec §1.1): a bare `/model` opens it, and `provider/id` still
    // takes a direct pick for somebody who knows what they want.
    "/model": { help: "pick the model, thinking level and effort", local: true, run: (arg) => {
      const id = curThread()?.pi;
      if (!arg.trim()) return void live.setDialog(selected(), "model");
      if (!id) return void live.setStatusNote("open a session first");
      if (!/^[^/]+\/.+$/.test(arg.trim())) return void live.setStatusNote("usage: /model, or /model provider/id");
      void live.setModel(id, { model: arg.trim() });
    } },
    "/settings": { help: "open settings", local: true, run: () => openSettings() },
    "/btw": {
      help: "ask one question of a read-only fork of this session: /btw <question>",
      run: (arg) => {
        const from = cur();
        if (!arg.trim()) return live.setStatusNote("usage: /btw <question> — one question, one answer, nothing changed");
        // The fork's read tools run on the bench pod: on a workspace thread they would read the wrong machine.
        if (/^[we]-/.test(from ?? "")) return live.setStatusNote("btw is only for bench sessions");
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
          (e: Error) => (live.setStatusNote(e.message), side.setStatus("no answer")),
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

  /**
   * The current thread's triple, cycled one step. `^T` is offered only the levels THIS model takes;
   * `^E` only when the model has an effort at all, so a key never sets a knob the model cannot take.
   */
  const curThread = () => threads().find((t) => t.id === selected());
  const cycle = <T extends string>(all: readonly T[], cur: string | undefined) => all[(Math.max(0, all.indexOf(cur as T)) + 1) % all.length];
  const cycleThinking = () => {
    const t = curThread();
    if (!t?.pi) return;
    void live.setModel(t.pi, { thinking: cycle(live.THINKING, t.thinking) });
  };
  const cycleEffort = () => {
    const t = curThread();
    if (!t?.pi) return;
    void live.setModel(t.pi, { effort: cycle(live.EFFORT, t.effort) });
  };

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
    if (why && c) return void live.setStatusNote(why);
    // The harness's own commands act on the selected session; typed in a
    // read-only fork they go to that fork's pi like any other line.
    if (slash && SLASH[slash[1].toLowerCase()] && c && pi && !pi.startsWith("btw-")) {
      c.value = "";
      fit(c);
      c.dispatchEvent(new Event("input", { bubbles: true })); // the composer's own state (completion) follows the value
      // A local command leaves NO transcript row: it is a thing the person did to the desktop, not
      // something they said to the model, and `> /model 23:56` sat in the history as if it were
      // (owner, on the fleet). Only a prompt becomes a row. A command that has something to show
      // says it itself — `/help` notes, `/btw` writes into its own fork's thread.
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
    } else if (!L.ready()) {
      // The session's pi is still starting: it has answered nothing yet, so there is no turn to
      // steer and no answer coming for a moment. Shown as QUEUED at once rather than as a sent
      // message into silence — that gap is what made the owner type `retry` over his own prompt
      // (session 2026-09-17T19-58-02), which is how two of his prompts came to be reordered.
      L.queued(text, how);
    } else L.sent(text, atts.map((i) => i.n));
    void window.harness
      .pi(cmd, pi)
      .then((r) => {
        if (r.success !== false) return;
        const why = String(r.error ?? "");
        // pi refuses a plain prompt while a turn is in flight. That is not news for a person — it
        // is a retry: the line goes in as a follow-up and is answered when the turn ends. The raw
        // refusal ("Agent is already processing. Specify streamingBehavior…") used to be printed
        // into the conversation (owner, 2026-09-17).
        if (/already processing|streamingBehavior/i.test(why)) {
          L.queued(text, "queue");
          return void window.harness
            .pi({ ...cmd, streamingBehavior: "follow_up" }, pi)
            .then((again) => void (again.success === false && live.setStatusNote(String(again.error))), (e: Error) => live.setStatusNote(e.message));
        }
        live.setStatusNote(why);
      }, (e: Error) => live.setStatusNote(e.message));
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
          <ReposPanel
            repos={repos()}
            workspaces={machine().workspaces}
            onOpen={(r) => {
              // The clone URL, copied. The desktop is told the API origin and nothing else — it
              // knows no web address for a repo — so a link would be a guessed hostname. The clone
              // URL is what a person actually wants to paste, and it is derived, not invented.
              void window.harness.auth.api().then((api) => {
                const url = `${api.replace(/\/+$/, "")}/${r.name}.git`;
                return navigator.clipboard
                  ?.writeText(url)
                  .then(() => live.setStatusNote(`copied the clone URL for ${r.name}`), () => live.setStatusNote(url));
              }, (e: Error) => live.setStatusNote(ipcError(e)));
            }}
          />
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
              sessions={live_()}
              env={isActive() && envTab() ? environment() : undefined}
              file={isActive() ? file() : undefined}
              onCloseFile={() => setFile(undefined)}
              task={isActive() ? (live.tasks.find((t) => t.id === taskId()) ?? asTask(live.procs.find((p) => p.id === taskId()))) : undefined}
              operation={isActive() ? operationTask() : undefined}
              onCloseTask={() => (setTaskId(undefined), setOperationTask(undefined))}
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
            /* Which tree an agent session works in: the bench's own row, never derived from a name. */
            treeOf={(id) => (sessions.find((x) => x.id === id) as unknown as { tree?: string } | undefined)?.tree}
            onOpenShell={() => toggleShell()}
            onOpenTask={(id) => (setEnvTab(false), setFile(undefined), setTaskId(id))}
            onOpenOperation={(row) => (setEnvTab(false), setFile(undefined), setTaskId(undefined), setOperationTask(row))}
            onOpenFile={(path, status) => {
              setEnvTab(false);
              // Which workspace's tool server holds it: the selected tab's own, as the terminal
              // and the inspector resolve it. "bench" has no files to read.
              const scope = scopeOfTab(machine(), selected());
              setFile({ path, status, scope: scope === "bench" ? undefined : scope });
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
