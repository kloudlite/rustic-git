/** One repo, one loop, and the sessions going round it — yours and your agents'.
 *  Each dot moves at its own speed, which is the whole idea: the work is
 *  concurrent, and a lap is short. Nothing else is drawn.
 *
 *  Pure SVG + CSS; the dots stop under prefers-reduced-motion (globals.css). */
const C = 160;
const R = 92;

const SESSIONS = [
  { label: "you", duration: "3.4s", delay: "0s", size: 6, fill: "var(--primary)" },
  { label: "agent", duration: "2.6s", delay: "-0.9s", size: 5, fill: "var(--primary)" },
  { label: "agent", duration: "4.2s", delay: "-2.1s", size: 5, fill: "var(--primary)" },
];

export function LoopVisual({ className }: { className?: string }) {
  return (
    <svg
      viewBox="0 0 320 320"
      className={className}
      role="img"
      aria-label="One repository at the centre of a single loop, with three sessions — yours and two agents' — moving round it at their own speeds."
    >
      <circle cx={C} cy={C} r={R} fill="none" stroke="var(--border)" strokeWidth="1" />

      {SESSIONS.map(({ label, duration, delay, size, fill }, i) => (
        <g
          key={`${label}-${i}`}
          className="kl-orbit"
          style={{ ["--kl-duration" as string]: duration, animationDelay: delay }}
        >
          <circle cx={C} cy={C - R} r={size} fill={fill} />
        </g>
      ))}

      <rect x={C - 6} y={C - 6} width="12" height="12" fill="var(--foreground)" />
    </svg>
  );
}
