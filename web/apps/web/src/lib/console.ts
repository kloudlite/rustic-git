/** The five tones every pill, bar and feed row in the superadmin place is painted with. Kept as
 *  one union rather than per-component variants so a "critical" capacity bar and a "critical"
 *  signal pill can never end up different colours on the same screen. */
export type Tone = "ok" | "warn" | "critical" | "info" | "neutral";

/** Whole-percent, clamped. A limit lowered under a running fleet leaves usage above it, and a bar
 *  that overflows its own track reads as a rendering bug rather than as being over the wall. */
export function pct(used: number, limit: number): number {
  if (!Number.isFinite(used) || !Number.isFinite(limit) || limit <= 0) return 0;
  return Math.min(100, Math.round((used / limit) * 100));
}

/** Amber at 80 %, red only at the wall (design README). "Critical" means the next create is
 *  refused — not "nearly" — so the colour keeps meaning something. */
export function capacityTone(used: number, limit: number): Tone {
  if (limit > 0 && used >= limit) return "critical";
  return pct(used, limit) >= 80 ? "warn" : "ok";
}

/** An inline-SVG polyline over the box, no chart library: a KPI sparkline is seven to ninety
 *  points with no axes, no legend and no interaction. */
export function sparkPath(values: number[], w: number, h: number): string {
  if (values.length === 0) return "";
  if (values.length === 1) return `M0,${h / 2} L${w},${h / 2}`;
  const min = Math.min(...values);
  const max = Math.max(...values);
  // A constant series has no range; centre it rather than dividing by zero.
  const span = max - min;
  const step = w / (values.length - 1);
  return values
    .map((v, i) => {
      const y = span === 0 ? h / 2 : h - ((v - min) / span) * h;
      const x = Math.round(i * step * 100) / 100;
      return `${i === 0 ? "M" : "L"}${x},${Math.round(y * 100) / 100}`;
    })
    .join(" ");
}

/** Avatar letters. An email is one initial, not "K@" — the local part before any punctuation. */
export function initials(name: string): string {
  const words = name.split("@")[0].split(/[\s._-]+/).filter(Boolean);
  if (words.length === 0) return "?";
  return words
    .slice(0, 2)
    .map((w) => w[0].toUpperCase())
    .join("");
}
