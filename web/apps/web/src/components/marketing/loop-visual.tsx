/** The claim, drawn. One repo at the centre; around it, several sessions running
 *  at once — yours and your agents' — each forked into its own workspace and each
 *  turning its own tight loop, at its own speed. The faint outer ring is the loop
 *  those changes used to take: commit, build, deploy, wait. Same centre, one lap
 *  of the outer ring against many laps of the inner ones.
 *
 *  Pure SVG + CSS. Rotating dashed arcs read as travelling comets with no frame
 *  loop and no JS, and they stop under prefers-reduced-motion (globals.css). */
const C = 160;
const OUTER = 116;
const STAGES = [
  { label: "commit", angle: -90 },
  { label: "build", angle: 0 },
  { label: "deploy", angle: 90 },
  { label: "wait", angle: 180 },
];

/** Staggered durations so the sessions visibly run independently rather than
 *  looking like one rigid mechanism. */
const SESSIONS = [
  { label: "you", angle: -90, duration: "2.2s" },
  { label: "agent", angle: 30, duration: "1.5s" },
  { label: "agent", angle: 150, duration: "1.8s" },
];

const ORBIT = 52;
const RING = 27;

function pointOn(radius: number, angleDeg: number) {
  const r = (angleDeg * Math.PI) / 180;
  return { x: C + radius * Math.cos(r), y: C + radius * Math.sin(r) };
}

export function LoopVisual({ className }: { className?: string }) {
  const outerLen = 2 * Math.PI * OUTER;
  const ringLen = 2 * Math.PI * RING;

  return (
    <svg
      viewBox="0 0 320 320"
      className={className}
      role="img"
      aria-label="One repository at the centre with three sessions around it — one yours, two agents' — each running its own fast loop in its own workspace, inside the much slower commit, build, deploy, wait loop."
    >
      {/* the loop this replaces */}
      <circle cx={C} cy={C} r={OUTER} fill="none" stroke="var(--border)" strokeWidth="1" strokeDasharray="3 4" />
      <circle
        cx={C}
        cy={C}
        r={OUTER}
        fill="none"
        stroke="var(--muted-foreground)"
        strokeWidth="1.5"
        strokeDasharray={`${outerLen * 0.09} ${outerLen}`}
        className="kl-orbit"
        style={{ ["--kl-duration" as string]: "11s", opacity: 0.55 }}
      />
      {STAGES.map(({ label, angle }) => {
        const n = pointOn(OUTER, angle);
        const t = pointOn(OUTER + 19, angle);
        return (
          <g key={label}>
            <rect x={n.x - 2.5} y={n.y - 2.5} width="5" height="5" fill="var(--background)" stroke="var(--muted-foreground)" strokeWidth="1" />
            <text
              x={t.x}
              y={t.y}
              textAnchor="middle"
              dominantBaseline="middle"
              fill="var(--muted-foreground)"
              style={{ fontSize: "9.5px", fontWeight: 600, letterSpacing: "0.06em" }}
            >
              {label}
            </text>
          </g>
        );
      })}

      {/* each session is tethered to the same repo */}
      {SESSIONS.map(({ label, angle }) => {
        const p = pointOn(ORBIT, angle);
        return (
          <line
            key={`${label}-${angle}`}
            x1={C}
            y1={C}
            x2={p.x}
            y2={p.y}
            stroke="var(--border)"
            strokeWidth="1"
          />
        );
      })}

      {SESSIONS.map(({ label, angle, duration }) => {
        const p = pointOn(ORBIT, angle);
        const t = pointOn(ORBIT + RING + 11, angle);
        const isYou = label === "you";
        return (
          <g key={`${label}-${angle}`}>
            <circle cx={p.x} cy={p.y} r={RING} fill="var(--background)" stroke="var(--border)" strokeWidth="1" />
            <circle
              cx={p.x}
              cy={p.y}
              r={RING}
              fill="none"
              stroke="var(--primary)"
              strokeWidth="2.5"
              strokeDasharray={`${ringLen * 0.3} ${ringLen}`}
              className="kl-orbit"
              style={{ ["--kl-duration" as string]: duration, opacity: isYou ? 1 : 0.75 }}
            />
            <text
              x={p.x}
              y={p.y}
              textAnchor="middle"
              dominantBaseline="middle"
              fill={isYou ? "var(--foreground)" : "var(--muted-foreground)"}
              style={{ fontSize: "10px", fontWeight: 700 }}
            >
              {label}
            </text>
            <text
              x={t.x}
              y={t.y}
              textAnchor="middle"
              dominantBaseline="middle"
              fill="var(--muted-foreground)"
              style={{ fontSize: "9px", fontWeight: 600, letterSpacing: "0.05em" }}
            >
              session
            </text>
          </g>
        );
      })}

      {/* the one repo they all fork from */}
      <rect x={C - 7} y={C - 7} width="14" height="14" fill="var(--primary)" />
      <circle cx={C} cy={C} r="13" fill="none" stroke="var(--primary)" strokeWidth="1" className="kl-pulse" />
    </svg>
  );
}
