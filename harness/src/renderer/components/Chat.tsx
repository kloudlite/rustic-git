import { For, Show, createEffect, createMemo, createSignal, onCleanup, onMount, untrack, type JSX } from "solid-js";
import { marked } from "marked";
import "../syntax";
import { Icon } from "../ui/Icon";
import { Button } from "../ui/Button";
import { Badge } from "../ui/Badge";
import { Kbd } from "../ui/parts";
import { EnvironmentPage } from "./EnvironmentPage";
import { SettingsPage } from "./SettingsPage";
import { FileView } from "./FileView";
import { TaskView } from "./TaskView";
import { ToolCall } from "./ToolCall";
import { HINTS } from "../keys";
import * as live from "../live";
import type { Environment, Machine, Message, Snapshot, Thread, Workspace } from "../model";

type Action = Extract<Message, { role: "action" }>;
type QuestionRow = Extract<Message, { role: "question" }>;

const KIND_GLYPH = { spawn: "+", run: "$", fold: "⇡", note: "…" } as const;
/** A workspace's answer to an ask comes back as a prompt tagged with its name; the tag is a label. */
const FROM_WS = /^\[from workspace ([^\]]+)\] /;
const fromWorkspace = (t: string) => FROM_WS.exec(t)?.[1];
const said = (t: string) => t.replace(FROM_WS, "");

/**
 * Every tab is a thread, and every thread belongs to a node of the machine's
 * tree: the machine's own thread is the one that changes things, a workspace's
 * is where that copy is worked, an ephemeral's is watched. With nothing open
 * the pane is the home screen — the places a person can go.
 */
