import { For, Show, createMemo, type JSX } from "solid-js";
import { Icon } from "../ui/Icon";
import { Button } from "../ui/Button";
import { Badge } from "../ui/Badge";
import { Kbd } from "../ui/parts";
import { EnvironmentPage } from "./EnvironmentPage";
import { FileView } from "./FileView";
import { HINTS } from "../keys";
import type { Environment, Machine, Message, Snapshot, Thread } from "../model";

type Action = Extract<Message, { role: "action" }>;
type Block = Exclude<Message, Action> | { role: "actions"; items: Action[] };

const KIND_GLYPH = { spawn: "+", run: "$", fold: "⇡", note: "…" } as const;

/** Everything the AI did between two things it said folds into one run. */
function group(ms: Message[]): Block[] {
  const out: Block[] = [];
  for (const m of ms) {
    const last = out[out.length - 1];
    if (m.role === "action") {
      if (last?.role === "actions") last.items.push(m);
      else out.push({ role: "actions", items: [m] });
    } else out.push(m);
  }
  return out;
}

/**
 * A machine holds several AI threads. The FIRST may change things — it spawns
 * agents, folds work back, ships. Every other thread is read-only: discussion
 * and planning. The tab bar is where that distinction is made visible.
 */
export function Chat(props: {
  machine: Machine;
  env?: Environment;        // open as a tab of its own, when the dock asks for it
  snapshots: Snapshot[];
  onCloseEnv: () => void;
  threads: Thread[];
  threadId: string;
  onThread: (id: string) => void;
  onNewThread: () => void;
  onCloseThread: (id: string) => void;
  file?: { path: string; status?: string };
  onCloseFile: () => void;
  /** The inspector rides with the thread: what it describes belongs to this
      machine's tree, so it lives inside the tab rather than beside it. */
  inspector?: JSX.Element;
  /** The shell, likewise: it runs in this machine, so it opens under the
      conversation inside the same container rather than across the window. */
  shell?: JSX.Element;
}) {
  const thread = createMemo<Thread | undefined>(
    () => props.threads.find((t) => t.id === props.threadId) ?? props.threads[0],
  );
  const blocks = createMemo(() => group(thread()?.messages ?? []));
  const readonly = () => thread()?.readonly ?? false;
  // The environment takes the pane while it is open: it is a different subject,
  // not a property of the thread underneath it.
  const onEnv = () => !!props.env;
  const onFile = () => !!props.file && !onEnv();
  const away = () => onEnv();
  const running = () => props.machine.workspaces.flatMap((w) => w.ephemerals).filter((e) => e.state === "running").length;

  return (
    <main class="grid h-full min-h-0 min-w-0 grid-rows-[34px_26px_minmax(0,1fr)] overflow-hidden">
      <div class="flex items-stretch border-b border-line bg-chrome pr-2" role="tablist">
        <For each={props.threads}>
          {(t) => (
            // A tab is a row, not a button: a close control cannot live inside a
            // button, and nesting one is invalid markup that browsers treat
            // inconsistently — which is why the close never fired.
            <div
              role="tab"
              aria-selected={!away() && t.id === thread()?.id}
              title={t.readonly ? "Read-only thread: discussion and planning, never a change" : "Main thread: the one that changes things"}
              class="flex max-w-60 min-w-32 items-center gap-1.5 border-r border-line pr-2 pl-3 text-sm whitespace-nowrap text-muted
                     hover:text-fg aria-selected:-mb-px aria-selected:border-b aria-selected:border-b-bg aria-selected:bg-bg aria-selected:text-fg"
            >
              <button
                class="flex min-w-0 flex-1 items-center gap-1.5 self-stretch"
                onClick={() => {
                  props.onCloseEnv();
                  props.onCloseFile();
                  props.onThread(t.id);
                }}
              >
                <Icon name={t.readonly ? "lock" : "sparkle"} class={t.readonly ? "text-subtle" : "text-accent"} />
                <span class="min-w-0 truncate">{t.name}</span>
              </button>
              <Show when={t.readonly} fallback={<Badge tone="accent">main</Badge>}>
                <button
                  class="shrink-0 rounded-sm p-0.5 text-subtle hover:bg-line hover:text-fg"
                  title="Close this thread"
                  onClick={() => props.onCloseThread(t.id)}
                >
                  <Icon name="x" size={11} />
                </button>
              </Show>
            </div>
          )}
        </For>
        <Show when={props.env}>
          {(env) => (
            <div
              role="tab"
              aria-selected={true}
              class="flex max-w-60 min-w-32 items-center gap-1.5 border-r border-line px-3 text-sm whitespace-nowrap text-muted
                     aria-selected:-mb-px aria-selected:border-b aria-selected:border-b-bg aria-selected:bg-bg aria-selected:text-fg"
            >
              <Icon name="server" class="text-accent" />
              <span class="min-w-0 flex-1 truncate font-mono">{env().name}</span>
              <button class="shrink-0 rounded-sm p-0.5 text-subtle hover:bg-line hover:text-fg" title="Close" onClick={props.onCloseEnv}>
                <Icon name="x" size={11} />
              </button>
            </div>
          )}
        </Show>
        <span class="flex-1" />
        <Button class="self-center" variant="icon" icon="plus" title="New read-only thread" onClick={props.onNewThread} />
        <Button class="self-center" variant="icon" icon="split" title="Split" />
      </div>

      <div class="flex items-center gap-1.5 border-b border-line-subtle px-6 text-sm whitespace-nowrap text-muted">
        <Show
          when={!away()}
          fallback={
            <>
              <span>team</span>
              <span class="text-subtle">›</span>
              <span>environment</span>
              <span class="text-subtle">›</span>
              <span>{props.env!.name}</span>
            </>
          }
        >
        <span>machine</span>
        <span class="text-subtle">›</span>
        <span>{props.machine.owner.split("@")[0]}</span>
        <span class="text-subtle">›</span>
        <span>{thread()?.name}</span>
        <span class="flex-1" />
        <Show when={readonly()}>
          <span class="inline-flex items-center gap-1 rounded-full bg-active px-2 text-xs text-muted">
            <Icon name="lock" size={10} /> read-only
          </span>
        </Show>
        </Show>
      </div>

      <Show when={props.env} keyed>
        {(env) => <EnvironmentPage env={env} snapshots={props.snapshots} />}
      </Show>


      {/* The conversation and what it is about are one thing: they sit in a
          centred container together, rather than the transcript floating in the
          middle of the pane with the inspector pinned to the window's edge. */}
      <div class="min-h-0 min-w-0 overflow-hidden" classList={{ hidden: away() }}>
      <div class="mx-auto grid h-full min-h-0 w-full max-w-[1240px] grid-rows-[minmax(0,1fr)_auto]">
      <div
        class="grid min-h-0"
        style={{ "grid-template-columns": props.inspector ? "minmax(0,1fr) 300px" : "minmax(0,1fr)" }}
      >
      <div class="grid min-h-0 min-w-0 grid-rows-[minmax(0,1fr)_auto_auto] font-mono text-base [font-feature-settings:'calt']">
        {/* A file opens in place: the thread stays selected and the sidebar
            stays put, because reading a file is part of following the work
            rather than a separate place to be. */}
        <Show when={props.file} keyed>
          {(f) => <FileView path={f.path} status={f.status} onClose={props.onCloseFile} />}
        </Show>
        <div class="flex flex-col justify-end overflow-y-auto px-6 pt-5 pb-4 select-text" classList={{ hidden: onFile() }}>
          <div class="flex flex-col gap-4">
            <For each={blocks()}>
              {(b) => (
                <Show when={b.role !== "actions"} fallback={<Steps items={(b as { items: Action[] }).items} />}>
                  <div class="flex items-start leading-6">
                    <span class={`w-5 shrink-0 ${b.role === "user" ? "text-accent" : "text-muted"}`}>
                      {b.role === "user" ? "❯" : "⏺"}
                    </span>
                    <span class="min-w-0 flex-1 wrap-words whitespace-pre-wrap">{(b as { text: string }).text}</span>
                    <Time at={(b as { at: string }).at} />
                  </div>
                </Show>
              )}
            </For>
          </div>
        </div>

        {/* The composer is a surface, not a line: a lifted box the caret lives in,
            with the keys that drive it underneath rather than crowding it. */}
        {/* One box: the caret and the keys that drive it belong together, and a
            hint row floating underneath read as a second, unrelated thing. */}
        <div class="bg-bg px-6 pt-1 pb-3.5" classList={{ hidden: onFile() }}>
          <div class="flex flex-col rounded-md border border-line bg-panel transition-colors duration-100 focus-within:border-focus">
            <div class="flex items-start px-3 pt-2 pb-1.5">
              <span class="w-5 shrink-0 leading-5 text-accent">❯</span>
              <textarea
                id="composer"
                rows="1"
                class="max-h-44 min-h-5 flex-1 resize-none border-0 bg-transparent p-0 leading-5 outline-none placeholder:text-subtle"
                placeholder={readonly() ? "ask or discuss · this thread cannot change anything" : "tell the machine what to do"}
              />
            </div>
            <div class="flex items-center gap-4 border-t border-line-subtle px-3 py-1.5 text-xs text-subtle">
              <For each={HINTS}>{(b) => <Hint keys={b.keys}>{b.label}</Hint>}</For>
              <Show when={readonly()}><Hint keys="/promote">to main</Hint></Show>
              <span class="flex-1" />
              <span class="truncate">{props.machine.model}</span>
            </div>
          </div>
        </div>
        {props.shell}
      </div>
      {props.inspector}
      </div>
      </div>
      </div>
    </main>
  );
}

