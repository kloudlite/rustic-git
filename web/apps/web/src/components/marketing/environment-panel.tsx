"use client";

import { useEffect, useRef, useState } from "react";
import { cn } from "@/lib/utils";

/** The hero visual: a live "working environment" panel. One sequential story on a
 *  150ms tick (T = 372, ~56s loop):
 *   1) a fix goes to session/backend → a review agent forks, approves, merges
 *   2) a ui change goes to session/frontend (busy → queued) → ships
 *   3) an e2e test session intercepts svc/backend + svc/frontend, passes, closes
 *
 *  Styling is inline against theme tokens so it follows light/dark. Decorative
 *  CSS animation stops under prefers-reduced-motion (the .ep-m rule below);
 *  the story itself keeps stepping, since it is the content. */

const T = 372;
const CMD1 = "fix: payment webhooks failing";
const CMD2 = "update: show payment status in ui";
const CMD3 = "test payments end-to-end";

/* ── palette, derived from theme tokens ─────────────────────────── */
const INK = "var(--card-foreground)";
const MUTED = "var(--muted-foreground)";
const FAINT = "color-mix(in oklab, var(--muted-foreground) 72%, transparent)";
const PRIMARY = "var(--primary)";
const GREEN = "var(--success)";
const BORDER = "var(--border)";
const TINT = "color-mix(in oklab, var(--primary) 7%, var(--card))";
const TINT_EDGE = "color-mix(in oklab, var(--primary) 28%, var(--card))";
const GREEN_TINT = "color-mix(in oklab, var(--success) 9%, var(--card))";
const GREEN_EDGE = "color-mix(in oklab, var(--success) 30%, var(--card))";
const HEAD_BG = "color-mix(in oklab, var(--foreground) 2%, var(--card))";
const SUB_BG = "color-mix(in oklab, var(--foreground) 1%, var(--card))";
const TOP_BG = "color-mix(in oklab, var(--primary) 5%, var(--card))";
const RING_BLUE = "color-mix(in oklab, var(--primary) 22%, var(--card))";
const RING_GREEN = "color-mix(in oklab, var(--success) 24%, var(--card))";
const STUB = "color-mix(in oklab, var(--muted-foreground) 45%, transparent)";
const BASE = BORDER; // idle flow line
const ACT_V = `linear-gradient(180deg, transparent, color-mix(in oklab, var(--card) 90%, transparent), transparent) 0 0 / 100% 22px repeat-y var(--primary)`;
const ACT_H = `linear-gradient(90deg, transparent, color-mix(in oklab, var(--card) 90%, transparent), transparent) 0 0 / 22px 100% repeat-x var(--primary)`;
const MONO = "var(--font-mono-brand), ui-monospace, SFMono-Regular, monospace";

function ring(mode: string) {
  if (mode === "blue") return { ringBase: RING_BLUE, ringTop: PRIMARY, ringDash: "26 11.7", spin: "running" };
  if (mode === "green") return { ringBase: RING_GREEN, ringTop: GREEN, ringDash: "26 11.7", spin: "running" };
  if (mode === "done") return { ringBase: GREEN, ringTop: GREEN, ringDash: "37.7 0", spin: "paused" };
  return { ringBase: BORDER, ringTop: "transparent", ringDash: "0 37.7", spin: "paused" };
}
const typed = (s: string, k: number, n: number) => s.slice(0, Math.max(0, Math.round((s.length * (k + 1)) / n)));