export function Chat(props: {
  machine: Machine;
  team: string;
  env?: Environment;        // open as a tab of its own, when the dock asks for it
  snapshots: Snapshot[];
  onCloseEnv: () => void;
  settings: boolean;        // the machine's settings page, a tab like the environment
  settingsPage?: { id: string }; // the section a caller asked to land on
  onCloseSettings: () => void;
  threads: Thread[];
  threadId: string;
  onThread: (id: string) => void;
  onSwitch: () => void;
  commands: { name: string; help: string }[];   // what `/` can complete to
  inspectorOpen: boolean;
  onToggleInspector: () => void;
  dimmed?: boolean;                                   // the other pane: quieter, so the active one reads
  onDropTab: (id: string, index: number) => void;     // a tab dragged onto this pane's tab row
  onSplit?: () => void;                               // move the current tab to a new pane on the right
  onCloseThread: (id: string) => void;
  file?: { path: string; status?: string };
  onCloseFile: () => void;
  task?: live.Task;          // a task's log, open in place like a file
  onCloseTask: () => void;
  /** The shell, likewise: it runs in this machine, so it opens under the
      conversation inside the same container rather than across the window. */
  shell?: JSX.Element;
  /** The shell takes the whole pane: the conversation row collapses to nothing. */
  shellFull?: boolean;
}) {
  const thread = createMemo<Thread | undefined>(
    () => props.threads.find((t) => t.id === props.threadId),
  );
  const blocks = () => thread()?.messages ?? [];
  // The live state behind this thread: the bench's, a side session's, or an
  // idle one for a recorded thread (nothing arrives on it, so it stays quiet).
  const L = () => live.thread(thread()?.pi ?? "");
  // A long session reads in sittings: a gap of more than half an hour between
  // two messages starts a new one, and every sitting but the latest folds to
  // one line — what it covered and when — until it is opened. The transcript
  // stays a page, not a scroll through the week.
  const GAP = 30 * 60 * 1000;
  const sittings = createMemo(() => {
    const out: Message[][] = [];
    let last: number | undefined;
    for (const m of blocks()) {
      if (!out.length || (m.ts && last && m.ts - last > GAP)) out.push([]);
      out[out.length - 1].push(m);
      if (m.ts) last = m.ts;
    }
    return out;
  });
  const [opened, setOpened] = createSignal(new Set<number>());
  const isOpen = (i: number) => i === sittings().length - 1 || opened().has(i);
  let hist = -1;     // how far back ↑ has walked; -1 is the draft
  let draft = "";
  // `/` completion: the commands whose name starts with what is typed, shown
  // above the composer while the caret is still in the first word.
  const [typed, setTyped] = createSignal("");
  const [pick, setPick] = createSignal(0);
  const matches = createMemo(() => {
    const m = /^(\/[a-z-]*)$/i.exec(typed());
    return m ? props.commands.filter((c) => c.name.startsWith(m[1].toLowerCase())) : [];
  });
  const accept = (t: HTMLTextAreaElement, i = pick()) => {
    const c = matches()[i];
    if (!c) return false;
    t.value = c.name + " ";
    setTyped(t.value);
    fit(t);
    t.setSelectionRange(t.value.length, t.value.length);
    return true;
  };
  let scroller: HTMLDivElement | undefined;
  // Reading back up is not fought: new content only pulls the view down when
  // it was already at the bottom; otherwise a "jump to latest" pill appears.
  const [behind, setBehind] = createSignal(false);
  // The scroller is a reversed column: the browser itself keeps the view
  // pinned to the end while scrollTop is 0, exactly as a terminal does, and
  // never moves it once a person has scrolled up (scrollTop goes negative).
  const atBottom = () => !!scroller && scroller.scrollTop > -2;
  const toBottom = () => {
    if (scroller) scroller.scrollTop = 0;
    setBehind(false);
  };
  // Follow growth, not renders: whatever makes the column taller — a new row,
  // a streamed word, an opened result — scrolls the view if it was at the
  // bottom, and raises the pill if it was not.
  let column: HTMLDivElement | undefined;
  onMount(() => {
    const ro = new ResizeObserver(() => { if (!atBottom()) setBehind(true); });
    if (column) ro.observe(column);
    onCleanup(() => ro.disconnect());
  });
  const readonly = () => thread()?.readonly ?? false;
  // The environment takes the pane while it is open: it is a different subject,
  // not a property of the thread underneath it.
  const onEnv = () => !!props.env;
  const onFile = () => (!!props.file || !!props.task) && !onEnv();
  const away = () => onEnv() || props.settings;
  const empty = () => props.threads.length === 0 && !away();
  const running = () => props.machine.workspaces.flatMap((w) => w.ephemerals).filter((e) => e.state === "running").length;

  return (
    <main class="grid h-full min-h-0 min-w-0 overflow-clip transition-opacity" classList={{ "grid-rows-[35px_22px_minmax(0,1fr)]": !empty(), "grid-rows-[minmax(0,1fr)]": empty(), "opacity-70": props.dimmed }}>
      {/* Nothing open, nothing to tab between: the home screen takes the whole pane. */}
      <Show when={!empty()}>
      <div
        class="flex items-stretch border-b border-line bg-tab-inactive pr-2"
        role="tablist"
        onDragOver={(e) => e.preventDefault()}
        onDrop={(e) => {
          // Dropped on the row itself (past the last tab): goes to the end.
          const id = e.dataTransfer?.getData("text/tab");
          if (id) (e.preventDefault(), props.onDropTab(id, props.threads.length));
        }}
      >
        <For each={props.threads}>
          {(t, i) => (
            // A tab is a row, not a button: a close control cannot live inside a
            // button, and nesting one is invalid markup that browsers treat
            // inconsistently — which is why the close never fired.
            <div
              role="tab"
              draggable={true}
              onDragStart={(e) => e.dataTransfer?.setData("text/tab", t.id)}
              onDragOver={(e) => (e.preventDefault(), e.stopPropagation())}
              onDrop={(e) => {
                const id = e.dataTransfer?.getData("text/tab");
                if (!id || id === t.id) return;
                e.preventDefault();
                e.stopPropagation();
                // Left half: before this tab; right half: after it.
                const r = e.currentTarget.getBoundingClientRect();
                props.onDropTab(id, i() + (e.clientX > r.left + r.width / 2 ? 1 : 0));
              }}
              aria-selected={!away() && t.id === thread()?.id}
              title={TAB_TITLE[t.kind]}
              class="relative flex h-[35px] max-w-60 min-w-32 items-center gap-1.5 border-r border-line pr-2 pl-3 text-base whitespace-nowrap text-muted
                     hover:bg-bg aria-selected:-mb-px aria-selected:bg-bg aria-selected:text-fg
                     aria-selected:before:absolute aria-selected:before:inset-x-0 aria-selected:before:top-0 aria-selected:before:h-px aria-selected:before:bg-focus"
            >
              <button
                class="flex min-w-0 flex-1 items-center gap-1.5 self-stretch"
                onClick={() => {
                  props.onCloseEnv();
                  props.onCloseFile();
                  props.onThread(t.id);
                }}
              >
                <Icon name={TAB_ICON[t.kind]} class={t.kind === "ephemeral" || t.kind === "session" ? "text-muted" : "text-accent"} />
                <span class="min-w-0 truncate">{t.name}</span>
              </button>
              <button
                class="shrink-0 rounded-sm p-0.5 text-subtle hover:bg-line hover:text-fg"
                title="Close (⌘W)"
                onClick={() => props.onCloseThread(t.id)}
              >
                <Icon name="x" size={11} />
              </button>
            </div>
          )}
        </For>
        <Show when={props.env}>
          {(env) => (
            <div
              role="tab"
              aria-selected={true}
              class="relative flex h-[35px] max-w-60 min-w-32 items-center gap-1.5 border-r border-line px-3 text-base whitespace-nowrap text-muted
                     aria-selected:-mb-px aria-selected:bg-bg aria-selected:text-fg
                     aria-selected:before:absolute aria-selected:before:inset-x-0 aria-selected:before:top-0 aria-selected:before:h-px aria-selected:before:bg-focus"
            >
              <Icon name="server" class="text-accent" />
              <span class="min-w-0 flex-1 truncate font-mono">{env().name}</span>
              <button class="shrink-0 rounded-sm p-0.5 text-subtle hover:bg-line hover:text-fg" title="Close" onClick={props.onCloseEnv}>
                <Icon name="x" size={11} />
              </button>
            </div>
          )}
        </Show>
        <Show when={props.settings}>
          <div
            role="tab"
            aria-selected={true}
            class="relative flex h-[35px] max-w-60 min-w-32 items-center gap-1.5 border-r border-line px-3 text-base whitespace-nowrap text-muted
                   aria-selected:-mb-px aria-selected:bg-bg aria-selected:text-fg
                   aria-selected:before:absolute aria-selected:before:inset-x-0 aria-selected:before:top-0 aria-selected:before:h-px aria-selected:before:bg-focus"
          >
            <Icon name="settings" class="text-accent" />
            <span class="min-w-0 flex-1 truncate">settings</span>
            <button class="shrink-0 rounded-sm p-0.5 text-subtle hover:bg-line hover:text-fg" title="Close" onClick={props.onCloseSettings}>
              <Icon name="x" size={11} />
            </button>
          </div>
        </Show>
        <span class="flex-1" />
        <Button class="self-center" variant="icon" icon="plus" title="Open a workspace (⌘T)" onClick={props.onSwitch} />
        <Show when={props.onSplit}>
          <Button class="self-center" variant="icon" icon="split" title="Split right (⌘\\)" onClick={props.onSplit} />
        </Show>
        <Show when={!away()}>
          <Button
            class="self-center aria-pressed:text-fg"
            variant="icon"
            icon="panelRight"
            title="Inspector (⌘⌥B)"
            aria-pressed={props.inspectorOpen}
            onClick={props.onToggleInspector}
          />
        </Show>
      </div>

      <div class="flex h-5.5 items-center gap-1 px-4 text-base whitespace-nowrap text-fg/80">
        <Show
          when={!away()}
          fallback={
            <Show when={props.env} fallback={<><span>{props.team}</span><Icon name="chevronRight" size={16} class="text-fg/60" /><span>{props.machine.owner.split("@")[0]}</span><Icon name="chevronRight" size={16} class="text-fg/60" /><span>settings</span></>}>
              <span>team</span>
              <Icon name="chevronRight" size={16} class="text-fg/60" />
              <span>environment</span>
              <Icon name="chevronRight" size={16} class="text-fg/60" />
              <span>{props.env!.name}</span>
            </Show>
          }
        >
        <span>{props.team}</span>
        <Icon name="chevronRight" size={16} class="text-fg/60" />
        <span>{props.machine.owner.split("@")[0]}</span>
        <Icon name="chevronRight" size={16} class="text-fg/60" />
        <span>{thread()?.name}</span>
        <span class="flex-1" />
        <Show when={thread()?.pi && !live.connected()}>
          <span class="mr-2 inline-flex items-center gap-1 text-xs text-warning" title="Showing what was last seen; nothing can be sent until the bench is back">
            <span class="size-1.5 rounded-full bg-warning" /> bench offline
          </span>
        </Show>
        <Show when={readonly()}>
          <span class="inline-flex items-center gap-1 text-xs text-muted">
            <Icon name="lock" size={12} /> read-only
          </span>
        </Show>
        </Show>
      </div>

      </Show>

      <Show when={props.env} keyed>
        {(env) => <EnvironmentPage env={env} snapshots={props.snapshots} />}
      </Show>
      <Show when={props.settings && !props.env}>
        <SettingsPage machine={props.machine} open={props.settingsPage} />
      </Show>


      {/* The conversation and what it is about are one thing: they sit in a
          centred container together, rather than the transcript floating in the
          middle of the pane with the inspector pinned to the window's edge. */}
      <Show when={!away() && !thread()}>
        <Home machine={props.machine} team={props.team} onGo={props.onThread} onSwitch={props.onSwitch} />
      </Show>

      <div class="min-h-0 min-w-0 overflow-clip" classList={{ hidden: away() || !thread() }}>
      {/* Every level here is `min-w-0`: a grid item defaults to a min-content
          width, and one wide table or unbroken token in the transcript would
          otherwise widen the column and run the text under the inspector. */}
      <div class={props.shellFull ? "grid h-full min-h-0 w-full min-w-0 grid-rows-[0_minmax(0,1fr)]" : "grid h-full min-h-0 w-full min-w-0 grid-rows-[minmax(0,1fr)_auto]"}>
      {/* A zero-height grid row does not clip: the composer would still paint
          over the terminal, so the whole chat column is hidden while a shell
          has the tab to itself. */}
      <div class="grid min-h-0 min-w-0 grid-cols-[minmax(0,1fr)]" classList={{ hidden: props.shellFull }}>
      <div class="grid min-h-0 min-w-0 grid-rows-[minmax(0,1fr)_auto_auto] font-mono text-sm leading-[18px]">
        {/* A file opens in place: the thread stays selected and the sidebar
            stays put, because reading a file is part of following the work
            rather than a separate place to be. */}
        <Show when={props.file} keyed>
          {(f) => <FileView path={f.path} status={f.status} onClose={props.onCloseFile} />}
        </Show>
        <Show when={!props.file && props.task}>
          {(t) => <TaskView task={t()} onClose={props.onCloseTask} />}
        </Show>
        {/* A reversed flex column: the end is the scroll origin, so the browser
            keeps the view pinned there as content grows and leaves it alone
            once a person scrolls up — the terminal's behaviour, no script. */}
        <div
          ref={scroller}
          class="relative flex min-w-0 flex-col-reverse overflow-x-clip overflow-y-auto px-9 pt-5 pb-4 select-text"
          classList={{ hidden: onFile() }}
          onScroll={() => { if (atBottom()) setBehind(false); }}
        >
          <div ref={column} class="flex min-w-0 flex-col gap-4">
            {/* A thread with nothing in it yet is a first run: say what the bench
                is for and offer a few first asks, which fill the prompt rather
                than send — the person's words go first. */}
            <Show when={(thread()?.kind === "machine" || thread()?.kind === "session") && L().ready() && blocks().length === 0}>
              <FirstRun team={props.team} onPick={(t) => {
                const c = scroller?.closest("main")?.querySelector<HTMLTextAreaElement>("textarea[data-composer]");
                if (c) (c.value = t, fit(c), c.focus());
              }} />
            </Show>
            {/* Where the thread begins, so the space above the first message reads
                as the top of something rather than as nothing. */}
            <div class="flex items-center gap-3 pb-1 font-ui text-xs text-subtle" classList={{ hidden: blocks().length === 0 }}>
              <span class="h-px flex-1 bg-line-subtle" />
              <span>
                {thread()?.kind === "machine" ? "Bench Thread" : thread()?.kind === "ephemeral" ? "watching" : thread()?.kind === "btw" ? "forked from the bench · read-only" : thread()?.name}
                <Show when={thread()?.messages[0]}>{(m) => <> · started {m().at}</>}</Show>
              </span>
              <span class="h-px flex-1 bg-line-subtle" />
            </div>
            <For each={sittings()}>
              {(sit, si) => (
                <Show when={isOpen(si())} fallback={<Folded messages={sit} onOpen={() => setOpened((o) => new Set(o).add(si()))} />}>
                  <Show when={si() > 0}><SittingRule messages={sit} /></Show>
            <For each={sit}>
              {(b) => (
                <Show when={b.role !== "question"} fallback={<Question q={b as QuestionRow} session={L().id} />}>
                <Show when={b.role !== "action"} fallback={<div class="[contain:layout_style]"><Show when={(b as Action).tool} fallback={<Step a={b as Action} />}><ToolCall a={b as Action} /></Show></div>}>
                  {/* A prompt is a command and reads like one; an answer is prose
                      and reads in the UI face, so the two turns are told apart by
                      their type rather than by a glyph alone. */}
                  <Show
                    when={b.role === "user"}
                    fallback={
                      <div class="flex items-start">
                        {/* The dot keeps the mono rail every other row hangs off; the answer beside it is prose. */}
                        <span class="w-5 shrink-0 font-mono leading-[21px] text-fg-strong">⏺</span>
                        <Prose text={(b as { text: string }).text} latest={b === blocks()[blocks().length - 1]} />
                      </div>
                    }
                  >
                    <div class="flex flex-col leading-[18px]">
                      <div class="-mx-3 flex items-start rounded-[2px] border border-request-line bg-request px-3 py-2">
                        <span class="w-5 shrink-0 font-bold text-accent">&gt;</span>
                        <span class="min-w-0 flex-1 wrap-words whitespace-pre-wrap text-fg">
                          {/* An answer a workspace sent back arrives as a prompt; the workspace is a label, not the message. */}
                          <Show when={fromWorkspace((b as { text: string }).text)}>
                            {(w) => <span class="mr-1.5 rounded-[2px] bg-fg/10 px-1 text-2xs text-subtle">{w()}</span>}
                          </Show>
                          {said((b as { text: string }).text)}
                        </span>
                        <Time at={(b as { at: string }).at} />
                      </div>
                      <For each={(b as { images?: number[] }).images ?? []}>
                        {(n) => (
                          <div class="flex items-start text-accent">
                            <span class="w-5 shrink-0 pl-2 text-subtle">⎿</span>
                            <span class="pl-1">[Image #{n}]</span>
                          </div>
                        )}
                      </For>
                    </div>
                  </Show>
                </Show>
                </Show>
              )}
            </For>
                </Show>
              )}
            </For>
          </div>
        </div>

        {/* The composer is a surface, not a line: a lifted box the caret lives in,
            with the keys that drive it underneath rather than crowding it. */}
        {/* One box: the caret and the keys that drive it belong together, and a
            hint row floating underneath read as a second, unrelated thing. */}
        <div class="relative min-w-0 bg-bg px-6 pt-1 pb-3.5" classList={{ hidden: onFile() || thread()?.kind === "btw" }}>
          <Show when={matches().length}>
            <div class="absolute right-6 bottom-full left-6 z-20 mb-1 max-h-64 overflow-y-auto rounded-md border border-widget-line bg-overlay py-1 shadow-overlay">
              <For each={matches()}>
                {(c, i) => (
                  <button
                    class="flex h-5.5 w-full items-center gap-3 px-3 text-left font-mono text-sm"
                    classList={{ "bg-selected text-selected-fg": i() === pick(), "text-fg": i() !== pick() }}
                    onMouseMove={() => setPick(i())}
                    onMouseDown={(e) => { e.preventDefault(); accept(e.currentTarget.closest("main")!.querySelector<HTMLTextAreaElement>("textarea[data-composer]")!, i()); }}
                  >
                    <span class="w-28 shrink-0">{c.name}</span>
                    <span class="min-w-0 truncate font-ui text-xs" classList={{ "text-muted": i() !== pick() }}>{c.help}</span>
                  </button>
                )}
              </For>
            </div>
          </Show>
          <Show when={behind()}>
            <button
              class="absolute -top-9 left-1/2 z-10 flex h-7 -translate-x-1/2 items-center gap-1.5 rounded-[2px] border border-widget-line bg-overlay px-3 text-xs text-fg shadow-overlay hover:bg-hover"
              onClick={toBottom}
            >
              <Icon name="chevronDown" size={12} /> Jump to latest
            </button>
          </Show>
          {/* What this session is waiting on: lines pi still holds, and asks a workspace has not
              answered yet. The asks were only ever in the bench's exchange log, so nothing showed
              them — a person could not tell a queued ask from a lost one. */}
          <Show when={L().queue.length || live.asksOf(L().id).length}>
            <div class="mb-2 flex flex-col gap-1 px-3 font-mono text-sm">
              <For each={L().queue}>
                {(q) => (
                  <div class="flex items-start gap-2 text-muted">
                    <span class="w-5 shrink-0 text-subtle" title={q.how === "steer" ? "steers the turn" : "waits its turn"}>›</span>
                    <span class="min-w-0 flex-1 truncate">{q.text}</span>
                    <Show when={q.how === "steer"}><span class="shrink-0 text-xs text-subtle">steer</span></Show>
                  </div>
                )}
              </For>
              <For each={live.asksOf(L().id)}>
                {(a) => (
                  <div class="flex items-start gap-2 text-muted">
                    <span class="w-5 shrink-0 text-subtle">›</span>
                    <span class="shrink-0 text-xs text-subtle">{a.state}</span>
                    <span class="shrink-0 rounded-[2px] bg-fg/10 px-1 text-2xs text-muted">{a.workspace}</span>
                    <span class="min-w-0 flex-1 truncate">{a.text.replace(/^\[ask \S+ from [^\]]*\] /, "")}</span>
                  </div>
                )}
              </For>
            </div>
          </Show>
          {/* One status line while a turn runs: what it is doing, for how long, what it has spent.
              A question card blocks it — nothing is happening until the person answers. */}
          <Show when={L().busy() && thread()?.pi && !L().messages.some((m) => m.role === "question" && !(m as { answer?: string }).answer)}>
            <StatusLine turn={L().turn()} />
          </Show>
          <div class="flex flex-col rounded-[2px] border border-input-line bg-input transition-[border-color] duration-[var(--motion)] ease-out-quick focus-within:border-focus">
            <div class="flex items-start px-3 pt-2 pb-1.5">
              <span class="w-5 shrink-0 leading-5 text-accent">❯</span>
              {/* Grows with what is typed, up to a cap, then scrolls: ↩ sends,
                  ⇧↩ is a newline, so a long prompt is still written in place. */}
              <textarea
                data-composer
                disabled={thread()?.kind === "machine" && !thread()?.pi}
                rows="1"
                class="max-h-60 min-h-5 flex-1 resize-none border-0 bg-transparent p-0 leading-5 outline-none placeholder:text-subtle"
                placeholder={thread()?.kind === "machine" && !thread()?.pi ? "no session yet · start one with + beside Sessions" : thread()?.pi && !live.connected() ? "not connected" : thread()?.kind === "btw" ? "ask about the bench's work · nothing here changes anything" : readonly() ? "ask or discuss · this thread cannot change anything" : "tell the bench what to do"}
                onInput={(e) => (fit(e.currentTarget), setTyped(e.currentTarget.value), setPick(0), (hist = -1))}
                onKeyDown={(e) => {
                  // Completion first: while suggestions show, ↑/↓ move, ⇥ and ↩
                  // take one (↩ sends only when the word is already complete).
                  if (matches().length) {
                    const t = e.currentTarget;
                    if (e.key === "ArrowDown") return (e.preventDefault(), setPick((p) => (p + 1) % matches().length));
                    if (e.key === "ArrowUp") return (e.preventDefault(), setPick((p) => (p - 1 + matches().length) % matches().length));
                    if (e.key === "Tab") return (e.preventDefault(), void accept(t));
                    if (e.key === "Escape") return (e.preventDefault(), setTyped(""));
                    if (e.key === "Enter" && !e.shiftKey && matches()[pick()]?.name !== t.value.trim()) return (e.preventDefault(), e.stopPropagation(), void accept(t));
                  }
                  // ↑/↓ walk the prompts already sent, shell-style, but only
                  // when the caret is on the first/last line so a multi-line
                  // prompt is still navigable; a draft in progress is kept.
                  const t = e.currentTarget;
                  const sent = blocks().filter((m) => m.role === "user").map((m) => (m as { text: string }).text);
                  if (!sent.length) return;
                  const onFirst = !t.value.slice(0, t.selectionStart).includes("\n");
                  const onLast = !t.value.slice(t.selectionEnd).includes("\n");
                  if (e.key === "ArrowUp" && onFirst) {
                    if (hist === -1) draft = t.value;
                    if (hist < sent.length - 1) hist++;
                    else return;
                  } else if (e.key === "ArrowDown" && onLast && hist >= 0) {
                    hist--;
                  } else return;
                  e.preventDefault();
                  t.value = hist === -1 ? draft : sent[sent.length - 1 - hist];
                  fit(t);
                  t.setSelectionRange(t.value.length, t.value.length);
                }}
                onPaste={(e) => {
                  // An image on the clipboard attaches; text pastes as text.
                  const files = Array.from(e.clipboardData?.items ?? []).filter((i) => i.type.startsWith("image/")).map((i) => i.getAsFile()).filter((f): f is File => !!f);
                  if (!files.length) return;
                  e.preventDefault();
                  const t = e.currentTarget;
                  void Promise.all(files.map(L().attach)).then((ns) => {
                    const token = ns.map((n) => `[Image #${n}]`).join(" ");
                    const at = t.selectionStart ?? t.value.length;
                    const before = t.value.slice(0, at);
                    const after = t.value.slice(t.selectionEnd ?? at);
                    t.value = `${before}${before && !/\s$/.test(before) ? " " : ""}${token}${after && !/^\s/.test(after) ? " " : ""}${after}`;
                    const caret = t.value.length - after.length;
                    t.setSelectionRange(caret, caret);
                    fit(t);
                    t.focus();
                  });
                }}
              />
            </div>
            {/* What is attached, as thumbnails the way an editor shows a pasted
                image: small, removable, sent with the next message. */}
            <Show when={L().attachments().length}>
              <div class="flex flex-wrap gap-2 px-3 pb-2">
                <For each={L().attachments()}>
                  {(img) => (
                    <div class="group relative h-14 w-14 overflow-hidden rounded-[2px] border border-input-line bg-input">
                      <img src={img.url} alt="" class="h-full w-full object-cover" />
                      <button
                        class="absolute top-0.5 right-0.5 hidden size-4 items-center justify-center rounded-sm bg-overlay text-fg group-hover:inline-flex"
                        title="Remove"
                        onClick={() => L().detach(img.id)}
                      >
                        <Icon name="x" size={10} />
                      </button>
                    </div>
                  )}
                </For>
              </div>
            </Show>
            <div class="flex min-w-0 flex-wrap items-center gap-x-3.5 gap-y-1 border-t border-line-subtle px-3 py-1.5 font-ui text-xs text-subtle">
              <Show when={L().busy()} fallback={<For each={HINTS}>{(b) => <Hint keys={b.keys}>{b.label}</Hint>}</For>}>
                <Hint keys="↩">queue after this</Hint>
                <Hint keys="⌘↩">steer now</Hint>
                <Hint keys="⇧↩">newline</Hint>
              </Show>
              <span class="flex-1" />
              <span class="min-w-0 truncate" title={L().status()}>{L().busy() ? "working…" : L().status()}</span>
            </div>
          </div>
        </div>
        {props.shell}
      </div>
      </div>
      </div>
      </div>
    </main>
  );
}