/** Times sit in a column of their own, right aligned, so they read as a margin. */
function Time(props: { at: string; wide?: boolean }) {
  return (
    <span class={`shrink-0 pl-6 text-right text-xs whitespace-nowrap tabular-nums text-subtle ${props.wide ? "w-28" : "w-11"}`}>
      {props.at}
    </span>
  );
}

function Hint(props: { keys: string; children: string }) {
  return (
    <span class="inline-flex items-baseline gap-1.5 whitespace-nowrap">
      <Kbd>{props.keys}</Kbd>
      {props.children}
    </span>
  );
}

/** A run of steps: a headline, then each step hanging off a box-drawing rail. */
function Steps(props: { items: Action[] }) {
  const first = () => props.items[0];
  const last = () => props.items[props.items.length - 1];
  const bad = () => props.items.some((a) => a.ok === false);
  const targets = () => Array.from(new Set(props.items.map((a) => a.target?.split(" ")[0]).filter(Boolean)));

  return (
    <div class="flex flex-col">
      <div class="flex items-start leading-6">
        <span class={`w-5 shrink-0 ${bad() ? "text-danger" : "text-success"}`}>{bad() ? "✗" : "✓"}</span>
        <span class="min-w-0 flex-1 text-muted">
          {props.items.length} {props.items.length === 1 ? "step" : "steps"}
          <span class="text-subtle"> · {targets().join(" ")}</span>
        </span>
        <Time at={first().at === last().at ? first().at : `${first().at}–${last().at}`} wide />
      </div>
      <For each={props.items}>
        {(a, i) => (
          <div class="group flex items-start leading-5 text-muted">
            <span class="w-5 shrink-0 text-line">{i() === props.items.length - 1 ? "└" : "├"}</span>
            <span class={`w-4 shrink-0 ${a.ok === true ? "text-success" : a.ok === false ? "text-danger" : "text-subtle"}`}>
              {KIND_GLYPH[a.kind]}
            </span>
            <span class="shrink-0 pr-2 whitespace-nowrap text-accent">{a.target}</span>
            <span class="min-w-0 flex-1 whitespace-pre-wrap group-hover:text-fg">{a.text}</span>
            <Time at={a.at} />
          </div>
        )}
      </For>
    </div>
  );
}
