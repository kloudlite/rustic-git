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
export function PlanLine({ row, width }: { row: PlanRow; width: number }) {
  const right = row.right ?? "";
  if (row.head) {
    const name = clip(row.text, Math.max(1, width - 2 - right.length - 4));
    const fill = Math.max(1, width - 2 - name.length - right.length - (right ? 2 : 1));
    return (
      <box flexDirection="row" height={1} overflow="hidden" paddingLeft={1} paddingRight={1}>
        <text fg={theme.fg}>
          <b>{name}</b>
        </text>
        <text fg={theme.muted}>{` ${"─".repeat(fill)}`}</text>
        {right ? <text fg={right === "working" ? theme.accent : right === "needs you" ? theme.warning : theme.muted}>{` ${right}`}</text> : null}
      </box>
    );
  }
  const left = clip(row.text, Math.max(1, width - right.length - 4));
  return (
    <box flexDirection="row" justifyContent="space-between" height={1} overflow="hidden" paddingLeft={1} paddingRight={1}>
      {row.live ? <Shimmer text={left} from={theme.accent} to={theme.fg} /> : <text fg={toneColor(row.tone)}>{left}</text>}
      {row.live ? <Shimmer text={right} from={theme.accent} to={theme.fg} /> : <text fg={row.tone === "error" ? toneColor(row.tone) : theme.muted}>{right}</text>}
    </box>
  );
}

/** The plan of the session on screen (rows are already folded by planRows), above the queue; nothing when empty. */
export function Plan({ rows, width }: { rows: PlanRow[]; width: number }) {
  if (rows.length === 0) return null;
  return (
    <box flexDirection="column" flexShrink={0} marginBottom={1}>
      <Heading width={width} flush>Plan</Heading>
      {rows.map((r, i) => (
        <PlanLine key={i} row={r} width={width} />
      ))}
    </box>
  );
}