/** Times sit in a column of their own, right aligned, so they read as a margin. */
const TAB_ICON = { machine: "machine", session: "thread", workspace: "workspace", ephemeral: "ephemeral", btw: "lock" } as const;
const TAB_TITLE = {
  machine: "The bench thread: the one that changes things",
  session: "A session of the bench: one thing being worked on",
  workspace: "This workspace's thread",
  ephemeral: "An agent's log: watch, it cannot be steered",
  btw: "A read-only side session forked from the bench: ask, it changes nothing",
} as const;

/**
 * Nothing open. The pane reads as a start page: who and where at the top,
 * then the places a person can go on the left and the keys that take them
 * there on the right — one screen, no scrolling, nothing to learn twice.
 */
function Home(props: { machine: Machine; team: string; onGo: (id: string) => void; onSwitch: () => void }) {
  const running = (w: Workspace) => w.ephemerals.filter((e) => e.state === "running").length;
  const KEYS_SHOWN: [string, string][] = [
    ["⌘T", "switch workspace"],
    ["⌘P", "go to anything"],
    ["⌘⇧P", "every command"],
    ["⌘J", "shell"],
    ["⌘E", "environment"],
    ["⌘,", "settings"],
  ];
  return (
    <div class="flex min-h-0 items-center justify-center overflow-y-auto p-10">
      <div class="w-full max-w-[820px]">
        <header class="mb-8 flex items-end gap-4 border-b border-line-subtle pb-5">
          <div class="min-w-0 flex-1">
            <div class="text-2xs font-semibold uppercase text-subtle">{props.team} · {props.machine.owner.split("@")[0]}</div>
            <h1 class="mt-1 text-md font-medium">Bench</h1>
            <p class="mt-1 truncate text-sm text-muted">{props.machine.goal || "No goal yet. The first message sets it."}</p>
          </div>
          <Button icon="workspace" onClick={props.onSwitch} title="⌘T">Open a workspace</Button>
        </header>

        <div class="grid grid-cols-[minmax(0,1fr)_240px] gap-12">
          <section>
            <div class="mb-1.5 text-2xs font-semibold uppercase text-subtle">Open</div>
            <div class="flex flex-col">
              <button class={HOME_ROW} onClick={() => props.onGo(props.machine.id)}>
                <Icon name="machine" size={14} class="shrink-0 text-accent" />
                <span class="text-sm text-fg">Bench Thread</span>
                <span class="min-w-0 flex-1 truncate text-xs text-subtle">the thread that changes things</span>
              </button>
              <For each={props.machine.workspaces}>
                {(w) => (
                  <button class={HOME_ROW} onClick={() => props.onGo(w.id)}>
                    <Icon name="workspace" size={14} class={`shrink-0 ${w.state === "running" ? "text-accent" : "text-subtle"}`} />
                    <span class="font-mono text-sm text-fg">{w.name}</span>
                    <span class="min-w-0 flex-1 truncate font-mono text-xs text-subtle">{w.repo} · {w.branch}</span>
                    <Show when={running(w)}>
                      {(n) => <span class="inline-flex items-center gap-1.5 text-xs text-muted"><span class="size-1.5 rounded-full bg-success" />{n()} running</span>}
                    </Show>
                  </button>
                )}
              </For>
            </div>
          </section>

          <section>
            <div class="mb-1.5 text-2xs font-semibold uppercase text-subtle">Keys</div>
            <div class="grid grid-cols-[auto_minmax(0,1fr)] items-center gap-x-3 gap-y-1">
              <For each={KEYS_SHOWN}>
                {([k, label]) => (
                  <>
                    <Kbd>{k}</Kbd>
                    <span class="text-xs text-muted">{label}</span>
                  </>
                )}
              </For>
            </div>
          </section>
        </div>
      </div>
    </div>
  );
}

