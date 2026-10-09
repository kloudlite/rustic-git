import { useEffect, useMemo, useRef, useState } from "react";
import { useKeyboard } from "@opentui/react";
import { TextAttributes, type ScrollBoxRenderable } from "@opentui/core";
import { theme } from "../theme.ts";
import { SplitBorder } from "../ui/border.ts";
import { SPECIAL } from "./Input.tsx";
import { uptime } from "../tasks.ts";
import type { Process } from "../workspaces.ts";
import { useWheelAccel } from "../wheel.ts";

type Row = { kind: "header"; label: string; extra: string } | { kind: "proc"; proc: Process };

const stopped = (p: Process) => p.status === "exited" || p.status === "crashed";

export { uptime };

export type Group = { label: string; processes: Process[] };

/** RUNNING then STOPPED, filtered by name or command; a section with no rows is not drawn. With
 * `groups` (main) each workspace is a section instead: running rows, then stopped, "{n} running". */
function buildRows(processes: Process[], filter: string, groups?: Group[]): Row[] {
  const q = filter.toLowerCase();
  const match = (list: Process[]) => list.filter((p) => !q || p.name.toLowerCase().includes(q) || p.command.toLowerCase().includes(q));
  const out: Row[] = [];
  const sections: [string, Process[], boolean][] = groups
    ? groups.map((g) => [g.label, match(g.processes), true])
    : [["RUNNING", match(processes).filter((p) => !stopped(p)), false], ["STOPPED", match(processes).filter(stopped), false]];
  for (const [label, list, byWorkspace] of sections) {
    if (!list.length) continue;
    const live = list.filter((p) => !stopped(p));
    out.push({ kind: "header", label, extra: byWorkspace ? `${live.length} running` : String(list.length) });
    for (const proc of [...live, ...list.filter(stopped)]) out.push({ kind: "proc", proc });
  }
  return out;
}

const glyph = (p: Process) => (stopped(p) ? (p.status === "crashed" ? "✕" : "○") : "●");
const glyphColor = (p: Process) => (p.status === "crashed" ? theme.error : stopped(p) ? theme.muted : theme.success);
const exit = (p: Process) => (p.status === "crashed" ? `exit ${p.code ?? 1}` : "done");

/**
 * Jobs screen (^j), laid out like the files view: what the workspace runs on the left (RUNNING,
 * STOPPED), the selected process's command and log on the right. j/k move, l or enter focuses the
 * log, tab swaps panes, `/` filters, esc backs out a layer. The log follows its tail until you
 * scroll up; G or F resumes. Exited processes keep their log (the pod lists them for 10 minutes).
 */
