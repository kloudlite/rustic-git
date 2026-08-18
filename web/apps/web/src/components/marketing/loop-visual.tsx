import { cn } from "@/lib/utils";

/** One repo at the centre; sessions going round it — yours and your agents'.
 *  The ring carries four markers, because the loop passes through more than the
 *  code: packages, workspace, environment, back to the repo. Each session moves
 *  at its own speed, with a trail behind it so the direction reads at a glance.
 *
 *  Pure SVG + CSS; everything stops under prefers-reduced-motion (globals.css). */
const C = 160;
const R = 92;

/** Faster sessions get a longer trail — the same cue a long exposure gives.
 *  `offset` is a static rotation, not an animation delay: under
 *  prefers-reduced-motion the animation stops dead, and anything relying on a
 *  negative delay for its phase would collapse into a single stacked dot. */
const SESSIONS = [
  { offset: 0, duration: "3.4s", trail: 26, dot: 6, opacity: 1 },
  { offset: 130, duration: "2.6s", trail: 34, dot: 5, opacity: 0.8 },
  { offset: 235, duration: "4.2s", trail: 20, dot: 5, opacity: 0.6 },
];

/** The loop's waypoints. Unlabelled on purpose: they give the ring structure
 *  without turning the drawing into a diagram that has to be read. */
const MARKERS = [-90, 0, 90, 180];

function polar(radius: number, deg: number) {
  const r = (deg * Math.PI) / 180;
  return { x: C + radius * Math.cos(r), y: C + radius * Math.sin(r) };
}

/** Clockwise arc between two angles on the ring. */
function arc(radius: number, fromDeg: number, toDeg: number) {
  const a = polar(radius, fromDeg);
  const b = polar(radius, toDeg);
  return `M ${a.x} ${a.y} A ${radius} ${radius} 0 0 1 ${b.x} ${b.y}`;
}

export function LoopVisual({ className }: { className?: string }) {
  return (
    <svg
      viewBox="0 0 320 320"
      className={cn("overflow-visible", className)}
      role="img"
      aria-label="One repository at the centre of a loop, with three sessions — yours and two agents' — travelling round it at their own speeds, past the points the loop passes through."
    >
      <circle cx={C} cy={C} r={R} fill="none" stroke="var(--border)" strokeWidth="1" />

      {MARKERS.map((deg) => {
        const inner = polar(R - 4, deg);
        const outer = polar(R + 4, deg);
        return (
          <line
            key={deg}
            x1={inner.x}
            y1={inner.y}
            x2={outer.x}
            y2={outer.y}
            stroke="var(--border)"
            strokeWidth="1"
          />
        );
      })}

      {SESSIONS.map(({ offset, duration, trail, dot, opacity }, i) => {
        const head = polar(R, -90);
        return (
          <g key={i} transform={`rotate(${offset} ${C} ${C})`} opacity={opacity}>
            <g className="kl-orbit" style={{ ["--kl-duration" as string]: duration }}>
              <path
                d={arc(R, -90 - trail, -90)}
                fill="none"
                stroke="var(--primary)"
                strokeWidth={dot * 0.9}
                strokeLinecap="round"
                opacity="0.18"
              />
              <circle cx={head.x} cy={head.y} r={dot} fill="var(--primary)" />
            </g>
          </g>
        );
      })}

      <rect x={C - 6} y={C - 6} width="12" height="12" fill="var(--muted-foreground)" />
    </svg>
  );
}