const HOME_ROW = "flex h-5.5 w-full items-center gap-2 px-3 text-left hover:bg-hover";

const FIRST_ASKS = [
  "Set up a workspace on kloudlite/rustic-git and run the test suite",
  "Read the open issues and propose a plan for the next release",
  "Fix the failing SLO probe and verify it on the fleet",
];

function FirstRun(props: { team: string; onPick: (text: string) => void }) {
  return (
    <div class="mb-6 flex flex-col gap-5 font-ui">
      <div>
        <h2 class="text-md font-medium">Your bench for {props.team}</h2>
        <p class="mt-1 max-w-[60ch] text-sm leading-relaxed text-muted">
          Say what you want done. The bench sets that as its goal, makes a plan, and opens the workspaces
          and agents it needs — you watch them appear on the left and read their work here.
        </p>
      </div>
      <div class="flex flex-col gap-1">
        <div class="text-2xs font-semibold uppercase text-subtle">Try</div>
        <For each={FIRST_ASKS}>
          {(t) => (
            <button class="flex h-6.5 w-fit max-w-full items-center gap-2 rounded-[2px] border border-btn2-line bg-btn2 px-3 text-left text-fg hover:bg-btn2-hover" onClick={() => props.onPick(t)}>
              <span class="text-accent">❯</span>
              <span class="truncate">{t}</span>
            </button>
          )}
        </For>
      </div>
    </div>
  );
}