function compute(t: number) {
  const seg = (a: number, b: number) => t >= a && t <= b;
  // The prompt only types a command, then clears — the connector flow to the
  // session conveys "sent", so no separate status line is shown.
  let topSub = "";
  if (seg(10, 33)) topSub = typed(CMD1, t - 10, 22);
  else if (seg(176, 199)) topSub = typed(CMD2, t - 176, 22);
  else if (seg(276, 295)) topSub = typed(CMD3, t - 276, 18);
  // Coarse phase id: changes only when the prompt swaps commands, so the fade
  // re-triggers on a real change, not on every typewriter character.
  const topPhase = seg(10, 33) ? 1 : seg(176, 199) ? 2 : seg(276, 295) ? 3 : 0;

  let b = { sub: "waiting for a task…", st: "idle", mode: "idle", bw: "0%", bo: 0 };
  if (seg(42, 49)) b = { sub: "picked up: webhook fix", st: "starting", mode: "blue", bw: "3%", bo: 1 };
  else if (seg(50, 95)) { const p = Math.round(((t - 49) / 46) * 100); b = { sub: "fixing payment webhook retries", st: p + "%", mode: "green", bw: p + "%", bo: 1 }; }
  else if (seg(96, 103)) b = { sub: "fixing payment webhook retries", st: "done ✓", mode: "done", bw: "100%", bo: 0 };
  else if (seg(104, 159)) b = { sub: "fix out for review", st: "in review", mode: "blue", bw: "0%", bo: 0 };
  else if (seg(160, 175)) b = { sub: "merging the approved fix", st: "merging", mode: "blue", bw: "0%", bo: 0 };
  else if (seg(176, 191)) b = { sub: "merged into session / you", st: "shipped ✓", mode: "done", bw: "0%", bo: 0 };
  else if (seg(308, 355)) b = { sub: "routing svc / backend into this workspace", st: "intercepting", mode: "blue", bw: "0%", bo: 0 };

  // The review agent is spawned on demand: it only exists while a review runs,
  // so its row is absent (collapsed) the rest of the time.
  let a = { sub: "", st: "", mode: "idle", bw: "0%", bo: 0, op: 0 };
  if (seg(108, 117)) a = { sub: "forking code · packages · workspace · env", st: "forking", mode: "blue", bw: "6%", bo: 1, op: 1 };
  else if (seg(118, 159)) { const p = Math.round(((t - 117) / 42) * 100); a = { sub: "reviewing the webhook fix", st: p + "%", mode: "green", bw: p + "%", bo: 1, op: 1 }; }
  else if (seg(160, 167)) a = { sub: "approved · merged into session / backend", st: "merged ✓", mode: "done", bw: "100%", bo: 0, op: 1 };
  else if (seg(168, 173)) a = { sub: "session closed", st: "closed", mode: "idle", bw: "0%", bo: 0, op: 0.6 };
  const s0Open = seg(106, 173);

  const queued = seg(208, 227);
  let f;
  if (t < 228) { const p = Math.round(Math.min(70, (t / 200) * 70)); f = { sub: "polishing checkout styles", st: queued ? "1 queued" : p + "%", mode: "green", bw: p + "%", bo: 1 }; }
  else if (seg(228, 235)) f = { sub: "picked up: payment status ui", st: "starting", mode: "blue", bw: "3%", bo: 1 };
  else if (seg(236, 267)) { const p = Math.round(((t - 235) / 32) * 100); f = { sub: "adding payment status to orders page", st: p + "%", mode: "green", bw: p + "%", bo: 1 }; }
  else if (seg(268, 279)) f = { sub: "payment status live on orders page", st: "done ✓", mode: "done", bw: "100%", bo: 0 };
  else if (seg(308, 357)) f = { sub: "routing svc / frontend into this workspace", st: "intercepting", mode: "blue", bw: "0%", bo: 0 };
  else { const p = 70 + Math.round(((t - 280) / 92) * 25); f = { sub: "polishing checkout styles", st: p + "%", mode: "green", bw: p + "%", bo: 1 }; }

  const tapOn = seg(308, 357);
  let e = { sub: "holding the e2e test code", st: "idle", mode: "idle", bw: "0%", bo: 0 };
  if (seg(306, 313)) e = { sub: "picked up: e2e payment tests", st: "starting", mode: "blue", bw: "3%", bo: 1 };
  else if (seg(314, 355)) { const p = Math.round(((t - 313) / 42) * 100); e = { sub: "e2e tests via the intercepted services", st: p + "%", mode: "green", bw: p + "%", bo: 1 }; }
  else if (seg(356, 363)) e = { sub: "all payments flows passed", st: "passed ✓", mode: "done", bw: "100%", bo: 0 };

  const toP0 = seg(34, 43) || seg(296, 305);
  const toS0 = seg(104, 113);
  const toP1 = seg(200, 209);
  const toE0 = seg(296, 305);
  const passDown = toP1 || toE0;
  const topSeg = toP0 || passDown ? ACT_V : BASE;
  const flashP0 = seg(40, 45) || seg(302, 307);
  const flashS0 = seg(110, 117);
  const flashP1 = seg(206, 211);
  const flashE0 = seg(302, 309);
  const V = (on: boolean) => (on ? ACT_V : BASE);
  const H = (on: boolean) => (on ? ACT_H : BASE);
  const noQ = { qW: "0px", qM: "0px", qOp: 0 };

  const rows = [
    { key: "p0", name: "session / backend", sub: b.sub, statusText: b.st, statusColor: b.st.includes("✓") ? GREEN : MUTED, ...ring(b.mode),
      g1u: V(toP0 || passDown), g1l: "50%", g1lBg: V(passDown), g1s: H(toP0), g1sw: "17px", g2w: "0px", g2v: "transparent", g2h: "transparent", g2hw: "0px",
      // The review agent forks from this row: a connector drops down to it. Keep
      // the line a real color and fade it via opacity (a gradient can't transition
      // to "transparent"), so it disappears in step with the collapsing agent row
      // instead of snapping off a beat early — the flicker on disappear.
      cc: toS0 ? ACT_V : BASE, ccOp: s0Open ? 1 : 0,
      barW: b.bw, barOp: b.bo, op: 1, mh: "90px", bg: flashP0 ? TINT : "var(--card)", tap: "⇄ svc / backend", tapMw: tapOn ? "160px" : "0px", tapOp: tapOn ? 1 : 0, ...noQ },
    { key: "s0", name: "agent / review", sub: a.sub, statusText: a.st, statusColor: a.st.includes("✓") ? GREEN : MUTED, ...ring(a.mode),
      g1u: V(passDown), g1l: "50%", g1lBg: V(passDown), g1s: "transparent", g1sw: "0px", g2w: "28px", g2v: V(toS0), g2h: H(toS0), g2hw: "24px", cc: "transparent", ccOp: 1,
      barW: a.bw, barOp: a.bo, op: s0Open ? a.op : 0, mh: s0Open ? "90px" : "0px", bg: flashS0 ? TINT : SUB_BG, tap: "", tapMw: "0px", tapOp: 0, ...noQ },
    { key: "p1", name: "session / frontend", sub: f.sub, statusText: f.st, statusColor: f.st.includes("✓") ? GREEN : f.st === "1 queued" ? PRIMARY : MUTED, ...ring(f.mode),
      g1u: V(toP1 || toE0), g1l: toE0 ? "50%" : "50%", g1lBg: V(toE0), g1s: H(toP1), g1sw: "17px", g2w: "0px", g2v: "transparent", g2h: "transparent", g2hw: "0px", cc: "transparent", ccOp: 1,
      barW: f.bw, barOp: f.bo, op: 1, mh: "90px", bg: flashP1 ? TINT : "var(--card)",
      tap: "⇄ svc / frontend", tapMw: tapOn ? "140px" : "0px", tapOp: tapOn ? 1 : 0, qW: queued ? "6px" : "0px", qM: queued ? "6px" : "0px", qOp: queued ? 1 : 0 },
    { key: "e0", name: "session / tests", sub: e.sub, statusText: e.st, statusColor: e.st.includes("✓") ? GREEN : MUTED, ...ring(e.mode),
      g1u: V(toE0), g1l: "0%", g1lBg: "transparent", g1s: H(toE0), g1sw: "17px", g2w: "0px", g2v: "transparent", g2h: "transparent", g2hw: "0px", cc: "transparent", ccOp: 1,
      barW: e.bw, barOp: e.bo, op: 1, mh: "90px", bg: flashE0 ? TINT : "var(--card)", tap: "", tapMw: "0px", tapOp: 0, ...noQ },
  ];

  const services = [{ name: "postgres" }, { name: "redis" }, { name: "svc / backend" }, { name: "svc / frontend" }].map((d, i) => {
    const hit = tapOn && (i === 2 || i === 3);
    return { name: d.name, border: hit ? TINT_EDGE : BORDER, bg: hit ? TINT : HEAD_BG, dot: hit ? PRIMARY : GREEN, blink: hit ? "running" : "paused" };
  });

  return { topSub, topPhase, topSeg, rows, services, s0Open };
}

