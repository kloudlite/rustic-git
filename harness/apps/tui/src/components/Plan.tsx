import { useEffect, useState } from "react";
import { theme } from "../theme.ts";
import { clip } from "../ui/text.ts";
import type { PlanRow, Tone } from "../tasks.ts";
import { Heading } from "./Sidebar.tsx";

export const toneColor = (t?: Tone) => (t ? theme[t] : theme.fg);

/** `#rrggbb` a→b by t in [0,1]; anything not a 6-digit hex just returns `a`. */
const mix = (a: string, b: string, t: number) => {
  const p = (h: string) => (/^#[0-9a-f]{6}$/i.test(h) ? [1, 3, 5].map((i) => parseInt(h.slice(i, i + 2), 16)) : null);
  const x = p(a), y = p(b);
  if (!x || !y) return a;
  return `#${x.map((v, i) => Math.round(v + (y[i]! - v) * t).toString(16).padStart(2, "0")).join("")}`;
};

const BAND = 6;
/** Working text: a bright band sweeps across the accent colour, one step every 70 ms. */
function Shimmer({ text, from, to }: { text: string; from: string; to: string }) {
  const [step, setStep] = useState(0);
  useEffect(() => {
    const id = setInterval(() => setStep((n) => n + 1), 70);
    return () => clearInterval(id);
  }, []);
  const at = (step % (text.length + BAND * 3)) - BAND;
  return (
    <text>
      {[...text].map((c, i) => (
        <span key={i} fg={mix(from, to, Math.max(0, 1 - Math.abs(i - at) / BAND))}>
          {c}
        </span>
      ))}
    </text>
  );
}

/** One plan line: guides and text left, state right. A session header is its name, a rule, its state. */
export function PlanLine({ row, width, on }: { row: PlanRow; width: number; on?: boolean }) {
  const right = row.right ?? "";
  if (row.head) {
    // a group label under the one "Tasks" heading: no rule of its own, so it never reads as a second title
    const name = clip(row.text, Math.max(1, width - right.length - 4));
    return (
      <box flexDirection="row" justifyContent="space-between" height={1} overflow="hidden" paddingLeft={1} paddingRight={1}>
        <text fg={theme.muted}>{name}</text>
        {right ? <text fg={right === "needs you" ? theme.warning : theme.muted}>{right}</text> : null}
      </box>
    );
  }
  const left = clip(row.text, Math.max(1, width - right.length - 4));
  if (on)
    // the selected row of the ^g list: plain text on the selection band, the way Files draws its cursor
    return (
      <box flexDirection="row" justifyContent="space-between" height={1} overflow="hidden" paddingLeft={1} paddingRight={1} backgroundColor={theme.selection}>
        <text selectable={false} fg={theme.bg}>{left}</text>
        <text selectable={false} fg={theme.bg}>{right}</text>
      </box>
    );
  return (
    <box flexDirection="row" justifyContent="space-between" height={1} overflow="hidden" paddingLeft={1} paddingRight={1}>
      {row.live ? <Shimmer text={left} from={theme.accent} to={theme.fg} /> : <text fg={toneColor(row.tone)}>{left}</text>}
      {row.live ? <Shimmer text={right} from={theme.accent} to={theme.fg} /> : <text fg={row.tone === "error" ? toneColor(row.tone) : theme.muted}>{right}</text>}
    </box>
  );
}

/** The open tasks of the view (rows are already folded by planRows), above the queue; nothing when empty.
 *  One heading; group labels only when more than one session has open work. */
export function Plan({ rows, width }: { rows: PlanRow[]; width: number }) {
  if (rows.length === 0) return null;
  const shown = rows.filter((r) => r.head).length > 1 ? rows : rows.filter((r) => !r.head);
  const open = rows.filter((r) => !r.head && /^[│├└ ─]*[●○!] /.test(r.text)).length;
  return (
    <box flexDirection="column" flexShrink={0} marginBottom={1}>
      <Heading width={width} count={open} flush>Tasks</Heading>
      {shown.map((r, i) => (
        <PlanLine key={i} row={r} width={width} />
      ))}
    </box>
  );
}