const when = (ms: Message[]) => {
  const first = ms.find((m) => m.ts)?.ts;
  const last = [...ms].reverse().find((m) => m.ts)?.ts;
  if (!first || !last) return "";
  const d = new Date(first);
  const day = d.toDateString() === new Date().toDateString() ? "today" : d.toLocaleDateString(undefined, { month: "short", day: "numeric" });
  return `${day} ${new Date(first).toTimeString().slice(0, 5)}–${new Date(last).toTimeString().slice(0, 5)}`;
};

/** A sitting that is folded: one line saying what it held, opened by a click. */
function Folded(props: { messages: Message[]; onOpen: () => void }) {
  const prompts = () => props.messages.filter((m) => m.role === "user").length;
  const first = () => (props.messages.find((m) => m.role === "user") as { text?: string } | undefined)?.text ?? "";
  return (
    <button class="group -mx-3 flex items-center gap-3 rounded-sm px-3 py-1 text-left font-ui text-xs text-subtle hover:bg-hover hover:text-fg" onClick={props.onOpen} title="Open this sitting">
      <span class="h-px w-6 shrink-0 bg-line-subtle" />
      <span class="shrink-0 tabular-nums">{when(props.messages)}</span>
      <span class="shrink-0">· {prompts()} {prompts() === 1 ? "prompt" : "prompts"}</span>
      <span class="min-w-0 flex-1 truncate font-mono text-muted">{first()}</span>
      <span class="h-px flex-1 bg-line-subtle" />
    </button>
  );
}