export function Processes({
  processes,
  groups,
  onClose,
}: {
  workspace: string;
  processes: Process[];
  /** Main only: every workspace's jobs, one section each; `processes` is then their flat list. */
  groups?: Group[];
  onClose: () => void;
}) {
  const wheel = useWheelAccel();
  const [selId, setSelId] = useState<string | null>(null);
  const [pane, setPane] = useState<"list" | "log">("list");
  const [filter, setFilter] = useState("");
  const [typing, setTyping] = useState(false);
  const [follow, setFollow] = useState(true);
  const [flash, setFlash] = useState(false);
  const logRef = useRef<ScrollBoxRenderable>(null);

  const rows = useMemo(() => buildRows(processes, filter, groups), [processes, filter, groups]);
  const picks = rows.flatMap((r) => (r.kind === "proc" ? [r.proc] : []));
  const proc = picks.find((p) => p.id === selId) ?? picks[0];
  const lines = proc?.logs ?? [];

  const toEnd = () => logRef.current?.scrollTo(Number.MAX_SAFE_INTEGER);
  const select = (p: Process) => {
    setSelId(p.id);
    setFollow(true);
  };

  // follow keeps the tail in view as lines arrive; a new selection starts at the tail
  useEffect(() => {
    if (follow) toEnd();
  }, [proc?.id, lines.length, follow]);

  // the files view's flash: the selected process gained lines
  const seen = useRef<{ id?: string; n: number }>({ n: 0 });
  useEffect(() => {
    const prev = seen.current;
    seen.current = { id: proc?.id, n: lines.length };
    if (prev.id !== proc?.id || lines.length <= prev.n) return;
    setFlash(true);
    const t = setTimeout(() => setFlash(false), 1200);
    return () => clearTimeout(t);
  }, [proc?.id, lines.length]);

  const move = (d: number) => {
    const i = picks.findIndex((p) => p.id === proc?.id);
    const next = picks[Math.max(0, Math.min(picks.length - 1, i + d))];
    if (next) select(next);
  };

  useKeyboard((key) => {
    // ^j toggles this screen (app.tsx command()); the app's own handler is parked while it is up
    if (key.ctrl && key.name === "j" && !key.meta) return onClose();
    if (key.ctrl || key.meta || key.option) return;
    if (typing) {
      if (key.name === "escape") {
        setFilter("");
        return setTyping(false);
      }
      if (key.name === "return") return setTyping(false);
      if (key.name === "backspace" || key.name === "delete") return setFilter((f) => f.slice(0, -1));
      if (SPECIAL.has(key.name)) return;
      const t = key.sequence;
      if (t && !t.startsWith("\x1b") && t >= " ") setFilter((f) => f + t);
      return;
    }
    // shift arrives as an upper-case name or as name + shift, depending on the terminal
    const k = key.shift ? key.name.toUpperCase() : key.name;
    if (k === "escape") return filter ? setFilter("") : onClose();
    if (key.sequence === "/") return setTyping(true);
    if (k === "tab") return setPane((p) => (p === "list" ? "log" : "list"));
    if (pane === "list") {
      if (k === "j" || k === "down") return move(1);
      if (k === "k" || k === "up") return move(-1);
      if (k === "l" || k === "return") return setPane("log");
      if (k === "G") return (setFollow(true), toEnd());
      if (k === "F") return setFollow((f) => (f ? false : (toEnd(), true)));
      return;
    }
    const sb = logRef.current;
    const page = Math.max(1, (sb?.viewport.height ?? 20) - 2);
    // any upward scroll lets go of the tail
    const up = (n: number) => (setFollow(false), sb?.scrollBy(-n));
    if (k === "j" || k === "down") sb?.scrollBy(1);
    if (k === "k" || k === "up") up(1);
    if (k === "d") sb?.scrollBy(Math.ceil(page / 2));
    if (k === "u") up(Math.ceil(page / 2));
    if (k === "g") (setFollow(false), sb?.scrollTo(0));
    if (k === "G") (setFollow(true), toEnd());
    if (k === "F") setFollow((f) => (f ? false : (toEnd(), true)));
    if (k === "h") setPane("list");
  });

  const running = processes.filter((p) => !stopped(p)).length;
  const crashed = processes.filter((p) => p.status === "crashed").length;
  const now = Date.now();
  const metaOf = (p: Process) => (stopped(p) ? exit(p) : uptime(p.startedAt, now) || "running");

  return (
    <box flexDirection="column" flexGrow={1} minHeight={0}>
      <box flexDirection="row" justifyContent="space-between" paddingLeft={1} paddingRight={1}>
        <text>
          <span fg={theme.muted}>~/workspace</span>
          {flash ? <span fg={theme.success}>  ● updated</span> : ""}
        </text>
        <text fg={theme.muted}>
          {typing ? (
            <span>
              <span fg={theme.accent}>/</span>
              <span fg={theme.fg}>{filter}</span>
              <span attributes={TextAttributes.INVERSE}> </span>
            </span>
          ) : filter ? (
            <span>
              <span fg={theme.accent}>/</span>
              <span fg={theme.fg}>{filter}</span>
              <span fg={theme.muted}> · {picks.length}/{processes.length}</span>
            </span>
          ) : processes.length === 0 ? (
            "no processes"
          ) : (
            `${running} running${crashed ? ` · ${crashed} crashed` : ""}`
          )}
        </text>
      </box>

      <box flexDirection="row" flexGrow={1} minHeight={0} marginTop={1}>
        {/* left: what is running, then what stopped */}
        <box flexDirection="column" width="42%" minWidth={24} flexShrink={0} paddingLeft={1} onMouseDown={() => setPane("list")}>
          <scrollbox flexGrow={1} flexBasis={0} scrollbarOptions={{ visible: false }} scrollAcceleration={wheel}>
            {rows.map((row, i) => {
              if (row.kind === "header")
                return (
                  <box key={`h${row.label}`} marginTop={i === 0 ? 0 : 1} flexDirection="row" justifyContent="space-between" paddingRight={1}>
                    <text fg={theme.muted}><b>{row.label}</b></text>
                    <text fg={theme.muted}>{row.extra}</text>
                  </box>
                );
              const p = row.proc;
              const active = p.id === proc?.id && pane === "list";
              return (
                <box
                  key={p.id}
                  flexDirection="row"
                  height={1}
                  overflow="hidden"
                  paddingLeft={1}
                  paddingRight={1}
                  backgroundColor={active ? theme.selection : undefined}
                  onMouseDown={() => (select(p), setPane("list"))}
                >
                  <text selectable={false} fg={active ? theme.bg : glyphColor(p)}>{glyph(p)} </text>
                  <text selectable={false} fg={active ? theme.bg : theme.fg}>{p.name}</text>
                  <box flexGrow={1} />
                  <text selectable={false} fg={active ? theme.bg : theme.muted}>{metaOf(p)}</text>
                </box>
              );
            })}
          </scrollbox>
        </box>

        {/* right: the reader — the selected process's command and log */}
        <box
          flexDirection="column"
          flexGrow={1}
          minHeight={0}
          {...SplitBorder}
          border={["left"]}
          borderColor={pane === "log" ? theme.accent : theme.border}
          onMouseDown={() => setPane("log")}
        >
          {!proc ? (
            <box paddingLeft={2} paddingTop={1}>
              <text fg={theme.muted}>nothing has run in this workspace — background commands the agent starts (exec with detach) show here</text>
            </box>
          ) : (
            <>
              <box flexDirection="row" justifyContent="space-between" paddingLeft={2} paddingRight={2}>
                <box flexGrow={1} height={1} overflow="hidden">
                  <text fg={theme.fg}>{proc.command}</text>
                </box>
                <text fg={theme.muted}>
                  {proc.status} · {metaOf(proc) || "—"} · {lines.length} lines
                  {follow ? <span fg={theme.accent}>  following</span> : ""}
                </text>
              </box>
              <scrollbox ref={logRef} flexGrow={1} flexBasis={0} marginTop={1} paddingLeft={1} scrollbarOptions={{ visible: false }} scrollAcceleration={wheel}>
                {lines.length === 0 ? (
                  <box paddingLeft={1}>
                    <text fg={theme.muted}>{stopped(proc) ? "no output" : "no output yet"}</text>
                  </box>
                ) : (
                  lines.map((l, i) => (
                    <box key={i} flexDirection="row" height={1} overflow="hidden" flexShrink={0}>
                      <text selectable={false} fg={theme.muted}>{String(i + 1).padStart(4)}  </text>
                      <text fg={l.err ? theme.error : theme.fg}>{l.text}</text>
                    </box>
                  ))
                )}
              </scrollbox>
            </>
          )}
        </box>
      </box>

      {/* footer hints */}
      <box flexDirection="row" gap={2} paddingLeft={1} marginTop={1}>
        <text fg={theme.muted}>{`processes › ${proc?.name ?? "—"}`}</text>
        <box flexGrow={1} />
        <text fg={theme.fg}>j k <span fg={theme.muted}>move</span></text>
        <text fg={theme.fg}>l <span fg={theme.muted}>open</span></text>
        <text fg={theme.fg}>tab <span fg={theme.muted}>pane</span></text>
        <text fg={theme.fg}>G <span fg={theme.muted}>tail</span></text>
        <text fg={theme.fg}>F <span fg={theme.muted}>follow</span></text>
        <text fg={theme.fg}>/ <span fg={theme.muted}>filter</span></text>
        <text fg={theme.fg}>^j <span fg={theme.muted}>chat</span></text>
        <text fg={theme.fg}>esc <span fg={theme.muted}>back</span></text>
      </box>
    </box>
  );
}