const CHIP =
  "flex items-center gap-2 border border-edge bg-card px-3 py-[7px] shadow-xs transition " +
  "hover:-translate-y-px hover:border-[color-mix(in_oklab,var(--primary)_55%,var(--border))] " +
  "hover:bg-[color-mix(in_oklab,var(--primary)_6%,var(--card))]";

function SourceChip({ label, icon }: { label: string; icon: React.ReactNode }) {
  return (
    <div className="flex flex-col items-center">
      <span className={CHIP} style={{ cursor: "default" }}>
        {icon}
        <span style={{ fontFamily: MONO, fontSize: 11.5, color: INK }}>{label}</span>
      </span>
      <span style={{ width: 1.5, height: 16, background: STUB }} />
      <span style={{ width: 5, height: 5, borderRadius: "50%", background: STUB, marginBottom: -2.5, position: "relative", zIndex: 2 }} />
    </div>
  );
}

export function EnvironmentPanel({ className }: { className?: string }) {
  const [t, setT] = useState(0);
  useEffect(() => {
    const id = setInterval(() => setT((v) => (v + 1) % T), 150);
    return () => clearInterval(id);
  }, []);

  // Measure a real row's height so the on-demand agent row collapses by exactly
  // that amount and the spacer below reserves exactly that amount — the two cancel,
  // so the panel's outer height never changes as the agent appears/disappears.
  const rowRef = useRef<HTMLDivElement>(null);
  const [rowH, setRowH] = useState(66);
  useEffect(() => {
    const el = rowRef.current;
    if (!el) return;
    const update = () => setRowH(el.offsetHeight);
    update();
    const ro = new ResizeObserver(update);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const { topSub, topPhase, topSeg, rows, services, s0Open } = compute(t);

  return (
    <div className={cn("flex w-full max-w-[560px] flex-col items-center", className)} aria-hidden>
      {/* ep-* keyframes live in globals.css so there is no inline <style> to
          hydrate (a React 19 mismatch there stalls the story tick), and the
          motion is deliberately not gated on prefers-reduced-motion. */}
      <div className="flex w-full items-end justify-evenly">
        <SourceChip
          label="code repos"
          icon={<svg width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" style={{ color: MUTED }}><circle cx="4.5" cy="4" r="1.7" /><circle cx="4.5" cy="12" r="1.7" /><circle cx="11.5" cy="5.5" r="1.7" /><path d="M4.5 5.7 L4.5 10.3 M11.5 7.2 C11.5 9.8 7.5 9.5 5.5 10.6" /></svg>}
        />
        <SourceChip
          label="storage snapshots"
          icon={<svg width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" style={{ color: MUTED }}><ellipse cx="8" cy="3.8" rx="5.3" ry="2" /><path d="M2.7 3.8 L2.7 12.2 C2.7 13.3 5.1 14.2 8 14.2 C10.9 14.2 13.3 13.3 13.3 12.2 L13.3 3.8 M2.7 8 C2.7 9.1 5.1 10 8 10 C10.9 10 13.3 9.1 13.3 8" /></svg>}
        />
      </div>

      <div className="relative w-full overflow-hidden border bg-card shadow-[0_20px_48px_-12px_color-mix(in_oklab,var(--foreground)_12%,transparent)]" style={{ borderColor: BORDER }}>
        {/* header */}
        <div className="flex items-center justify-between px-5 py-3" style={{ borderBottom: `1px solid ${BORDER}`, background: HEAD_BG }}>
          <span style={{ fontFamily: MONO, fontSize: 13, color: INK }}>working environment</span>
        </div>

        {/* top session (you) */}
        <div className="relative flex items-center gap-3.5 px-5 py-[15px]" style={{ background: TOP_BG }}>
          <div className="ep-m absolute" style={{ left: 27, top: "calc(50% + 10px)", bottom: 0, width: 1.5, animation: "ep-flowY .45s linear infinite", background: topSeg }} />
          <svg width="16" height="16" viewBox="0 0 16 16" className="ep-m shrink-0" style={{ animation: "ep-spin 2.4s linear infinite" }}>
            <circle cx="8" cy="8" r="6" fill="none" stroke={BORDER} strokeWidth="2" />
            <circle cx="8" cy="8" r="6" fill="none" stroke={INK} strokeWidth="2" strokeDasharray="26 11.7" strokeLinecap="round" transform="rotate(-90 8 8)" />
          </svg>
          <div className="flex min-w-0 flex-1 flex-col gap-[5px]">
            <span className="whitespace-nowrap" style={{ fontFamily: MONO, fontSize: 13, color: INK, fontWeight: 500 }}>session / you</span>
            <span className="flex max-w-full items-center" style={{ fontFamily: MONO, fontSize: 11.5, color: MUTED }}>
              <span className="shrink-0" style={{ color: TINT_EDGE, marginRight: 7 }}>❯</span>
              <span key={topPhase} className="min-w-0 overflow-hidden text-ellipsis whitespace-nowrap" style={{ animation: "ep-fade .35s ease" }}>{topSub}</span>
              <span className="ep-m shrink-0" style={{ width: 1.5, height: 12, marginLeft: 2, background: PRIMARY, animation: "ep-blink 1.1s steps(1) infinite" }} />
            </span>
          </div>
          <span className="flex items-center gap-[7px] px-2.5 py-[3px]" style={{ fontFamily: MONO, fontSize: 11, color: GREEN, border: `1px solid ${GREEN_EDGE}`, background: GREEN_TINT }}>
            <span className="ep-m" style={{ width: 6, height: 6, borderRadius: "50%", background: GREEN, animation: "ep-blink 2.4s infinite" }} />
            live
          </span>
        </div>

        {/* session rows */}
        {rows.map((s) => (
          <div
            key={s.key}
            className="overflow-hidden"
            style={{
              transition: "height .55s cubic-bezier(0.4, 0, 0.2, 1), opacity .3s ease",
              // The agent row uses an explicit height (not max-height) so its box
              // follows the transition exactly — independent of its content height,
              // which shrinks when the sub-text empties on collapse — and therefore
              // cancels the spacer below perfectly, with no outer-height wobble.
              ...(s.key === "s0" ? { height: s0Open ? rowH : 0 } : { maxHeight: s.mh }),
              opacity: s.op,
            }}
          >
            <div ref={s.key === "p0" ? rowRef : undefined} className="relative flex items-center gap-3.5 px-5 py-[13px]" style={{ borderTop: `1px solid var(--rule)`, transition: "background .6s", background: s.bg }}>
              <div className="ep-m absolute" style={{ left: 51, top: "calc(50% + 10px)", bottom: 0, width: 1.5, animation: "ep-flowY .45s linear infinite", background: s.cc, opacity: s.ccOp, transition: "opacity .4s ease" }} />
              <div className="flex shrink-0 self-stretch" style={{ margin: "-13px 0 -13px -6px" }}>
                <div className="relative" style={{ width: 16 }}>
                  <div className="ep-m absolute" style={{ left: 13, top: 0, height: "50%", width: 1.5, animation: "ep-flowY .45s linear infinite", background: s.g1u }} />
                  <div className="ep-m absolute" style={{ left: 13, top: "50%", width: 1.5, animation: "ep-flowY .45s linear infinite", background: s.g1lBg, height: s.g1l }} />
                  <div className="ep-m absolute" style={{ left: 13, top: "50%", height: 1.5, animation: "ep-flowX .45s linear infinite", background: s.g1s, width: s.g1sw }} />
                </div>
                <div className="relative" style={{ width: s.g2w }}>
                  <div className="ep-m absolute" style={{ left: 21, top: 0, height: "50%", width: 1.5, animation: "ep-flowY .45s linear infinite", background: s.g2v }} />
                  <div className="ep-m absolute" style={{ left: 21, top: "50%", height: 1.5, animation: "ep-flowX .45s linear infinite", background: s.g2h, width: s.g2hw }} />
                </div>
              </div>
              <svg width="16" height="16" viewBox="0 0 16 16" className="ep-m shrink-0" style={{ animation: "ep-spin 1.3s linear infinite", animationPlayState: s.spin as "running" | "paused" }}>
                <circle cx="8" cy="8" r="6" fill="none" strokeWidth="2" stroke={s.ringBase} style={{ transition: "stroke .4s" }} />
                <circle cx="8" cy="8" r="6" fill="none" strokeWidth="2" strokeLinecap="round" transform="rotate(-90 8 8)" stroke={s.ringTop} strokeDasharray={s.ringDash} style={{ transition: "stroke .4s" }} />
              </svg>
              <div className="flex min-w-0 flex-1 flex-col gap-[5px]">
                <span className="flex items-center gap-2.5">
                  <span className="whitespace-nowrap" style={{ fontFamily: MONO, fontSize: 13, color: INK }}>{s.name}</span>
                  <span className="inline-block overflow-hidden whitespace-nowrap px-[7px] py-px" style={{ transition: "max-width .6s, opacity .5s", maxWidth: s.tapMw, opacity: s.tapOp, fontFamily: MONO, fontSize: 11, color: PRIMARY, background: TINT, boxShadow: `inset 0 0 0 1px ${TINT_EDGE}` }}>{s.tap}</span>
                </span>
                <span className="flex items-center" style={{ fontSize: 12, color: FAINT }}>
                  <span className="ep-m inline-block shrink-0" style={{ borderRadius: "50%", background: PRIMARY, height: 6, transition: "width .4s, margin .4s, opacity .4s", width: s.qW, marginRight: s.qM, opacity: s.qOp, animation: "ep-blink 1.2s infinite" }} />
                  {s.sub}
                </span>
              </div>
              <div className="hidden shrink-0 flex-col items-end gap-1.5 sm:flex sm:w-32">
                <span style={{ fontFamily: MONO, fontSize: 11.5, transition: "color .5s", color: s.statusColor }}>{s.statusText}</span>
                <div className="hidden sm:block" style={{ width: 110, height: 5, transition: "opacity .5s", opacity: s.barOp, background: `linear-gradient(90deg, ${BORDER} 0 6px, transparent 6px 11px) left center / 11px 5px repeat-x` }}>
                  <div style={{ height: 5, background: `linear-gradient(90deg, ${PRIMARY} 0 6px, transparent 6px 11px) left center / 11px 5px repeat-x`, transition: "width .5s linear, opacity .5s", width: s.barW, opacity: s.barOp }} />
                </div>
              </div>
            </div>
          </div>
        ))}

        {/* backing services */}
        <div className="px-5 py-3" style={{ borderTop: `1px solid ${BORDER}` }}>
          <div className="mb-[9px]">
            <span className="font-semibold" style={{ fontSize: 11, letterSpacing: "1.2px", color: FAINT }}>BACKING SERVICES</span>
          </div>
          <div className="flex flex-wrap gap-2">
            {services.map((v) => (
              <span key={v.name} className="flex items-center gap-[7px] px-2.5 py-[5px]" style={{ fontFamily: MONO, fontSize: 11.5, color: INK, transition: "border-color .5s, background .5s", border: `1px solid ${v.border}`, background: v.bg }}>
                <span className="ep-m" style={{ width: 6, height: 6, borderRadius: "50%", transition: "background .5s", background: v.dot, animation: "ep-blink 1.2s infinite", animationPlayState: v.blink as "running" | "paused" }} />
                {v.name}
              </span>
            ))}
          </div>
        </div>
      </div>
      {/* Holds the agent row's 90px below the card while it is collapsed, so the
          surrounding hero never reflows. Same easing as the row so they move in
          lockstep and the outer height stays constant through the transition. */}
      <div aria-hidden style={{ height: s0Open ? 0 : rowH, transition: "height .55s cubic-bezier(0.4, 0, 0.2, 1)" }} />
    </div>
  );
}