/** Where an opened sitting begins, so the gap before it still reads. */
function SittingRule(props: { messages: Message[] }) {
  return (
    <div class="flex items-center gap-3 pt-2 font-ui text-xs text-subtle">
      <span class="h-px flex-1 bg-line-subtle" />
      <span class="tabular-nums">{when(props.messages)}</span>
      <span class="h-px flex-1 bg-line-subtle" />
    </div>
  );
}

/** Size the composer to its text: collapse first, so it shrinks as well as grows. */
export function fit(t: HTMLTextAreaElement) {
  t.style.height = "0";
  t.style.height = t.value ? `${t.scrollHeight}px` : "";
}

function Time(props: { at: string }) {
  return (
    <span class="shrink-0 pl-6 text-right text-xs leading-[inherit] whitespace-nowrap tabular-nums text-subtle">
      {props.at}
    </span>
  );
}

/**
 * A tool asking to run (spec §9). It reads as what it is — a question with an answer — rather than
 * as a dialog over the thread: the person says yes or no in the transcript, and the answer stays
 * there as the record of what was agreed to. Nothing changes on the platform until they do.
 */
function Question(props: { q: QuestionRow; session: string }) {
  const answered = () => props.q.answer;
  return (
    <div class="my-1 flex flex-col gap-2 rounded-[2px] border border-request-line bg-request px-3 py-2 font-ui text-sm">
      <div class="flex items-baseline gap-2">
        <span class="shrink-0 font-bold text-accent">?</span>
        <span class="min-w-0 flex-1 text-fg">{props.q.summary}</span>
        <Time at={props.q.at} />
      </div>
      <Show when={props.q.args && Object.keys(props.q.args).length}>
        <div class="grid grid-cols-[repeat(auto-fit,minmax(220px,1fr))] gap-x-6 gap-y-0.5 font-mono text-xs">
          <For each={Object.entries(props.q.args ?? {}).filter(([, v]) => v !== undefined && v !== "")}>
            {([k, v]) => (
              <div class="flex min-w-0 items-baseline gap-2">
                <span class="w-24 shrink-0 truncate text-subtle">{k}</span>
                <span class="min-w-0 truncate text-muted" title={typeof v === "object" ? JSON.stringify(v) : String(v)}>{typeof v === "object" ? JSON.stringify(v) : String(v)}</span>
              </div>
            )}
          </For>
        </div>
      </Show>
      <Show
        when={!answered()}
        fallback={<div class="text-xs text-subtle">{answered() === "yes" ? "you said yes" : "you said no"}</div>}
      >
        <div class="flex items-center gap-2">
          <Button size="sm" onClick={() => live.answerProposal(props.session, props.q.id, "yes")}>Yes</Button>
          <Button size="sm" variant="ghost" onClick={() => live.answerProposal(props.session, props.q.id, "no")}>No</Button>
          <span class="text-xs text-subtle">nothing changes until you answer</span>
        </div>
      </Show>
    </div>
  );
}

