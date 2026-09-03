/** A snapshot's frozen definition — what the thing WAS when the push was taken, carried on every
 *  history row as `state` (`null` for snapshots pushed before the field existed).
 *
 *  Pure and in `lib` so the one-line summary is testable without rendering, like `ws-status.ts`:
 *  the rows render `stateSummary` and nothing decides those words twice. */
export type SnapshotState =
  | {
      kind: "workspace";
      image: string;
      packages: string[];
      quotaGb: number;
      attachedEnvironment?: string | null;
      resources: { cpu?: string; memory?: string };
    }
  | {
      kind: "environment";
      services: { name: string; image: string }[];
      quotaGb: number;
    };

const plural = (n: number, word: string) => `${n} ${word}${n === 1 ? "" : "s"}`;

/** One line for a snapshot row. `""` when the snapshot predates the field — the row omits it
 *  rather than saying "unknown", which would read as a fact about the snapshot. */
export function stateSummary(s: SnapshotState | null | undefined): string {
  if (!s) return "";
  if (s.kind === "environment") return plural(s.services.length, "service");
  return s.packages.length === 0 ? s.image : `${s.image} · ${plural(s.packages.length, "package")}`;
}
