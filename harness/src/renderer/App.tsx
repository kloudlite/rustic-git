import { Show, createMemo, createSignal, onCleanup } from "solid-js";
import { TitleBar } from "./components/TitleBar";
import { MachinePanel } from "./components/MachinePanel";
import { Chat } from "./components/Chat";
import { Inspector } from "./components/inspector/Inspector";
import { StatusBar } from "./components/StatusBar";
import { TerminalPanel } from "./components/terminal/TerminalPanel";
import { makeTab, type TermTab } from "./components/terminal/tabs";
import { ENVIRONMENTS, MACHINES, SNAPSHOTS, TEAMS } from "./model";
import { KEYS, threadIndex } from "./keys";
import type { Thread } from "./model";

export function App() {
  // A developer has exactly one work machine per team, so switching team is what
  // switches machine; there is nothing to choose within a team.
  const [teamId, setTeamId] = createSignal(TEAMS[0].id);
  const machine = createMemo(() => MACHINES.find((m) => m.teamId === teamId()) ?? MACHINES[0]);

  // Environments belong to the team, not the machine: the machine is connected
  // to one of them at a time.
  const [connected, setConnected] = createSignal(machine().environmentId);
  const environment = createMemo(() => ENVIRONMENTS.find((e) => e.id === connected()) ?? ENVIRONMENTS[0]);

  const [selected, setSelected] = createSignal(location.hash.slice(1).split("/")[0] || machine().id);
  // Threads are tabs: the machine's own plus any opened here, minus any closed.
  // The main thread cannot be closed — it is the one that changes things.
  const [closed, setClosed] = createSignal(new Set<string>());
  const [added, setAdded] = createSignal<Thread[]>([]);
  const threads = createMemo(() => [...machine().threads, ...added()].filter((t) => !closed().has(t.id)));
  const [threadId, setThreadId] = createSignal(machine().threads[0]?.id ?? "");

  let seq = 0;
  const newThread = () => {
    const t: Thread = { id: `th-new-${++seq}`, name: "new thread", readonly: true, messages: [] };
    setAdded((ts) => [...ts, t]);
    setEnvTab(false);
    setFile(undefined);
    setThreadId(t.id);
  };

  const closeThread = (id: string) => {
    const list = threads();
    const i = list.findIndex((t) => t.id === id);
    setClosed((prev) => new Set(prev).add(id));
    if (threadId() === id) {
      const next = list[i + 1] ?? list[i - 1];
      if (next) setThreadId(next.id);
    }
  };

  // The environment opens in the centre, as its own tab. While it is showing,
  // the inspector is put away: it describes the machine's tree, which has
  // nothing to say about a team's environment, and leaving a workspace's details
  // beside it only invites the wrong reading.
  const [envTab, setEnvTab] = createSignal(false);

  // A file opens as a tab too: reading one is a subject of its own, not a
  // property of the workspace it came from.
  const [file, setFile] = createSignal<{ path: string; status?: string } | undefined>();
  const inspector = () => rightOpen() && !envTab();

  const switchTeam = (id: string) => {
    setTeamId(id);
    const m = MACHINES.find((x) => x.teamId === id);
    if (m) {
      setConnected(m.environmentId);
      setSelected(m.id);
      setClosed(new Set<string>());
      setAdded([] as Thread[]);
      setThreadId(m.threads[0]?.id ?? "");
    }
  };

  // Either side dock can be put away; the conversation takes the room.
  const [leftOpen, setLeftOpen] = createSignal(true);
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

  /** Back out of one layer at a time: a file, then the environment, then a
      maximised shell. Nothing else swallows escape. */
  const back = () => {
    if (file()) setFile(undefined);
    else if (envTab()) setEnvTab(false);
    else if (maximised()) setMaximised(false);
    else if (tabs().length) closePanel();
  };

  const cycleThread = (by: number) => {
    const ts = threads();
    if (ts.length < 2) return;
    const i = ts.findIndex((t) => t.id === threadId());
    const next = ts[(i + by + ts.length) % ts.length];
    setEnvTab(false);
    setFile(undefined);
    setThreadId(next.id);
  };

  const onKey = (e: KeyboardEvent) => {
    const hit = (b: { match: (e: KeyboardEvent) => boolean }) => b.match(e);
    const stop = () => e.preventDefault();

    if (hit(KEYS.shell)) return (stop(), void (tabs().length ? closePanel() : openShell(scope())));
    if (hit(KEYS.environment)) return (stop(), setFile(undefined), void setEnvTab((v) => !v));
    if (hit(KEYS.inspector)) return (stop(), void setRightOpen((v) => !v));
    if (hit(KEYS.panel)) return (stop(), void setLeftOpen((v) => !v));
    if (hit(KEYS.prevThread)) return (stop(), cycleThread(-1));
    if (hit(KEYS.nextThread)) return (stop(), cycleThread(1));
    if (hit(KEYS.close)) return (stop(), back());
    if (hit(KEYS.back)) return back();
    if (hit(KEYS.composer)) {
      stop();
      setFile(undefined);
      setEnvTab(false);
      document.getElementById("composer")?.focus();
      return;
    }

    const i = threadIndex(e);
    if (i !== undefined && threads()[i]) {
      stop();
      setEnvTab(false);
      setFile(undefined);
      setThreadId(threads()[i].id);
    }
  };
  document.addEventListener("keydown", onKey);
  onCleanup(() => document.removeEventListener("keydown", onKey));

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
    <div class="grid h-full grid-rows-[36px_minmax(0,1fr)_26px]">
      <TitleBar machine={machine()} teams={TEAMS} teamId={teamId()} onSwitchTeam={switchTeam} />

      <div
        class="grid min-h-0"
        style={{
          "grid-template-columns": `${leftOpen() ? "280px " : ""}minmax(0,1fr)`,
        }}
      >
        <Show when={leftOpen()}>
          <MachinePanel
            machine={machine()}
            environment={environment()}
            environments={ENVIRONMENTS}
            selected={selected()}
            onSelect={setSelected}
            onOpenEnv={() => setEnvTab(true)}
            onConnect={setConnected}
          />
        </Show>

        <div class="grid min-h-0 min-w-0">
            <Chat
              machine={machine()}
              env={envTab() ? environment() : undefined}
              file={file()}
              onCloseFile={() => setFile(undefined)}
              snapshots={SNAPSHOTS.filter((s) => s.environment === environment().name)}
              onCloseEnv={() => setEnvTab(false)}
              threads={threads()}
              threadId={threadId()}
              onThread={setThreadId}
              onNewThread={newThread}
              onCloseThread={closeThread}
              inspector={
                inspector() ? (
                  <Inspector
                    machine={machine()}
                    selected={selected()}
                    onOpenShell={openShell}
                    onOpenFile={(path, status) => {
                      setEnvTab(false);
                      setFile({ path, status });
                    }}
                  />
                ) : undefined
              }
              shell={
                tabs().length ? (
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
      </div>

      <StatusBar
        machine={machine()}
        shells={tabs().length}
        leftOpen={leftOpen()}
        rightOpen={rightOpen()}
        onToggleLeft={() => setLeftOpen((v) => !v)}
        onToggleRight={() => setRightOpen((v) => !v)}
      />
    </div>
  );
}