/** The verb, the clock and the tokens: a person watching an agent wants to know it is alive and on what. */
function StatusLine(props: { turn?: { verb: string; since: number; tokens: number } }) {
  const [now, setNow] = createSignal(Date.now());
  const t = setInterval(() => setNow(Date.now()), 1000);
  onCleanup(() => clearInterval(t));
  const secs = () => Math.max(0, Math.round((now() - (props.turn?.since ?? now())) / 1000));
  return (
    <div class="flex items-center gap-2 px-3 pb-2 font-mono text-sm text-muted">
      <span class="animate-pulse text-accent">✻</span>
      <span>{props.turn?.verb ?? "Thinking"}…</span>
      <span class="text-subtle tabular-nums">{secs()}s</span>
      <Show when={props.turn?.tokens}>{(n) => <span class="text-subtle tabular-nums">· {n() > 1000 ? `${Math.round(n() / 100) / 10}k` : n()} tokens</span>}</Show>
      <span class="text-subtle">(esc to interrupt · ^B to background a command)</span>
    </div>
  );
}

function Hint(props: { keys: string; children: string }) {
  return (
    <span class="inline-flex items-center gap-1.5 whitespace-nowrap">
      <Kbd>{props.keys}</Kbd>
      {props.children}
    </span>
  );
}

