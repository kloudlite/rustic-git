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
import { report } from "./results/toolline";
import { elapsed, segments, timing, verb } from "./results/group";
import { TEXT_RENDER_PACE_MS, paced } from "./results/paced";
import { mentions } from "./results/mentions";
import { ContextGroup } from "./results/ContextGroup";
import { notification, spinnerMeta, summary, turnFooter, verbAt } from "./results/summary";
import { modeLine, modeParts, modelOfThread } from "../rows";
import { KEYS } from "../keys";
import { Spinner, Ticker, WorkingDots } from "./Motion";
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
  /** How many times escape has been pressed just now: two stops the turn. */
  const [escapes, setEscapes] = createSignal(0);
  let escTimer: ReturnType<typeof setTimeout> | undefined;
  const [dockOpen, setDockOpen] = createSignal(true);
  onCleanup(() => clearTimeout(escTimer));
  // The live state behind this thread: the bench's, a side session's, or an
  // idle one for a recorded thread (nothing arrives on it, so it stays quiet).
  const L = () => live.thread(thread()?.pi ?? "");
  /**
   * Only the newest rows are drawn. Opening a 22-prompt thread used to render every row before the
   * first paint — a markdown parse and a highlight pass each — and the pane sat blank for seconds
   * (owner, 2026-09-17). The rest are one muted line that loads them when asked.
   */
  const PAGE = 60;
  const [shown, setShown] = createSignal(PAGE);
  createEffect(() => (void thread()?.id, setShown(PAGE)));
  const earlier = () => Math.max(0, blocks().length - shown());
  const visible = createMemo(() => (earlier() ? blocks().slice(-shown()) : blocks()));
  let hist = -1;     // how far back ↑ has walked; -1 is the draft
  let draft = "";
  // `/` completion: the commands whose name starts with what is typed, shown
  // above the composer while the caret is still in the first word.
  const [typed, setTyped] = createSignal("");
  // The composer floats over the reversed scroller, so the last row — the turn footer — was under
  // it. Measured rather than guessed: it grows with what is typed and with the queued rows.
  let composerBox: HTMLDivElement | undefined;
  onMount(() => {
    if (!composerBox) return;
    const ro = new ResizeObserver(([e]) => scroller?.style.setProperty("--composer-h", `${Math.round(e.contentRect.height)}px`));
    ro.observe(composerBox);
    onCleanup(() => ro.disconnect());
  });
  // One clock for the footer bar, ticking only while something runs.
  const [now, setNow] = createSignal(Date.now());
  const clock = setInterval(() => setNow(Date.now()), 1000);
  onCleanup(() => clearInterval(clock));
  const elapsed = () => Math.max(0, Math.round((now() - (L().turn()?.since ?? now())) / 1000));
  /**
   * The model of THIS thread — a workspace session has its own row and its own model, and reading
   * only the bench's showed "no model" in a workspace tab (owner, 2026-09-17). pi's status line
   * first while it is up, then the row, then the bench's default.
   */
  const modelName = () => modelOfThread(thread()?.model ?? props.machine.model, live.benchModel());
  /** One string for the mode, the model and the level, used in all three places. */
  const line = () => modeLine(live.mode(), modelName(), live.levelKnown() ? live.level() : undefined);
  /** Where this thread works, as the footer says it: `~/workspaces/<name>` or the bench's own dir. */
  const where = () => {
    const t = thread();
    if (!t) return "~";
    return t.kind === "workspace" || t.kind === "ephemeral" ? `~/workspaces/${t.name}` : "~/workspaces/bench";
  };
  const parts = () => modeParts(live.mode(), modelName(), live.levelKnown() ? live.level() : undefined);
  const [pick, setPick] = createSignal(0);
  /** Whether escape has shut the command list; typing opens it again. */
  const [closed, setClosed] = createSignal(false);
  const matches = createMemo(() => {
    const m = /^(\/[a-z-]*)$/i.exec(typed());
    return m && !closed() ? props.commands.filter((c) => c.name.startsWith(m[1].toLowerCase())) : [];
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
              class="relative flex h-[35px] max-w-60 min-w-32 items-center gap-1.5 border-r border-line pr-2 pl-3 whitespace-nowrap text-muted hover:bg-bg aria-selected:-mb-px aria-selected:bg-bg aria-selected:text-fg aria-selected:before:absolute aria-selected:before:inset-x-0 aria-selected:before:top-0 aria-selected:before:h-px aria-selected:before:bg-focus"
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
              class="relative flex h-[35px] max-w-60 min-w-32 items-center gap-1.5 border-r border-line px-3 whitespace-nowrap text-muted aria-selected:-mb-px aria-selected:bg-bg aria-selected:text-fg aria-selected:before:absolute aria-selected:before:inset-x-0 aria-selected:before:top-0 aria-selected:before:h-px aria-selected:before:bg-focus"
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
            class="relative flex h-[35px] max-w-60 min-w-32 items-center gap-1.5 border-r border-line px-3 whitespace-nowrap text-muted aria-selected:-mb-px aria-selected:bg-bg aria-selected:text-fg aria-selected:before:absolute aria-selected:before:inset-x-0 aria-selected:before:top-0 aria-selected:before:h-px aria-selected:before:bg-focus"
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

      <div class="flex h-5.5 items-center gap-1 px-4 whitespace-nowrap text-fg/80">
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
          <span class="mr-2 inline-flex items-center gap-1 text-warning" title="Showing what was last seen; nothing can be sent until the bench is back">
            <span class="size-1.5 rounded-full bg-warning" /> bench offline
          </span>
        </Show>
        <Show when={readonly()}>
          <span class="inline-flex items-center gap-1 text-muted">
            <Icon name="lock" size={12} /> read-only
          </span>
        </Show>
        </Show>
      </div>

      </Show>

      <Show when={props.env} keyed>
        {(env) => <EnvironmentPage env={env} snapshots={props.snapshots} followed={env.id === props.machine.environmentId} />}
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
      <div class="grid min-h-0 min-w-0 grid-rows-[minmax(0,1fr)_auto_auto] font-mono">
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
          class="pane relative flex min-w-0 flex-col-reverse overflow-x-clip overflow-y-auto px-9 pt-5 select-text"
          style={{ "padding-bottom": `calc(var(--composer-h, 0px) + 16px)` }}
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
            {/* `# session` left, what it has spent right: the session view's own header. */}
            <div class="flex items-baseline gap-3 pb-2 font-mono text-subtle" classList={{ hidden: blocks().length === 0 }}>
              <span class="min-w-0 truncate text-fg-strong">
                <span class="text-subtle"># </span>
                {thread()?.kind === "machine" ? "bench" : thread()?.kind === "ephemeral" ? "agent" : thread()?.kind === "btw" ? "fork · read-only" : thread()?.name}
              </span>
              <span class="flex-1" />
              <Show when={L().spend().tokens}>
                {(n) => (
                  <span class="shrink-0 tabular-nums">
                    {n() > 1000 ? `${Math.round(n() / 100) / 10}k` : n()} tokens
                    <Show when={L().spend().context}>{(w) => <> · {Math.min(100, Math.round((n() / w()) * 100))}%</>}</Show>
                    <Show when={L().spend().cost}>{(c) => <> · ${c().toFixed(2)}</>}</Show>
                  </span>
                )}
              </Show>
            </div>
            {/* Rows just continue; what is older is one line, not a divider with a summary. */}
            <Show when={earlier()}>
              {(n) => (
                <button class="w-fit text-left text-subtle hover:text-fg" onClick={() => setShown((v) => v + PAGE * 4)}>
                  ↑ {n()} earlier {n() === 1 ? "message" : "messages"}
                </button>
              )}
            </Show>
            <For each={segments(visible())}>
              {(seg) => (
                <Show when={seg.kind === "one"} fallback={
                  seg.kind === "context"
                    ? <ContextGroup rows={(seg as { rows: Action[] }).rows} />
                    : <ToolGroup rows={(seg as { rows: Action[] }).rows} />
                }>
                {(() => { const b = (seg as { row: Message }).row; return (
                <Show when={b.role !== "divider"} fallback={<Divider text={(b as { text: string }).text} />}>
                <Show when={b.role !== "question"} fallback={<Question q={b as QuestionRow} session={L().id} onChat={(t) => {
                  const c = scroller?.closest("main")?.querySelector<HTMLTextAreaElement>("textarea[data-composer]");
                  if (c) (c.value = t, fit(c), c.focus());
                }} />}>
                <Show when={b.role !== "action"} fallback={<div class="[contain:layout_style]"><Show when={(b as Action).tool} fallback={<Step a={b as Action} />}><ToolCall a={b as Action} /></Show></div>}>
                  {/* A prompt is a command and reads like one — an accent rail and a `>` — and an
                      answer is plain text beside it; the two turns are told apart by shape. */}
                  <Show
                    when={b.role === "user" && !notification((b as { text: string }).text)}
                    /* A message the HARNESS delivered is a row of its own — an agent reporting, a
                       command finishing, a question answered — never a prompt the person appears
                       to have typed. An answer is plain text. */
                    fallback={
                      <Show when={b.role === "user"} fallback={
                      <Show
                        when={(b as { kind?: string }).kind !== "reasoning"}
                        /* Thinking is the model working, not what it decided: plain, dimmed, apart
                           (`message-part.tsx:1759` — no header, no fold on the web). */
                        fallback={
                          <div data-component="reasoning-part" class="flex items-start text-muted italic">
                            <Prose text={(b as { text: string }).text} latest={false} />
                          </div>
                        }
                      >
                      <div data-component="text-part" class="group/text flex flex-col">
                        <div data-slot="text-part-body" class="flex items-start">
                          <Prose text={(b as { text: string }).text} latest={b === blocks()[blocks().length - 1]} />
                          {/* Copy the answer itself — the one action on a text part (`:1654`). */}
                          <Copy text={(b as { text: string }).text} />
                        </div>
                        {/* After every assistant turn: what answered, on what, in how long. */}
                        <div class="pt-1 font-mono text-subtle">
                          ✻&nbsp; {[
                            (b as { ms?: number }).ms ? turnFooter((b as { ms: number }).ms, new Date((b as { ts?: number }).ts ?? Date.now()), live.procs.filter((p) => !p.ended).length) : line(),
                            (b as { interrupted?: true }).interrupted ? "Interrupted" : "",
                          ].filter(Boolean).join(" · ")}
                        </div>
                      </div>
                      </Show>
                      }>
                        {(() => {
                          const n = notification((b as { text: string }).text)!;
                          return (
                            <div class="flex flex-col">
                              <div class="flex min-w-0 items-baseline gap-2">
                                <span class="w-4 shrink-0 text-success">⏺</span>
                                <span class="min-w-0 flex-1 truncate text-fg">{n.verb}</span>
                                <Show when={report((b as { text: string }).text).status}>{(st) => <span class="shrink-0 text-muted">{st()}</span>}</Show>
                                <Show when={report((b as { text: string }).text).left}>{(l) => <span class="shrink-0 rounded-[2px] bg-fg/10 px-1 text-muted">{l()}</span>}</Show>
                                <Time at={(b as { at: string }).at} />
                              </div>
                              <Show when={n.detail}>
                                {(d) => (
                                  <div class="flex min-w-0 items-baseline gap-1 pl-4 text-muted">
                                    <span class="shrink-0 text-subtle">⎿</span>
                                    <span class="min-w-0 truncate">{d()}</span>
                                  </div>
                                )}
                              </Show>
                            </div>
                          );
                        })()}
                      </Show>
                    }
                  >
                    <div class="flex flex-col font-mono">
                      <div class="-mx-3 flex items-start border-l-2 border-request-line bg-request px-3 py-2">
                        <span class="w-4 shrink-0 font-bold text-accent">&gt;</span>
                        <span class="min-w-0 flex-1 wrap-words whitespace-pre-wrap text-fg">
                          {/* An answer a workspace sent back arrives as a prompt; the workspace is a label, not the message. */}
                          <Show when={fromWorkspace((b as { text: string }).text)}>
                            {(w) => <span class="mr-1.5 rounded-[2px] bg-fg/10 px-1 text-subtle">{w()}</span>}
                          </Show>
                          <For each={mentions(said((b as { text: string }).text))}>
                            {(seg) => (
                              <Show when={seg.type !== "text"} fallback={seg.text}>
                                <span data-highlight={seg.type} class="rounded-[2px] bg-accent/15 px-0.5 text-accent">{seg.text}</span>
                              </Show>
                            )}
                          </For>
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
                </Show>
                ); })()}
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
          {/* The commands, as opencode lists them: full width ABOVE the composer, two columns on
              the cell grid, the selected row a solid accent bar with dark text — no border, no
              rounding, nothing floating. Ten rows, then it scrolls; the `/` stays in the input. */}
          <Show when={matches().length}>
            <div data-slot="slash-menu" class="pane mb-1 max-h-[200px] min-w-0 overflow-y-auto font-mono">
              <For each={matches()}>
                {(c, i) => (
                  <button
                    class="flex w-full items-baseline gap-4 px-6 text-left"
                    classList={{ "bg-accent text-bg": i() === pick(), "text-fg": i() !== pick() }}
                    onMouseMove={() => setPick(i())}
                    onMouseDown={(e) => { e.preventDefault(); accept(e.currentTarget.closest("main")!.querySelector<HTMLTextAreaElement>("textarea[data-composer]")!, i()); }}
                  >
                    <span class="w-28 shrink-0">{c.name}</span>
                    <span class="min-w-0 flex-1 truncate" classList={{ "text-muted": i() !== pick() }}>{c.help}</span>
                  </button>
                )}
              </For>
            </div>
          </Show>
          <Show when={behind()}>
            <button
              class="absolute -top-9 left-1/2 z-10 flex h-7 -translate-x-1/2 items-center gap-1.5 rounded-[2px] border border-widget-line bg-overlay px-3 text-fg shadow-overlay hover:bg-hover"
              onClick={toBottom}
            >
              <Icon name="chevronDown" size={12} /> Jump to latest
            </button>
          </Show>
          {/* What this session is waiting on: lines pi still holds, and asks a workspace has not
              answered yet. The asks were only ever in the bench's exchange log, so nothing showed
              them — a person could not tell a queued ask from a lost one. */}
          <Show when={L().queue.length || live.asksOf(L().id).length}>
            <div class="mb-2 flex flex-col gap-1 px-3 font-mono">
              {/* The dock says how many are waiting and shows the first when it is shut
                  (`session-followup-dock.tsx:24`); a queue of six must not push the composer down. */}
              <Show when={L().queue.length > 1}>
                <button class="flex items-baseline gap-2 text-subtle hover:text-fg" onClick={() => setDockOpen((v) => !v)}>
                  <Icon name={dockOpen() ? "chevronDown" : "chevronRight"} size={12} />
                  {L().queue.length} queued
                  <Show when={!dockOpen()}><span class="min-w-0 truncate text-muted">{L().queue[0].text}</span></Show>
                </button>
              </Show>
              <For each={dockOpen() || L().queue.length <= 1 ? L().queue : []}>
                {(q) => (
                  <div class="group/q arrive flex flex-col">
                    <div class="flex items-start gap-2 text-muted">
                      <span class="arrive shrink-0 rounded-[2px] bg-fg/10 px-1 text-subtle" title={q.how === "steer" ? "steers the turn" : "waits its turn"}>QUEUED</span>
                      <span class="min-w-0 flex-1 truncate">{q.text}</span>
                      <button class="shrink-0 text-subtle opacity-0 group-hover/q:opacity-100 hover:text-fg" onClick={() => live.sendNow(L().id, q.text)}>Send now</button>
                      <button
                        class="shrink-0 text-subtle opacity-0 group-hover/q:opacity-100 hover:text-fg"
                        onClick={() => {
                          const c = scroller?.closest("main")?.querySelector<HTMLTextAreaElement>("textarea[data-composer]");
                          if (c) (c.value = q.text, fit(c), c.focus());
                        }}
                      >
                        Edit
                      </button>
                    </div>
                    {/* Why it is where it is: a queue that reorders itself without saying why is a mystery. */}
                    <Show when={q.reason}>{(r) => <span class="pl-7 text-subtle">{r()}</span>}</Show>
                  </div>
                )}
              </For>
              <For each={live.asksOf(L().id)}>
                {(a) => (
                  <div class="flex items-start gap-2 text-muted">
                    <span class="w-5 shrink-0 text-subtle">›</span>
                    <span class="shrink-0 text-subtle">{a.state}</span>
                    <span class="shrink-0 rounded-[2px] bg-fg/10 px-1 text-muted">{a.workspace}</span>
                    <span class="min-w-0 flex-1 truncate">{a.text.replace(/^\[ask \S+ from [^\]]*\] /, "")}</span>
                  </div>
                )}
              </For>
            </div>
          </Show>


          {/* The composer is a block with the same accent rail a person's message has: what you
              type and what you typed read as the same thing. */}
          {/* `px-4 py-3` is opencode's own composer padding
              (`packages/app/src/pages/session/composer/session-composer-region.tsx:102`), and the
              rail is the theme's accent rather than a blue of its own. */}
          <div ref={composerBox} class="pane flex flex-col border-l-2 border-accent bg-input transition-[border-color] duration-[var(--motion)] ease-out-quick focus-within:border-focus">
            <div class="flex items-start px-4 pt-3 pb-1.5 font-mono">
              <span class="w-4 shrink-0 text-accent">❯</span>
              {/* Grows with what is typed, up to a cap, then scrolls: ↩ sends,
                  ⇧↩ is a newline, so a long prompt is still written in place. */}
              <textarea
                data-composer
                disabled={thread()?.kind === "machine" && !thread()?.pi}
                rows="1"
                class="max-h-60 min-h-5 flex-1 resize-none border-0 bg-transparent p-0 outline-none placeholder:text-subtle"
                placeholder={thread()?.kind === "machine" && !thread()?.pi ? "no session yet · start one with + beside Sessions" : thread()?.pi && !live.connected() ? "not connected" : thread()?.kind === "btw" ? "ask about the bench's work · nothing here changes anything" : readonly() ? "ask or discuss · this thread cannot change anything" : "tell the bench what to do"}
                onInput={(e) => (fit(e.currentTarget), setTyped(e.currentTarget.value), setPick(0), setClosed(false), (hist = -1))}
                onKeyDown={(e) => {
                  // Completion first: while suggestions show, ↑/↓ move, ⇥ and ↩
                  // take one (↩ sends only when the word is already complete).
                  if (matches().length) {
                    const t = e.currentTarget;
                    if (e.key === "ArrowDown") return (e.preventDefault(), setPick((p) => (p + 1) % matches().length));
                    if (e.key === "ArrowUp") return (e.preventDefault(), setPick((p) => (p - 1 + matches().length) % matches().length));
                    if (e.key === "Tab") return (e.preventDefault(), void accept(t));
                    if (e.key === "Escape") {
                      e.preventDefault();
                      // Escape closes the list first: the `/` a person typed is theirs to keep.
                      if (!closed()) return setClosed(true);
                      // While a turn runs, escape means STOP — but only on the second press
                      // (`prompt/index.tsx:408`); the counter forgets after two seconds.
                      if (L().busy()) {
                        if (escapes() >= 1) { setEscapes(0); return live.interrupt(L().id); }
                        setEscapes(1);
                        clearTimeout(escTimer);
                        escTimer = setTimeout(() => setEscapes(0), 2000);
                        return;
                      }
                      return setTyped("");
                    }
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
            {/* One blank line, then the status row: mode, what answers, and how hard it thinks.
                This is the ONLY status under the composer — the turn footer belongs to a finished
                turn in the transcript, and two of them said one fact twice (owner, 2026-09-17). */}
            <div data-slot="composer-status" class="flex min-w-0 items-center gap-1.5 px-4 pt-3 pb-3 font-mono">
              <span class="shrink-0 text-accent">{parts().mode}</span>
              <span class="shrink-0 text-subtle">·</span>
              <span class="shrink-0 text-fg" title={L().status()}>{parts().model}</span>
              <Show when={parts().provider}>{(p) => <span class="shrink-0 text-subtle">{p()}</span>}</Show>
              <Show when={parts().level}>
                {(l) => (
                  <>
                    <span class="shrink-0 text-subtle">·</span>
                    <span class="shrink-0 font-bold text-warning">{l()}</span>
                  </>
                )}
              </Show>
            </div>
          </div>
          {/* The footer bar: what is running, how to stop it, and the keys — one line, always there. */}
          {/* The footer bar: what is running and how to stop it on the left, what it has spent and
              the way to the commands on the right. Idle it says where you are instead. */}
          <div class="pane flex min-w-0 flex-wrap items-center gap-x-3.5 gap-y-1 px-3 pt-1.5 font-mono text-subtle">
            <Show
              when={L().busy()}
              /* Idle, the left of the footer says WHERE you are — the session's own path — not the
                 mode again (opencode's `footer.tsx:52`, `directory()` in the muted text). */
              fallback={<span class="min-w-0 truncate" title={thread()?.name}>{where()}</span>}
            >
              {/* A retry says what failed and which attempt this is; `retrying - attempt #2`. */}
              <Show when={L().retry()}>
                {(r) => (
                  <span class="min-w-0 truncate text-warning" title={r().message}>
                    {r().message.slice(0, 80)}{r().message.length > 80 ? "…" : ""} [retrying · attempt #{r().attempt}]
                  </span>
                )}
              </Show>
              <Spinner class="text-accent" />
              <WorkingDots class="shrink-0" />
              <span class="text-muted">{verbAt(elapsed())}…</span>
              <Ticker class="tabular-nums" value={`(${spinnerMeta(elapsed(), L().turn()?.tokens)})`} />
              {/* `esc interrupt`, then `esc again to interrupt` once it has been pressed once. */}
              <span classList={{ "text-accent": escapes() > 0 }}>esc <span class="text-subtle" classList={{ "text-accent": escapes() > 0 }}>{escapes() > 0 ? "again to interrupt" : "interrupt"}</span></span>
            </Show>
            <span class="flex-1" />
            <Show when={L().spend().tokens}>
              {(n) => (
                <span class="flex shrink-0 items-center gap-1.5 tabular-nums">
                  {/* How full the window is, drawn: a ring beside the number, 16px (§2.1). */}
                  <Show when={L().spend().context}>{(w) => <ProgressCircle pct={Math.min(100, Math.round((n() / w()) * 100))} />}</Show>
                  {live.usage(n(), L().spend().cost, L().spend().context)}
                </span>
              )}
            </Show>
            {/* The one key the footer names, the way opencode names it: the chord in the text
                colour, what it does muted. */}
            <span class="shrink-0"><span class="font-bold text-fg">{KEYS.palette.keys.toLowerCase().replace("^", "ctrl+")}</span> <span class="text-subtle">commands</span></span>
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
            <div class="font-semibold uppercase text-subtle">{props.team} · {props.machine.owner.split("@")[0]}</div>
            <h1 class="mt-1 font-medium">Bench</h1>
            <p class="mt-1 truncate text-muted">{props.machine.goal || "No goal yet. The first message sets it."}</p>
          </div>
          <Button icon="workspace" onClick={props.onSwitch} title="⌘T">Open a workspace</Button>
        </header>

        <div class="grid grid-cols-[minmax(0,1fr)_240px] gap-12">
          <section>
            <div class="mb-1.5 font-semibold uppercase text-subtle">Open</div>
            <div class="flex flex-col">
              <button class={HOME_ROW} onClick={() => props.onGo(props.machine.id)}>
                <Icon name="machine" size={14} class="shrink-0 text-accent" />
                <span class="text-fg">Bench Thread</span>
                <span class="min-w-0 flex-1 truncate text-subtle">the thread that changes things</span>
              </button>
              <For each={props.machine.workspaces}>
                {(w) => (
                  <button class={HOME_ROW} onClick={() => props.onGo(w.id)}>
                    <Icon name="workspace" size={14} class={`shrink-0 ${w.state === "running" ? "text-accent" : "text-subtle"}`} />
                    <span class="font-mono text-fg">{w.name}</span>
                    <span class="min-w-0 flex-1 truncate font-mono text-subtle">{w.repo} · {w.branch}</span>
                    <Show when={running(w)}>
                      {(n) => <span class="inline-flex items-center gap-1.5 text-muted"><span class="size-1.5 rounded-full bg-success" />{n()} running</span>}
                    </Show>
                  </button>
                )}
              </For>
            </div>
          </section>

          <section>
            <div class="mb-1.5 font-semibold uppercase text-subtle">Keys</div>
            <div class="grid grid-cols-[auto_minmax(0,1fr)] items-center gap-x-3 gap-y-1">
              <For each={KEYS_SHOWN}>
                {([k, label]) => (
                  <>
                    <Kbd>{k}</Kbd>
                    <span class="text-muted">{label}</span>
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
    <div class="mb-6 flex flex-col gap-4 font-mono">
      {/* The wordmark, then what to type: an empty session says what it is for, not what it can do. */}
      <div class="pt-6 text-center">
        <div class="font-bold tracking-wide text-fg-strong">kloudlite</div>
        <div class="pt-1 text-subtle">your bench for {props.team}</div>
      </div>
      <div class="flex flex-col gap-1">
        <For each={FIRST_ASKS}>
          {(t) => (
            <button class="flex w-fit max-w-full items-baseline gap-2 text-left text-muted hover:text-fg" onClick={() => props.onPick(t)}>
              <span class="text-accent">❯</span>
              <span class="truncate">{t}</span>
            </button>
          )}
        </For>
      </div>
      <div class="text-subtle">● Tip: tab switches Build and Plan · ctrl+p for commands · / for a command by name</div>
    </div>
  );
}


/** A sitting that is folded: one line saying what it held, opened by a click. */
function Time(props: { at: string }) {
  return (
    <span class="shrink-0 pl-6 text-right leading-[inherit] whitespace-nowrap tabular-nums text-subtle">
      {props.at}
    </span>
  );
}

/**
 * A tool asking to run (spec §9). It reads as what it is — a question with an answer — rather than
 * as a dialog over the thread: the person says yes or no in the transcript, and the answer stays
 * there as the record of what was agreed to. Nothing changes on the platform until they do.
 */
function Question(props: { q: QuestionRow; session: string; onChat?: (text: string) => void }) {
  const answered = () => props.q.answer;
  // Claude Code's shape (§21): a `☐ header` card, numbered options with descriptions, then two
  // ways out of the choices — say something of your own, or talk about the question itself.
  const OPTIONS = () =>
    props.q.ask?.options?.length
      ? props.q.ask.options.map((o) => ({ key: o.label, label: o.label, hint: o.description }))
      : [{ key: "yes", label: "Yes", hint: "do it now" }, { key: "no", label: "No", hint: "leave it alone" }];
  const [pick, setPick] = createSignal(0);
  const [typing, setTyping] = createSignal(false);
  const [own, setOwn] = createSignal("");
  const answer = (a: string) => live.answerProposal(props.session, props.q.id, a);
  const rows = () => OPTIONS().length + 2;
  const take = (i: number) => {
    if (i < OPTIONS().length) return answer(OPTIONS()[i].key);
    if (i === OPTIONS().length) return setTyping(true);
    // "Chat about this": the question goes back as an ordinary prompt, and the card is done with.
    answer("no");
    props.onChat?.(props.q.summary);
  };
  return (
    <div class="my-1 flex flex-col gap-1 border-l-2 border-request-line bg-request px-3 py-2 font-mono"
      tabindex={0}
      onKeyDown={(e) => {
        if (answered()) return;
        if (e.key === "ArrowDown") return (e.preventDefault(), setPick((p) => (p + 1) % rows()));
        if (e.key === "ArrowUp") return (e.preventDefault(), setPick((p) => (p - 1 + rows()) % rows()));
        if (e.key === "Enter" && !typing()) return (e.preventDefault(), take(pick()));
        if (e.key === "Escape") return (e.preventDefault(), answer("no"));
      }}
    >
      <div class="flex items-baseline gap-2">
        <span class="shrink-0 text-accent">☐</span>
        <span class="min-w-0 flex-1 text-fg-strong">{props.q.ask?.header ?? "Confirm"}</span>
        <Time at={props.q.at} />
      </div>
      <div class="pl-5 text-fg">{props.q.summary}</div>
      <Show when={props.q.args && Object.keys(props.q.args).length}>
        <div class="grid grid-cols-[repeat(auto-fit,minmax(220px,1fr))] gap-x-6 gap-y-0.5 pl-5">
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
      <Show when={!answered()} fallback={<div class="pl-5 text-subtle">you said {answered()}</div>}>
        <div class="flex flex-col pl-5">
          <For each={OPTIONS()}>
            {(o, i) => (
              <button class="flex items-baseline gap-2 text-left" classList={{ "text-fg": pick() === i(), "text-muted": pick() !== i() }} onMouseEnter={() => setPick(i())} onClick={() => take(i())}>
                <span class="shrink-0 text-subtle">{i() + 1}.</span>
                <span class="shrink-0">{o.label}</span>
                <span class="min-w-0 truncate text-subtle">{o.hint}</span>
              </button>
            )}
          </For>
          <Show
            when={typing()}
            fallback={
              <button class="flex items-baseline gap-2 text-left" classList={{ "text-fg": pick() === OPTIONS().length, "text-muted": pick() !== OPTIONS().length }} onMouseEnter={() => setPick(OPTIONS().length)} onClick={() => setTyping(true)}>
                <span class="shrink-0 text-subtle">{OPTIONS().length + 1}.</span>
                <span>Type something.</span>
              </button>
            }
          >
            <div class="flex items-baseline gap-2">
              <span class="shrink-0 text-subtle">{OPTIONS().length + 1}.</span>
              <input
                autofocus
                class="min-w-0 flex-1 bg-transparent text-fg outline-none placeholder:text-subtle"
                placeholder="your answer"
                value={own()}
                onInput={(e) => setOwn(e.currentTarget.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter" && own().trim()) return (e.preventDefault(), e.stopPropagation(), answer(own().trim()));
                  if (e.key === "Escape") return (e.preventDefault(), e.stopPropagation(), setTyping(false));
                }}
              />
            </div>
          </Show>
          <button class="flex items-baseline gap-2 text-left" classList={{ "text-fg": pick() === OPTIONS().length + 1, "text-muted": pick() !== OPTIONS().length + 1 }} onMouseEnter={() => setPick(OPTIONS().length + 1)} onClick={() => take(OPTIONS().length + 1)}>
            <span class="shrink-0 text-subtle">{OPTIONS().length + 2}.</span>
            <span>Chat about this</span>
          </button>
          <div class="pt-1 text-subtle">Enter to select · ↑/↓ to navigate · Esc to cancel</div>
        </div>
      </Show>
    </div>
  );
}

export function fit(t: HTMLTextAreaElement) {
  t.style.height = "0";
  t.style.height = t.value ? `${t.scrollHeight}px` : "";
}

/**
 * Several tool calls of one turn, as one row. pi runs them concurrently, so they started together
 * and they are read together: the group says what is happening and for how long, each sub-row says
 * where it got to, and a finished group is one line again.
 */
function ToolGroup(props: { rows: Action[] }) {
  const [now, setNow] = createSignal(Date.now());
  const t = setInterval(() => setNow(Date.now()), 500);
  onCleanup(() => clearInterval(t));
  const state = () => timing(props.rows, now());
  const [open, setOpen] = createSignal(true);
  // A finished group folds itself away; a failure keeps it open, like a single row does.
  createEffect(() => !state().running && !props.rows.some((r) => r.ok === false) && setOpen(false));
  return (
    <div class="flex flex-col">
      <button class="group flex w-full items-baseline gap-2 py-px text-left" onClick={() => setOpen((v) => !v)}>
        <span class={`w-4 shrink-0 ${state().running ? "text-accent" : props.rows.some((r) => r.ok === false) ? "text-danger" : "text-subtle"}`} classList={{ "animate-pulse": state().running }}>●</span>
        {/* What is happening, as a sentence: "Reading 1 file, listing 1 directory…", and the same
            sentence in the past tense once it is over. */}
        <span class="min-w-0 flex-1 truncate text-muted">
          {summary(props.rows, !state().running)}
          <span class="text-subtle"> · {elapsed(state().ms)}{state().running ? "…" : ""}</span>
        </span>
        <Icon name={open() ? "chevronDown" : "chevronRight"} size={14} class="shrink-0 text-subtle opacity-40 group-hover:opacity-100" />
      </button>
      <Show when={open()}>
        <div class="flex flex-col pl-2">
          <For each={props.rows}>
            {(r) => (
              <div class="flex min-w-0 items-baseline gap-1">
                <span class="shrink-0 text-subtle">└</span>
                <span class="min-w-0 flex-1"><ToolCall a={r} /></span>
              </div>
            )}
          </For>
        </div>
      </Show>
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
 * A line from the harness itself — a note, a background task reporting in — in the same shape as
 * every other row in the pane: `~ harness: pi exited (1)`, with whatever else it had behind a
 * chevron. No bullet, no bold label, no parentheses round the message.
 */
function Step(props: { a: Action }) {
  const [open, setOpen] = createSignal(false);
  const lines = () => (props.a.output ?? "").replace(/\s+$/, "").split("\n");
  // One muted line, the shape every other row in the pane has: a glyph, who, what. The `●` rail and
  // the bold label were the last of the old shapes (owner, 2026-09-17: `● harness(pi exited (1))`).
  return (
    <div class="flex flex-col">
      <button class="group flex w-full items-baseline gap-2 py-px text-left" onClick={() => setOpen((v) => !v)}>
        <span class={`w-4 shrink-0 ${props.a.pending ? "text-accent" : props.a.ok === false ? "text-danger" : "text-subtle"}`} classList={{ "animate-pulse": props.a.pending }}>~</span>
        <span class="min-w-0 flex-1 truncate text-muted">
          <Show when={props.a.target}>{(t) => <span class="text-fg">{t().toLowerCase()}: </span>}</Show>
          {props.a.text}
        </span>
        <Show when={props.a.output}>
          <Icon name={open() ? "chevronDown" : "chevronRight"} size={14} class="shrink-0 text-subtle opacity-40 group-hover:opacity-100" />
        </Show>
      </button>
      <Show when={open() && props.a.output}>
        <pre class="m-0 pl-6 whitespace-pre-wrap wrap-words font-[inherit] text-muted [tab-size:4]">{lines().join("\n")}</pre>
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
/**
 * How full the context window is, as a 16px ring (`SessionProgressIndicatorV2`): a percentage is a
 * number to read, a ring is a glance. It warns past 70 and turns dangerous past 90, like every
 * other proportion in this app.
 */
function ProgressCircle(props: { pct: number }) {
  const r = 6;
  const c = 2 * Math.PI * r;
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" class="shrink-0" aria-label={`${props.pct}% of the context window`}>
      <circle cx="8" cy="8" r={r} fill="none" stroke="currentColor" stroke-width="2" class="text-fg/15" />
      <circle
        cx="8" cy="8" r={r} fill="none" stroke="currentColor" stroke-width="2"
        stroke-dasharray={`${(c * Math.min(100, props.pct)) / 100} ${c}`}
        style={{ transition: "stroke-dasharray var(--motion-body) var(--ease-emphasis)" }}
        transform="rotate(-90 8 8)"
        class={props.pct >= 90 ? "text-danger" : props.pct >= 70 ? "text-warning" : "text-accent"}
      />
    </svg>
  );
}

/**
 * A line across the transcript — "Session compacted", "Interrupted"
 * (`MessageDivider`, `message-part.tsx:1635`). The turn did not say this; it happened to the turn.
 */
function Divider(props: { text: string }) {
  return (
    <div data-component="compaction-part" class="flex items-center gap-2 py-1 text-subtle">
      <span data-slot="compaction-part-line" class="h-px flex-1 bg-fg/10" />
      <span data-slot="compaction-part-label" class="shrink-0">{props.text}</span>
      <span data-slot="compaction-part-line" class="h-px flex-1 bg-fg/10" />
    </div>
  );
}

/** The one action on an answer: take it. Shows only on hover, and says so once it has. */
function Copy(props: { text: string }) {
  const [done, setDone] = createSignal(false);
  return (
    <button
      class="ml-2 shrink-0 self-start text-subtle opacity-0 group-hover/text:opacity-100 hover:text-fg"
      title="Copy"
      onClick={() => {
        void navigator.clipboard.writeText(props.text);
        setDone(true);
        setTimeout(() => setDone(false), 1200);
      }}
    >
      {done() ? "copied" : "copy"}
    </button>
  );
}

function Prose(props: { text: string; latest?: boolean; streaming?: boolean }) {
  const streaming = () => props.streaming !== false;
  // A long answer that is not the one being read folds to its head, with the
  // rest a click away; the latest answer is always whole.
  const LONG = 40;
  const lines = () => props.text.split("\n").length;
  const [openAll, setOpenAll] = createSignal(false);
  const folded = () => !props.latest && !openAll() && lines() > LONG;
  const shown = () => (folded() ? props.text.split("\n").slice(0, 16).join("\n") : props.text);
  /**
   * Streaming text grows by WORDS on a 24 ms tick (opencode's `createPacedValue`,
   * `message-part.tsx:252`): raw deltas lurch, and a page that catches up one word at a time reads
   * like writing. A burst over 512 characters, a rewrite, or a finished message lands whole.
   */
  const [paced_, setPaced] = createSignal(shown());
  let timer: ReturnType<typeof setTimeout> | undefined;
  const tick = () => {
    timer = undefined;
    const want = untrack(shown);
    const have = untrack(paced_);
    const nextText = paced(want, have, !!props.latest && untrack(streaming));
    if (nextText === undefined) return;
    setPaced(nextText);
    if (nextText.length < want.length) timer = setTimeout(tick, TEXT_RENDER_PACE_MS);
  };
  createEffect(() => {
    void shown();
    if (!timer) timer = setTimeout(tick, TEXT_RENDER_PACE_MS);
  });
  onCleanup(() => clearTimeout(timer));
  const html = () => render(paced_());
  return (
    <div class="min-w-0 flex-1">
      <div class="prose" innerHTML={html()} />
      <Show when={folded()}>
        <button class="mt-1 font-mono text-subtle hover:text-fg" onClick={() => setOpenAll(true)}>… +{lines() - 16} lines</button>
      </Show>
      <Show when={openAll() && lines() > LONG}>
        <button class="mt-1 font-mono text-subtle hover:text-fg" onClick={() => setOpenAll(false)}>… collapse</button>
      </Show>
    </div>
  );
}

/**
 * Markdown, parsed once per text. A transcript re-renders on every event — a new row, a token, a
 * plan change — and re-parsing every answer each time is what made a long thread crawl. The cache
 * is bounded because a streaming answer makes one entry per token otherwise.
 */
const PARSED = new Map<string, string>();
const render = (text: string) => {
  const had = PARSED.get(text);
  if (had !== undefined) return had;
  const html = marked.parse(text, { async: false, gfm: true, breaks: false }) as string;
  if (PARSED.size > 400) PARSED.clear();
  PARSED.set(text, html);
  return html;
};

