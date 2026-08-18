/** The claim, drawn: one change travelling two loops. The outer one is the usual
 *  round trip — commit, build, publish, deploy, wait — and the inner one is what
 *  is left when those stop being separate systems. Same centre, same direction,
 *  an order of magnitude apart in how long a lap takes.
 *
 *  Pure SVG + CSS. A rotating dashed arc reads as a travelling comet with no
 *  frame loop and no JS, and it stops under prefers-reduced-motion. */
const STAGES = [
  { label: "commit", angle: -90 },
  { label: "build", angle: -18 },
  { label: "publish", angle: 54 },
  { label: "deploy", angle: 126 },
  { label: "wait", angle: 198 },
];

const OUTER = 104;
const INNER = 50;
const C = 160;

function pointOn(radius: number, angleDeg: number) {
  const r = (angleDeg * Math.PI) / 180;
  return { x: C + radius * Math.cos(r), y: C + radius * Math.sin(r) };
}

export function LoopVisual({ className }: { className?: string }) {
  const outerLen = 2 * Math.PI * OUTER;
  const innerLen = 2 * Math.PI * INNER;

  return (
    <svg
      viewBox="0 0 320 320"
      className={className}
      role="img"
      aria-label="Two concentric loops: the usual commit, build, publish, deploy round trip on the outside, and a much shorter edit-to-live loop at the centre."
    >
      <circle cx={C} cy={C} r={OUTER} fill="none" stroke="var(--border)" strokeWidth="1" />
      <circle
        cx={C}
        cy={C}
        r={OUTER}
        fill="none"
        stroke="var(--muted-foreground)"
        strokeWidth="1.5"
        strokeDasharray={`${outerLen * 0.1} ${outerLen}`}
        className="kl-orbit"
        style={{ ["--kl-duration" as string]: "9s", opacity: 0.65 }}
      />

      {STAGES.map(({ label, angle }) => {
        const n = pointOn(OUTER, angle);
        const t = pointOn(OUTER + 21, angle);
        return (
          <g key={label}>
            <rect
              x={n.x - 3}
              y={n.y - 3}
              width="6"
              height="6"
              fill="var(--background)"
              stroke="var(--muted-foreground)"
              strokeWidth="1"
            />
            <text
              x={t.x}
              y={t.y}
              textAnchor="middle"
              dominantBaseline="middle"
              fill="var(--muted-foreground)"
              style={{ fontSize: "10px", fontWeight: 600, letterSpacing: "0.06em" }}
            >
              {label}
            </text>
          </g>
        );
      })}

      <circle cx={C} cy={C} r={INNER} fill="none" stroke="var(--border)" strokeWidth="1" />
      <circle
        cx={C}
        cy={C}
        r={INNER}
        fill="none"
        stroke="var(--primary)"
        strokeWidth="2.5"
        strokeDasharray={`${innerLen * 0.28} ${innerLen}`}
        className="kl-orbit"
        style={{ ["--kl-duration" as string]: "1.8s" }}
      />

      <text x={C} y={C - 7} textAnchor="middle" fill="var(--foreground)" style={{ fontSize: "13px", fontWeight: 700 }}>
        edit
      </text>
      <text x={C} y={C + 11} textAnchor="middle" fill="var(--primary)" style={{ fontSize: "13px", fontWeight: 700 }}>
        live
      </text>
      <circle cx={C} cy={C + 24} r="2.5" fill="var(--primary)" className="kl-pulse" />
    </svg>
  );
}