/**
 * A tool call, the way the terminal prints it: `⏺ Bash(cmd)` on one line, the
 * result hanging under it on a `⎿` rail — the first lines, then `… +N lines`
 * that opens on click. A call still running shows a dim dot; a failed one red.
 */
function Step(props: { a: Action }) {
  const [openOut, setOpenOut] = createSignal(false);
  const [showOut, setShowOut] = createSignal(true); // the call line folds its result away
  const HEAD = 5;
  const lines = () => (props.a.output ?? "").replace(/\s+$/, "").split("\n");
  const more = () => Math.max(0, lines().length - HEAD);
  const shown = () => (openOut() ? lines() : lines().slice(0, HEAD));
  const tool = () => (props.a.target && !/\s/.test(props.a.target) ? props.a.target : undefined);
  const tone = () => (props.a.pending ? "text-subtle" : props.a.ok === false ? "text-danger" : "text-success");
  return (
    <div class="flex flex-col leading-[18px]">
      {/* `⏺ Bash(cmd)` then `⎿  output` — the terminal's own shape, in the
          terminal's own face, nothing drawn that the terminal would not. */}
      <div class="flex cursor-pointer items-start" onClick={() => setShowOut((v) => !v)} title={showOut() ? "Fold the result" : "Show the result"}>
        <span class={`w-5 shrink-0 ${tone()}`} classList={{ "animate-pulse": props.a.pending }}>⏺</span>
        <span class="min-w-0 flex-1 wrap-words whitespace-pre-wrap text-fg">
          <Show when={tool()} fallback={<><Show when={props.a.target}><span class="font-bold text-fg-strong">{props.a.target} </span></Show>{props.a.text}</>}>
            <span class="font-bold text-fg-strong">{tool()}</span><span class="text-subtle">(</span>{props.a.text}<span class="text-subtle">)</span>
          </Show>
        </span>
      </div>
      <Show when={(props.a.output !== undefined || props.a.pending) && showOut()}>
        <div class="flex items-start" classList={{ "text-muted": props.a.ok !== false, "text-danger": props.a.ok === false }}>
          <span class="w-5 shrink-0 pl-2 text-subtle">⎿</span>
          <div class="min-w-0 flex-1 pl-1">
            <Show when={!props.a.pending} fallback={<span class="text-accent">Running…</span>}>
              <Show when={props.a.output} fallback={<span class="text-subtle">(No output)</span>}>
                <pre class="m-0 whitespace-pre-wrap wrap-words font-[inherit]">{shown().join("\n")}</pre>
                <Show when={more()}>
                  <button class="text-subtle hover:text-fg" onClick={(e) => (e.stopPropagation(), setOpenOut((v) => !v))}>
                    {openOut() ? "… collapse" : `… +${more()} ${more() === 1 ? "line" : "lines"}`}
                  </button>
                </Show>
              </Show>
            </Show>
          </div>
        </div>
      </Show>
    </div>
  );
}

/**
 * Assistant prose is markdown, as Claude Code renders it. While a message
 * streams, the parse runs on a short trailing timer rather than on every
 * token: re-rendering the whole block per token is what flickered, and a
 * reader cannot tell 80 ms from 0.
 */
function Prose(props: { text: string; latest?: boolean }) {
  // A long answer that is not the one being read folds to its head, with the
  // rest a click away; the latest answer is always whole.
  const LONG = 40;
  const lines = () => props.text.split("\n").length;
  const [openAll, setOpenAll] = createSignal(false);
  const folded = () => !props.latest && !openAll() && lines() > LONG;
  const shown = () => (folded() ? props.text.split("\n").slice(0, 16).join("\n") : props.text);
  const [html, setHtml] = createSignal(render(shown()));
  let timer: ReturnType<typeof setTimeout> | undefined;
  createEffect(() => {
    const text = shown();
    if (timer) return;
    timer = setTimeout(() => {
      timer = undefined;
      setHtml(render(untrack(shown)));
    }, 80);
    void text;
  });
  onCleanup(() => clearTimeout(timer));
  return (
    <div class="min-w-0 flex-1">
      <div class="prose" innerHTML={html()} />
      <Show when={folded()}>
        <button class="mt-1 font-mono text-sm text-subtle hover:text-fg" onClick={() => setOpenAll(true)}>… +{lines() - 16} lines</button>
      </Show>
      <Show when={openAll() && lines() > LONG}>
        <button class="mt-1 font-mono text-sm text-subtle hover:text-fg" onClick={() => setOpenAll(false)}>… collapse</button>
      </Show>
    </div>
  );
}

const render = (text: string) => marked.parse(text, { async: false, gfm: true, breaks: false }) as string;

