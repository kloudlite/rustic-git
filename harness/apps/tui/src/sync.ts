//! What a TUI does with what the daemon pushes. Pure: app.tsx owns React state and calls these.
//! The daemon holds every session's state; nothing here invents state the daemon did not send.
import type { Ask, SessionState, SpaceView } from "@kloudlite-tui/backend";
import type { Entry } from "./components/Transcript.tsx";
import { fromSpace } from "./workspaces";

const text = (m: any) => (m.content ?? []).filter((b: any) => b.type === "text").map((b: any) => b.text).join("\n");

/** A transcript from an open's snapshot, with the ids the live path uses so later events update rows. */
export function transcript(messages: any[], busy: boolean, toolSummary: (name: string, args: any) => string) {
  const entries: Entry[] = [];
  const history: string[] = [];
  for (const m of messages) {
    if (m.role === "user") {
      const t = text(m);
      if (t) {
        entries.push({ kind: "user", id: `u${m.timestamp}`, text: t } as Entry);
        history.push(t);
      }
    } else if (m.role === "assistant") {
      const mid = `m${m.timestamp}`;
      const thinking = (m.content ?? []).filter((b: any) => b.type === "thinking").map((b: any) => b.thinking).join("");
      if (thinking.trim()) entries.push({ kind: "thinking", id: `${mid}t`, text: thinking, done: true });
      const t = (m.content ?? []).filter((b: any) => b.type === "text").map((b: any) => b.text).join("");
      if (t.trim()) entries.push({ kind: "agent", id: mid, text: t });
      for (const b of m.content ?? [])
        if (b.type === "toolCall")
          entries.push({ kind: "tool", id: b.id, name: b.name, summary: toolSummary(b.name, b.arguments), status: busy ? "running" : "ok" });
    } else if (m.role === "toolResult") {
      const i = entries.findIndex((e) => e.kind === "tool" && e.id === m.toolCallId);
      if (i === -1) continue;
      const t = text(m);
      entries[i] = { ...(entries[i] as Entry & { kind: "tool" }), status: m.isError ? "error" : "ok", output: t || undefined, error: m.isError ? t.split("\n")[0] : undefined, display: m.details?.display };
    }
  }
  return { entries, history };
}

export function userRow(e: any): Entry | null {
  if (e?.type !== "message_start" || e.message?.role !== "user") return null;
  return { kind: "user", id: `u${e.message.timestamp}`, text: e.shown ?? text(e.message) } as Entry;
}

export function upsertById(entries: Entry[], e: Entry): Entry[] {
  const id = (e as any).id;
  const i = entries.findIndex((x: any) => x.id === id);
  if (i === -1) return [...entries, e];
  const next = [...entries];
  next[i] = e;
  return next;
}

export function applyState(s: SessionState) {
  return {
    model: s.model,
    tokens: s.tokens,
    queued: [
      ...s.queued.steering.map((text) => ({ text, kind: "steer" as const })),
      ...s.queued.followUp.map((text) => ({ text, kind: "followUp" as const })),
    ],
  };
}

/** Focus after a space push: follows its workspace by id; an errored push never moves it. */
export function keepFocus(prev: { focus: number; ids: string[] }, view: SpaceView): number {
  if (view.error || prev.focus === 0) return prev.focus;
  const next = fromSpace(view).workspaces;
  const at = next.findIndex((w) => w.id === prev.ids[prev.focus - 1]);
  return at >= 0 ? at + 1 : Math.min(prev.focus, next.length);
}

/** Always-allow is this TUI's own (the person: "Let always allow be in Tui"). */
export function autoAnswer(ask: Ask, granted: Map<string, Set<string>>): string | null {
  return ask.kind === "permission" && granted.get(ask.key)?.has(ask.tool) ? "once" : null;
}

export function grant(granted: Map<string, Set<string>>, ask: Ask): void {
  const s = granted.get(ask.key) ?? new Set<string>();
  s.add(ask.tool);
  granted.set(ask.key, s);
}
